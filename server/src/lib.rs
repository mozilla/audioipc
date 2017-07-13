#[macro_use]
extern crate error_chain;

#[macro_use]
extern crate log;

extern crate audioipc;
extern crate cubeb;
extern crate mio;
extern crate mio_uds;
extern crate slab;

use audioipc::messages::{ClientMessage, ServerMessage};
use mio::Token;
use mio_uds::UnixListener;
use std::convert::From;
use std::os::unix::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub mod errors {
    error_chain! {
        links {
            AudioIPC(::audioipc::errors::Error, ::audioipc::errors::ErrorKind);
        }
        foreign_links {
            Io(::std::io::Error);
        }
    }
}

use errors::*;

type Slab<T> = slab::Slab<T, Token>;

// TODO: Server token must be outside range used by server.connections slab.
// usize::MAX is already used internally in mio.
const SERVER: Token = Token(std::usize::MAX - 1);

struct ServerConn {
    connection: audioipc::Connection,
    token: Option<Token>
}

impl ServerConn {
    fn new<FD>(fd: FD) -> ServerConn
    where
        FD: IntoRawFd,
    {
        ServerConn {
            connection: unsafe { audioipc::Connection::from_raw_fd(fd.into_raw_fd()) },
            token: None
        }
    }

    fn process(&mut self, poll: &mut mio::Poll, context: &mut cubeb::Context) -> Result<()> {
        let r = self.connection.receive();
        debug!("got {:?}", r);

        // TODO: Might need a simple state machine to deal with create/use/destroy ordering, etc.
        // TODO: receive() and all this handling should be moved out of this event loop code.
        let msg = r.unwrap();
        let _ = try!(self.process_msg(&msg, context));

        poll.reregister(
            &self.connection,
            self.token.unwrap(),
            mio::Ready::readable(),
            mio::PollOpt::edge() | mio::PollOpt::oneshot()
        ).unwrap();

        Ok(())
    }

    fn process_msg(&mut self, msg: &ServerMessage, context: &mut cubeb::Context) -> Result<()> {
        match msg {
            &ServerMessage::ClientConnect => {
                panic!("already connected");
            },
            &ServerMessage::ClientDisconnect => {
                // TODO:
                //self.connection.client_disconnect();
                self.connection
                    .send(ClientMessage::ClientDisconnected)
                    .unwrap();
            },

            &ServerMessage::ContextGetBackendId => {},

            &ServerMessage::ContextGetMaxChannelCount => {
                match context.max_channel_count() {
                    Ok(channel_count) => {
                        self.connection
                            .send(ClientMessage::ContextMaxChannelCount(channel_count))
                            .unwrap();
                    },
                    Err(e) => {
                        self.send_error(e);
                    },
                }
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

                match context.min_latency(&params) {
                    Ok(latency) => {
                        self.connection
                            .send(ClientMessage::ContextMinLatency(latency))
                            .unwrap();
                    },
                    Err(e) => {
                        self.send_error(e);
                    },
                }
            },

            &ServerMessage::ContextGetPreferredSampleRate => {
                match context.preferred_sample_rate() {
                    Ok(rate) => {
                        self.connection
                            .send(ClientMessage::ContextPreferredSampleRate(rate))
                            .unwrap();
                    },
                    Err(e) => {
                        self.send_error(e);
                    },
                }
            },

            &ServerMessage::ContextGetPreferredChannelLayout => {
                match context.preferred_channel_layout() {
                    Ok(layout) => {
                        self.connection
                            .send(ClientMessage::ContextPreferredChannelLayout(layout as _))
                            .unwrap();
                    },
                    Err(e) => {
                        self.send_error(e);
                    },
                }
            },
            /*
            &ServerMessage::StreamInit(params) => {
                match server.connections[token].stream_init(server.context, params) {
                    Ok(_) => {
                        server.connections[token]
                            .send(ClientMessage::StreamCreated)
                            .unwrap()
                    },
                    Err(e) => {
                        self.send_error(e);
                    },
                }
            },
            &ServerMessage::StreamDestroy(stream) => {
                server.connections[token].stream_destroy(stream as *mut _);
                server.connections[token]
                    .send(ClientMessage::StreamDestroyed)
                    .unwrap();
            },

            &ServerMessage::StreamStart(stream) => {
                server.connections[token].stream.start();
                server.connections[token]
                    .send(ClientMessage::StreamStarted)
                    .unwrap();
            },
            &ServerMessage::StreamStop(stream) => {
                server.connections[token].stream.stop();
                server.connections[token]
                    .send(ClientMessage::StreamStopped)
                    .unwrap();
            },
            &ServerMessage::StreamGetPosition(stream) => {
                match server.connections[token].stream.position() {
                    Ok(position) => {
                        server.connections[token]
                            .send(ClientMessage::StreamPosition(position))
                            .unwrap()
                    },
                    Err(_) => panic!(""),
                }
            },
            &ServerMessage::StreamGetLatency(stream) => {
                match server.connections[token].stream.latency() {
                    Ok(latency) => {
                        server.connections[token]
                            .send(ClientMessage::StreamLatency(latency))
                            .unwrap()
                    },
                    Err(_) => panic!(""),
                }
            },
            &ServerMessage::StreamSetVolume(stream, volume) => {
                server.connections[token].stream.set_volume(volume);
                server.connections[token]
                    .send(ClientMessage::StreamVolumeSet())
                    .unwrap();
            },
            &ServerMessage::StreamSetPanning(stream, panning) => {
                server.connections[token].stream.set_panning();
                server.connections[token]
                    .send(ClientMessage::StreamPanningSet())
                    .unwrap();
            },
            &ServerMessage::StreamGetCurrentDevice(stream) => {
                panic!("Not implemented");
                            server.connections[token].stream_get_current_device(stream as *mut _);
                            server.connections[token].
                                send(ClientMessage::StreamCurrentDevice())
            .unwrap();
             */
            _ => bail!("Not implemented"),
        }
        Ok(())
    }

    fn send_error(&mut self, error: cubeb::Error) {
        self.connection
            .send(ClientMessage::ContextError(error.raw_code()))
            .unwrap();
    }
}

pub struct Server {
    socket: UnixListener,
    context: cubeb::Context,
    conns: Slab<ServerConn>
}

impl Server {
    pub fn new(socket: UnixListener) -> Server {
        let ctx = cubeb::Context::init("AudioIPC Server", None).expect("Failed to create cubeb context");

        Server {
            socket: socket,
            context: ctx,
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
            Ok(None) => unreachable!(),
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
            &self.conns[token].connection,
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
                token => {
                    debug!("token {:?} ready", token);

                    //                    let r = self.process(poll, token);

                    let r = self.conns[token].process(poll, &mut self.context);

                    debug!("got {:?}", r);

                    // TODO: Handle disconnection etc.
                    // TODO: Should be handled at a higher level by a disconnect message.
                    if let Err(e) = r {
                        debug!("dropped client {:?} due to error {:?}", token, e);
                        self.conns.remove(token);
                        continue;
                    }

                    // poll.reregister(
                    //     &self.conn(token).connection,
                    //     token,
                    //     mio::Ready::readable(),
                    //     mio::PollOpt::edge() | mio::PollOpt::oneshot()
                    // ).unwrap();
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
    let mut server = Server::new(UnixListener::bind(audioipc::get_uds_path())?);
    let mut poll = mio::Poll::new()?;

    poll.register(
        &server.socket,
        SERVER,
        mio::Ready::readable(),
        mio::PollOpt::edge() | mio::PollOpt::oneshot()
    ).unwrap();

    loop {
        if !running.load(Ordering::SeqCst) {
            bail!("server quit due to ctrl-c");
        }

        let _ = try!(server.poll(&mut poll));
    }
}
