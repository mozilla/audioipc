#[macro_use]
extern crate error_chain;

#[macro_use]
extern crate log;

extern crate audioipc;
extern crate bytes;
extern crate cubeb;
extern crate cubeb_core;
extern crate lazycell;
extern crate mio;
extern crate mio_uds;
extern crate slab;

use audioipc::AutoCloseFd;
use audioipc::async::{Async, AsyncRecvFd, AsyncSendFd};
use audioipc::codec::{Decoder, encode};
use audioipc::messages::{ClientMessage, DeviceInfo, ServerMessage, StreamInitParams, StreamParams};
use audioipc::shm::{SharedMemReader, SharedMemWriter};
use bytes::{Bytes, BytesMut, IntoBuf};
use cubeb_core::binding::Binding;
use cubeb_core::ffi;
use mio::{Ready, Token};
use mio_uds::{UnixListener, UnixStream};
use std::{slice, thread};
use std::collections::VecDeque;
use std::convert::From;
use std::os::raw::c_void;
use std::os::unix::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

mod channel;

pub mod errors {
    error_chain! {
        links {
            AudioIPC(::audioipc::errors::Error, ::audioipc::errors::ErrorKind);
        }
        foreign_links {
            Cubeb(::cubeb_core::Error);
            Io(::std::io::Error);
        }
    }
}

use errors::*;

// TODO: Remove and let caller allocate based on cubeb backend requirements.
const SHM_AREA_SIZE: usize = 2 * 1024 * 1024;

// TODO: this should forward to the client.
struct Callback {
    /// Size of input frame in bytes
    input_frame_size: u16,
    /// Size of output frame in bytes
    output_frame_size: u16,
    connection: audioipc::Connection,
    input_shm: SharedMemWriter,
    output_shm: SharedMemReader
}

impl cubeb::StreamCallback for Callback {
    type Frame = u8;

    fn data_callback(&mut self, input: &[u8], output: &mut [u8]) -> isize {
        info!("Stream data callback: {} {}", input.len(), output.len());

        // len is of input and output is frame len. Turn these into the real lengths.
        let real_input = unsafe {
            let size_bytes = input.len() * self.input_frame_size as usize;
            slice::from_raw_parts(input.as_ptr(), size_bytes)
        };
        let real_output = unsafe {
            let size_bytes = output.len() * self.output_frame_size as usize;
            info!("Resize output to {}", size_bytes);
            slice::from_raw_parts_mut(output.as_mut_ptr(), size_bytes)
        };

        self.input_shm.write(&real_input).unwrap();

        self.connection
            .send(ClientMessage::StreamDataCallback(
                output.len() as isize,
                self.output_frame_size as usize
            ))
            .unwrap();

        let r = self.connection.receive();
        match r {
            Ok(ServerMessage::StreamDataCallback(cb_result)) => {
                if cb_result >= 0 {
                    let len = cb_result as usize * self.output_frame_size as usize;
                    self.output_shm.read(&mut real_output[..len]).unwrap();
                    cb_result
                } else {
                    cb_result
                }
            },
            _ => {
                debug!("Unexpected message {:?} during callback", r);
                -1
            },
        }
    }

    fn state_callback(&mut self, state: cubeb::State) {
        info!("Stream state callback: {:?}", state);
        // TODO: Share this conversion with the same code in cubeb-rs?
        let state = match state {
            cubeb::State::Started => ffi::CUBEB_STATE_STARTED,
            cubeb::State::Stopped => ffi::CUBEB_STATE_STOPPED,
            cubeb::State::Drained => ffi::CUBEB_STATE_DRAINED,
            cubeb::State::Error => ffi::CUBEB_STATE_ERROR,
        };
        self.connection.send(ClientMessage::StreamStateCallback(state)).unwrap();
    }
}

impl Drop for Callback {
    fn drop(&mut self) {
        self.connection
            .send(ClientMessage::StreamDestroyed)
            .unwrap();
    }
}

type Slab<T> = slab::Slab<T, Token>;
type StreamSlab = slab::Slab<cubeb::Stream<Callback>, usize>;

// TODO: Server token must be outside range used by server.connections slab.
// usize::MAX is already used internally in mio.
const QUIT: Token = Token(std::usize::MAX - 2);
const SERVER: Token = Token(std::usize::MAX - 1);

struct ServerConn {
    //connection: audioipc::Connection,
    io: UnixStream,
    token: Option<Token>,
    streams: StreamSlab,
    decoder: Decoder,
    recv_buffer: BytesMut,
    send_buffer: BytesMut,
    pending_send: VecDeque<(Bytes, Option<AutoCloseFd>)>
}

impl ServerConn {
    fn new(io: UnixStream) -> ServerConn {
        ServerConn {
            io: io,
            token: None,
            // TODO: Handle increasing slab size. Pick a good default size.
            streams: StreamSlab::with_capacity(64),
            decoder: Decoder::new(),
            recv_buffer: BytesMut::with_capacity(4096),
            send_buffer: BytesMut::with_capacity(4096),
            pending_send: VecDeque::new()
        }
    }

    fn process_read(&mut self, context: &Result<cubeb::Context>) -> Result<Ready> {
        // According to *something*, processing non-blocking stream
        // should attempt to read until EWOULDBLOCK is returned.
        while let Async::Ready((n, fd)) = try!(self.io.recv_buf_fd(&mut self.recv_buffer)) {
            trace!("Received {} bytes and fd {:?}", n, fd);

            // Reading 0 signifies EOF
            if n == 0 {
                return Err(
                    ::errors::ErrorKind::AudioIPC(::audioipc::errors::ErrorKind::Disconnected).into()
                );
            }

            if let Some(fd) = fd {
                trace!("Unexpectedly received an fd from client.");
                let _ = unsafe { AutoCloseFd::from_raw_fd(fd) };
            }

            // Process all the complete messages contained in
            // send.recv_buffer.  It's possible that a read might not
            // return a complete message, so self.decoder.decode
            // returns Ok(None).
            loop {
                match self.decoder.decode::<ServerMessage>(&mut self.recv_buffer) {
                    Ok(Some(msg)) => {
                        info!("ServerConn::process: got {:?}", msg);
                        try!(self.process_msg(&msg, context));
                    },
                    Ok(None) => {
                        break;
                    },
                    Err(e) => {
                        return Err(e).chain_err(|| "Failed to decoder ServerMessage");
                    },
                }
            }
        }

        // Send any pending responses to client.
        self.flush_pending_send()
    }

    // Process a request coming from the client.
    fn process_msg(&mut self, msg: &ServerMessage, context: &Result<cubeb::Context>) -> Result<()> {
        let resp: ClientMessage = if let &Ok(ref context) = context {
            if let &ServerMessage::StreamInit(ref params) = msg {
                return self.process_stream_init(context, params);
            };

            match msg {
                &ServerMessage::ClientConnect => {
                    panic!("already connected");
                },

                &ServerMessage::ClientDisconnect => {
                    // TODO:
                    //self.connection.client_disconnect();
                    ClientMessage::ClientDisconnected
                },

                &ServerMessage::ContextGetBackendId => ClientMessage::ContextBackendId(),

                &ServerMessage::ContextGetMaxChannelCount => {
                    context
                        .max_channel_count()
                        .map(ClientMessage::ContextMaxChannelCount)
                        .unwrap_or_else(|e| error(e))
                },

                &ServerMessage::ContextGetMinLatency(ref params) => {
                    let format = cubeb::SampleFormat::from(params.format);
                    let layout = cubeb::ChannelLayout::from(params.layout);

                    let params = cubeb::StreamParamsBuilder::new()
                        .format(format)
                        .rate(params.rate as _)
                        .channels(params.channels as _)
                        .layout(layout)
                        .take();

                    context
                        .min_latency(&params)
                        .map(ClientMessage::ContextMinLatency)
                        .unwrap_or_else(|e| error(e))
                },

                &ServerMessage::ContextGetPreferredSampleRate => {
                    context
                        .preferred_sample_rate()
                        .map(ClientMessage::ContextPreferredSampleRate)
                        .unwrap_or_else(|e| error(e))
                },

                &ServerMessage::ContextGetPreferredChannelLayout => {
                    context
                        .preferred_channel_layout()
                        .map(|l| ClientMessage::ContextPreferredChannelLayout(l as _))
                        .unwrap_or_else(|e| error(e))
                },

                &ServerMessage::ContextGetDeviceEnumeration(device_type) => {
                    context
                        .enumerate_devices(cubeb::DeviceType::from_bits_truncate(device_type))
                        .map(|devices| {
                            let v: Vec<DeviceInfo> = devices.iter().map(|i| i.raw().into()).collect();
                            ClientMessage::ContextEnumeratedDevices(v)
                        })
                        .unwrap_or_else(|e| error(e))
                },

                &ServerMessage::StreamInit(_) => {
                    panic!("StreamInit should have already been handled.");
                },

                &ServerMessage::StreamDestroy(stm_tok) => {
                    self.streams.remove(stm_tok);
                    ClientMessage::StreamDestroyed
                },

                &ServerMessage::StreamStart(stm_tok) => {
                    let _ = self.streams[stm_tok].start();
                    ClientMessage::StreamStarted
                },

                &ServerMessage::StreamStop(stm_tok) => {
                    let _ = self.streams[stm_tok].stop();
                    ClientMessage::StreamStopped
                },

                &ServerMessage::StreamGetPosition(stm_tok) => {
                    self.streams[stm_tok]
                        .position()
                        .map(ClientMessage::StreamPosition)
                        .unwrap_or_else(|e| error(e))
                },
                &ServerMessage::StreamGetLatency(stm_tok) => {
                    self.streams[stm_tok]
                        .latency()
                        .map(ClientMessage::StreamLatency)
                        .unwrap_or_else(|e| error(e))
                },
                &ServerMessage::StreamSetVolume(stm_tok, volume) => {
                    self.streams[stm_tok]
                        .set_volume(volume)
                        .map(|_| ClientMessage::StreamVolumeSet)
                        .unwrap_or_else(|e| error(e))
                },
                &ServerMessage::StreamSetPanning(stm_tok, panning) => {
                    self.streams[stm_tok]
                        .set_panning(panning)
                        .map(|_| ClientMessage::StreamPanningSet)
                        .unwrap_or_else(|e| error(e))
                },
                &ServerMessage::StreamGetCurrentDevice(stm_tok) => {
                    self.streams[stm_tok]
                        .current_device()
                        .map(|device| ClientMessage::StreamCurrentDevice(device.into()))
                        .unwrap_or_else(|e| error(e))
                },
                _ => {
                    bail!("Unexpected Message");
                },
            }
        } else {
            error(cubeb::Error::new())
        };

        debug!("process_msg: req={:?}, resp={:?}", msg, resp);

        self.queue_message(resp)
    }

    // Stream init is special, so it's been separated from process_msg.
    fn process_stream_init(&mut self, context: &cubeb::Context, params: &StreamInitParams) -> Result<()> {
        fn opt_stream_params(params: Option<&StreamParams>) -> Option<cubeb::StreamParams> {
            params.and_then(|p| {
                let raw = ffi::cubeb_stream_params::from(p);
                Some(unsafe { cubeb::StreamParams::from_raw(&raw as *const _) })
            })
        }

        fn frame_size_in_bytes(params: Option<cubeb::StreamParams>) -> u16 {
            params.map(|p| {
                let sample_size = match p.format() {
                    cubeb::SampleFormat::S16LE |
                    cubeb::SampleFormat::S16BE |
                    cubeb::SampleFormat::S16NE => 2,
                    cubeb::SampleFormat::Float32LE |
                    cubeb::SampleFormat::Float32BE |
                    cubeb::SampleFormat::Float32NE => 4,
                };
                let channel_count = p.channels() as u16;
                sample_size * channel_count
            }).unwrap_or(0u16)
        }


        // TODO: Yuck!
        let input_device = unsafe { cubeb::DeviceId::from_raw(params.input_device as *const _) };
        let output_device = unsafe { cubeb::DeviceId::from_raw(params.output_device as *const _) };
        let latency = params.latency_frames;
        let mut builder = cubeb::StreamInitOptionsBuilder::new();
        builder
            .input_device(input_device)
            .output_device(output_device)
            .latency(latency);

        if let Some(ref stream_name) = params.stream_name {
            builder.stream_name(stream_name);
        }
        let input_stream_params = opt_stream_params(params.input_stream_params.as_ref());
        if let Some(ref isp) = input_stream_params {
            builder.input_stream_param(isp);
        }
        let output_stream_params = opt_stream_params(params.output_stream_params.as_ref());
        if let Some(ref osp) = output_stream_params {
            builder.output_stream_param(osp);
        }
        let params = builder.take();

        let input_frame_size = frame_size_in_bytes(input_stream_params);
        let output_frame_size = frame_size_in_bytes(output_stream_params);

        let (conn1, conn2) = audioipc::Connection::pair()?;
        info!("Created connection pair: {:?}-{:?}", conn1, conn2);

        let (input_shm, input_file) = SharedMemWriter::new(&audioipc::get_shm_path("input"), SHM_AREA_SIZE)?;
        let (output_shm, output_file) = SharedMemReader::new(&audioipc::get_shm_path("output"), SHM_AREA_SIZE)?;

        let err = match context.stream_init(
            &params,
            Callback {
                input_frame_size: input_frame_size,
                output_frame_size: output_frame_size,
                connection: conn2,
                input_shm: input_shm,
                output_shm: output_shm
            }
        ) {
            Ok(stream) => {
                let stm_tok = match self.streams.vacant_entry() {
                    Some(entry) => {
                        debug!(
                            "Registering stream {:?}",
                            entry.index(),
                        );

                        entry.insert(stream).index()
                    },
                    None => {
                        // TODO: Turn into error
                        panic!("Failed to insert stream into slab. No entries");
                    },
                };

                let _ = try!(self.queue_init_messages(
                    stm_tok,
                    conn1,
                    input_file,
                    output_file
                ));
                None
            },
            Err(e) => Some(error(e)),
        };

        if let Some(err) = err {
            try!(self.queue_message(err))
        }

        Ok(())
    }

    fn queue_init_messages<T, U, V>(&mut self, stm_tok: usize, conn: T, input_file: U, output_file: V) -> Result<()>
    where
        T: IntoRawFd,
        U: IntoRawFd,
        V: IntoRawFd,
    {
        let _ = try!(self.queue_message_fd(
            ClientMessage::StreamCreated(stm_tok),
            conn
        ));
        let _ = try!(self.queue_message_fd(
            ClientMessage::StreamCreatedInputShm,
            input_file
        ));
        let _ = try!(self.queue_message_fd(
            ClientMessage::StreamCreatedOutputShm,
            output_file
        ));
        Ok(())
    }

    fn queue_message(&mut self, msg: ClientMessage) -> Result<()> {
        debug!("queue_message: {:?}", msg);
        encode::<ClientMessage>(&mut self.send_buffer, &msg).or_else(|e| {
            Err(e).chain_err(|| "Failed to encode msg into send buffer")
        })
    }

    // Since send_fd supports sending one RawFd at a time, queuing a
    // message with a RawFd forces use to take the current send_buffer
    // and move it pending queue.
    fn queue_message_fd<FD: IntoRawFd>(&mut self, msg: ClientMessage, fd: FD) -> Result<()> {
        let fd = fd.into_raw_fd();
        debug!("queue_message_fd: {:?} {:?}", msg, fd);
        let _ = try!(self.queue_message(msg));
        self.take_pending_send(Some(fd));
        Ok(())
    }

    // Take the current messages in the send_buffer and move them to
    // pending queue.
    fn take_pending_send(&mut self, fd: Option<RawFd>) {
        let pending = self.send_buffer.take().freeze();
        debug!("take_pending_send: ({:?} {:?})", pending, fd);
        self.pending_send.push_back((
            pending,
            fd.map(|fd| unsafe { AutoCloseFd::from_raw_fd(fd) })
        ));
    }

    // Process the pending queue and send them to client.
    fn flush_pending_send(&mut self) -> Result<Ready> {
        debug!("flush_pending_send");
        // take any pending messages in the send buffer.
        if self.send_buffer.len() > 0 {
            self.take_pending_send(None);
        }

        trace!("pending queue: {:?}", self.pending_send);

        let mut result = Ready::readable();
        let mut processed = 0;

        for &mut (ref buf, ref fd) in self.pending_send.iter_mut() {
            let mut src = buf.into_buf();
            let fd = match fd {
                &Some(ref fd) => Some(fd.as_raw_fd()),
                &None => None,
            };
            let r = try!(self.io.send_buf_fd(&mut src, fd));
            if r.is_ready() {
                processed += 1;
            } else {
                result.insert(Ready::writable());
                break;
            }
        }

        debug!("processed {} buffers", processed);

        self.pending_send = self.pending_send.split_off(processed);

        trace!("pending queue: {:?}", self.pending_send);

        Ok(result)
    }
}

pub struct Server {
    socket: UnixListener,
    // Ok(None)      - Server hasn't tried to create cubeb::Context.
    // Ok(Some(ctx)) - Server has successfully created cubeb::Context.
    // Err(_)        - Server has tried and failed to create cubeb::Context.
    //                 Don't try again.
    context: Option<Result<cubeb::Context>>,
    conns: Slab<ServerConn>
}

impl Server {
    pub fn new(socket: UnixListener) -> Server {
        Server {
            socket: socket,
            context: None,
            conns: Slab::with_capacity(16)
        }
    }

    fn accept(&mut self, poll: &mut mio::Poll) -> Result<()> {
        debug!("Server accepting connection");

        let client_socket = match self.socket.accept() {
            Err(e) => {
                error!("server accept error: {}", e);
                return Err(e.into());
            },
            Ok(None) => panic!("accept returned EAGAIN unexpectedly"),
            Ok(Some((socket, _))) => socket,
        };
        let token = match self.conns.vacant_entry() {
            Some(entry) => {
                debug!("registering {:?}", entry.index());
                let cxn = ServerConn::new(client_socket);
                entry.insert(cxn).index()
            },
            None => {
                panic!("failed to insert connection");
            },
        };

        // Register the connection
        self.conns[token].token = Some(token);
        poll.register(
            &self.conns[token].io,
            token,
            mio::Ready::readable(),
            mio::PollOpt::edge() | mio::PollOpt::oneshot()
        ).unwrap();
        /*
        let r = self.conns[token].receive();
        debug!("received {:?}", r);
        let r = self.conns[token].send(ClientMessage::ClientConnected);
        debug!("sent {:?} (ClientConnected)", r);
         */

        // Since we have a connection try creating a cubeb context. If
        // it fails, mark the failure with Err.
        if self.context.is_none() {
            self.context = Some(cubeb::Context::init("AudioIPC Server", None).or_else(|e| {
                Err(e).chain_err(|| "Unable to create cubeb context.")
            }))
        }

        Ok(())
    }

    pub fn poll(&mut self, poll: &mut mio::Poll) -> Result<()> {
        let mut events = mio::Events::with_capacity(16);

        match poll.poll(&mut events, None) {
            Ok(_) => {},
            Err(e) => error!("server poll error: {}", e),
        }

        for event in events.iter() {
            match event.token() {
                SERVER => {
                    match self.accept(poll) {
                        Err(e) => {
                            error!("server accept error: {}", e);
                        },
                        _ => {},
                    };
                },
                QUIT => {
                    info!("Quitting Audio Server loop");
                    bail!("quit");
                },
                token => {
                    debug!("token {:?} ready", token);

                    let context = self.context.as_ref().expect(
                        "Shouldn't receive a message before accepting connection."
                    );

                    let mut readiness = Ready::readable();

                    if event.readiness().is_readable() {
                        let r = self.conns[token].process_read(context);
                        debug!("got {:?}", r);

                        if let Err(e) = r {
                            debug!("dropped client {:?} due to error {:?}", token, e);
                            self.conns.remove(token);
                            continue;
                        }
                    };

                    if event.readiness().is_writable() {
                        let r = self.conns[token].flush_pending_send();
                        debug!("got {:?}", r);

                        match r {
                            Ok(r) => readiness = r,
                            Err(e) => {
                                debug!("dropped client {:?} due to error {:?}", token, e);
                                self.conns.remove(token);
                                continue;
                            },
                        }
                    };

                    poll.reregister(
                        &self.conns[token].io,
                        token,
                        readiness,
                        mio::PollOpt::edge() | mio::PollOpt::oneshot()
                    ).unwrap();
                },
            }
        }

        Ok(())
    }
}


// TODO: This should take an "Evented" instead of opening the UDS path
// directly (and let caller set up the Evented), but need a way to describe
// it as an Evented that we can send/recv file descriptors (or HANDLEs on
// Windows) over.
pub fn run(running: Arc<AtomicBool>) -> Result<()> {

    // Ignore result.
    let _ = std::fs::remove_file(audioipc::get_uds_path());

    // TODO: Use a SEQPACKET, wrap it in UnixStream?
    let mut poll = mio::Poll::new()?;
    let mut server = Server::new(UnixListener::bind(audioipc::get_uds_path())?);

    poll.register(
        &server.socket,
        SERVER,
        mio::Ready::readable(),
        mio::PollOpt::edge()
    ).unwrap();

    loop {
        if !running.load(Ordering::SeqCst) {
            bail!("server quit due to ctrl-c");
        }

        let _ = try!(server.poll(&mut poll));
    }

    //poll.deregister(&server.socket).unwrap();
}

#[no_mangle]
pub extern "C" fn audioipc_server_start() -> *mut c_void {

    let (tx, rx) = channel::ctl_pair();

    thread::spawn(move || {
        // Ignore result.
        let _ = std::fs::remove_file(audioipc::get_uds_path());

        // TODO: Use a SEQPACKET, wrap it in UnixStream?
        let mut poll = mio::Poll::new().unwrap();
        let mut server = Server::new(UnixListener::bind(audioipc::get_uds_path()).unwrap());

        poll.register(
            &server.socket,
            SERVER,
            mio::Ready::readable(),
            mio::PollOpt::edge()
        ).unwrap();

        poll.register(&rx, QUIT, mio::Ready::readable(), mio::PollOpt::edge())
            .unwrap();

        loop {
            match server.poll(&mut poll) {
                Err(_) => {
                    return;
                },
                _ => (),
            }
        }
    });

    Box::into_raw(Box::new(tx)) as *mut _
}

#[no_mangle]
pub extern "C" fn audioipc_server_stop(p: *mut c_void) {
    // Dropping SenderCtl here will notify the other end.
    let _ = unsafe { Box::<channel::SenderCtl>::from_raw(p as *mut _) };
}

fn error(error: cubeb::Error) -> ClientMessage {
    ClientMessage::ContextError(error.raw_code())
}
