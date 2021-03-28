// Copyright © 2021 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use std::io::Result;
use std::sync::{mpsc, Arc};

use mio_07::{event::Event, Events, Interest, Poll, Registry, Token, Waker};
use slab::Slab;

use crate::messages::AssocRawPlatformHandle;
use crate::rpccore::{make_client, make_server, Handler, Proxy, RpcClient, RpcServer};
use crate::{
    codec::Codec,
    codec::LengthDelimitedCodec,
    sys::{self, RecvMsg, SendMsg},
    ClientMessage, PlatformHandle, ServerMessage,
};

use serde::{de::DeserializeOwned, Serialize};
use std::fmt::Debug;

#[cfg(unix)]
use crate::cmsg;
#[cfg(windows)]
use crate::duplicate_platform_handle;

const WAKE_TOKEN: Token = Token(!0);

// Requests sent by an EventLoopHandle to be serviced by
// the handle's associated EventLoop.
enum Request {
    // See EventLoop::add_connection
    AddConnection(
        sys::Pipe,
        Box<dyn Driver + Send>,
        mpsc::Sender<Result<Token>>,
    ),
    // See EventLoop::shutdown
    Shutdown,
    // See EventLoop::wake_connection
    WakeConnection(Token),
}

// EventLoopHandle is a cloneable external reference
// to a running EventLoop, allowing registration of
// new client and server connections, in addition to
// requesting the EventLoop shut down cleanly.
#[derive(Clone)]
pub struct EventLoopHandle {
    waker: Arc<Waker>,
    requests_tx: mpsc::Sender<Request>,
}

impl EventLoopHandle {
    pub fn new_client<C: RpcClient + 'static>(
        &self,
        server_pipe: sys::Pipe,
    ) -> Result<Proxy<<C as RpcClient>::Request, <C as RpcClient>::Response>>
    where
        <C as RpcClient>::Request: Serialize + Debug + AssocRawPlatformHandle + Send,
        <C as RpcClient>::Response: DeserializeOwned + Debug + AssocRawPlatformHandle + Send,
    {
        let (handler, mut proxy) = make_client::<C>();
        let driver = Box::new(FramedDriver::new(handler));
        let token = self.add_connection(server_pipe, driver)?;
        proxy.connect_event_loop(self.clone(), token);
        Ok(proxy)
    }

    pub fn new_server<S: RpcServer + Send + 'static>(&self, server: S) -> Result<PlatformHandle>
    where
        <S as RpcServer>::Request: DeserializeOwned + Debug + AssocRawPlatformHandle + Send,
        <S as RpcServer>::Response: Serialize + Debug + AssocRawPlatformHandle + Send,
    {
        let handler = make_server::<S>(server);
        let driver = Box::new(FramedDriver::new(handler));
        let (server_pipe, client_pipe) = sys::make_pipe_pair()?;
        self.add_connection(server_pipe, driver)?;
        Ok(client_pipe)
    }

    // Register a new connection with associated driver on the EventLoop.
    // TODO: Since this is called from a Gecko main thread, make this non-blocking wrt. the EventLoop.
    fn add_connection(
        &self,
        connection: sys::Pipe,
        driver: Box<dyn Driver + Send>,
    ) -> Result<Token> {
        let (tx, rx) = mpsc::channel();
        self.requests_tx
            .send(Request::AddConnection(connection, driver, tx))
            .expect("EventLoop::add_connection");
        self.waker.wake().expect("wake failed");
        let token = rx.recv().expect("EventLoop::add_connection")?;
        Ok(token)
    }

    // Signal EventLoop to shutdown.  Causes EventLoop::poll to return Ok(false).
    pub fn shutdown(&self) {
        self.requests_tx
            .send(Request::Shutdown)
            .expect("EventLoop::shutdown");
        self.waker.wake().expect("wake failed");
    }

    // Signal EventLoop to wake connection specified by `token` for processing.
    pub fn wake_connection(&self, token: Token) {
        self.requests_tx
            .send(Request::WakeConnection(token))
            .expect("EventLoop::shutdown");
        self.waker.wake().expect("wake failed");
    }
}

// EventLoop owns all registered connections, and is responsible for calling each connection's
// `handle_event` function any time a readiness or wake event associated with that connection is
// produced.
struct EventLoop {
    poll: Poll,
    events: Events,
    waker: Arc<Waker>,
    connections: Slab<Connection>,
    requests: mpsc::Receiver<Request>,
    requests_tx: mpsc::Sender<Request>,
}

const EVENT_LOOP_INITIAL_CLIENTS: usize = 64; // Initial client allocation, exceeding this will cause the connection slab to grow.
const EVENT_LOOP_EVENTS_PER_ITERATION: usize = 256; // Number of events per poll() step, arbitrary limit.

impl EventLoop {
    fn new() -> Result<EventLoop> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE_TOKEN)?);
        let (tx, rx) = mpsc::channel();
        let eventloop = EventLoop {
            poll,
            events: Events::with_capacity(EVENT_LOOP_EVENTS_PER_ITERATION),
            waker,
            connections: Slab::with_capacity(EVENT_LOOP_INITIAL_CLIENTS),
            requests: rx,
            requests_tx: tx,
        };

        Ok(eventloop)
    }

    // Return a cloneable handle for controlling the EventLoop externally.
    fn handle(&mut self) -> EventLoopHandle {
        EventLoopHandle {
            waker: self.waker.clone(),
            requests_tx: self.requests_tx.clone(),
        }
    }

    // Register a connection and driver.
    fn add_connection(
        &mut self,
        connection: sys::Pipe,
        driver: Box<dyn Driver + Send>,
    ) -> Result<Token> {
        if self.connections.len() == self.connections.capacity() {
            trace!("connection slab full, insert will allocate");
        }
        let entry = self.connections.vacant_entry();
        let token = Token(entry.key());
        let connection = Connection::new(connection, token, driver, self.poll.registry())?;
        debug!("[{:?}]: new connection", token);
        entry.insert(connection);
        Ok(token)
    }

    // Step EventLoop once.  Call this in a loop from a dedicated thread.
    // Returns false if EventLoop is shutting down.
    // Each step may call `handle_event` on any registered connection that
    // has received readiness events from the poll wakeup.
    fn poll(&mut self) -> Result<bool> {
        self.poll.poll(&mut self.events, None)?;

        for event in self.events.iter() {
            match event.token() {
                WAKE_TOKEN => {
                    debug!("WAKE: wake event, will process requests");
                }
                token => {
                    debug!("[{:?}]: connection ready: {:?}", token, event);
                    let done = if let Some(connection) = self.connections.get_mut(token.0) {
                        match connection.handle_event(Some(event), self.poll.registry()) {
                            Ok(done) => done,
                            Err(e) => {
                                error!("[{:?}]: connection error: {:?}", token, e);
                                true
                            }
                        }
                    } else {
                        debug!("[{:?}]: token not found in slab: {:?}", token, event);
                        debug_assert!(false); // This shouldn't happen, catch it in debug mode.
                        false
                    };
                    if done {
                        debug!("[{:?}]: done, removing", token);
                        let connection = self.connections.remove(token.0);
                        connection.shutdown(self.poll.registry())?;
                    }
                }
            }
        }

        // If the waker was signalled there may be pending requests to process.
        while let Ok(req) = self.requests.try_recv() {
            match req {
                Request::AddConnection(pipe, driver, tx) => {
                    debug!("EventLoop: handling add_connection");
                    let r = self.add_connection(pipe, driver);
                    tx.send(r).expect("EventLoop::add_connection");
                }
                Request::Shutdown => {
                    debug!("EventLoop: handling shutdown");
                    return Ok(false);
                }
                Request::WakeConnection(token) => {
                    debug!("EventLoop: handling wake_connection [{:?}]", token);
                    if let Some(connection) = self.connections.get_mut(token.0) {
                        match connection.handle_event(None, self.poll.registry()) {
                            Ok(done) => assert_eq!(done, false),
                            Err(e) => {
                                error!("[{:?}]: connection error: {:?}", token, e);
                            }
                        }
                    } else {
                        debug!("[{:?}]: token not found in slab: wake_connection", token);
                        debug_assert!(false); // This shouldn't happen, catch it in debug mode.
                    };
                }
            }
        }

        Ok(true)
    }
}

// Connection wraps an interprocess connection (Pipe) and manages
// receiving inbound and sending outbound buffers (and associated handles, if any).
// The associated driver is responsible for message framing and serialization.
struct Connection {
    io: sys::Pipe,
    token: Token,
    interest: Interest,
    inbound: sys::ConnectionBuffer,
    outbound: sys::ConnectionBuffer,
    driver: Box<dyn Driver + Send>,
}

const IPC_CLIENT_BUFFER_SIZE: usize = 16384;

impl Connection {
    fn new(
        mut io: sys::Pipe,
        token: Token,
        driver: Box<dyn Driver + Send>,
        registry: &Registry,
    ) -> Result<Connection> {
        let interest = Interest::READABLE;
        registry.register(&mut io, token, interest)?;
        Ok(Connection {
            io,
            token,
            interest,
            inbound: sys::ConnectionBuffer::with_capacity(IPC_CLIENT_BUFFER_SIZE),
            outbound: sys::ConnectionBuffer::with_capacity(IPC_CLIENT_BUFFER_SIZE),
            driver,
        })
    }

    fn shutdown(mut self, registry: &Registry) -> Result<()> {
        registry.deregister(&mut self.io)
    }

    // Update connection registration with the current readiness event interests.
    fn update_registration(&mut self, registry: &Registry) -> Result<()> {
        registry.reregister(&mut self.io, self.token, self.interest)
    }

    // Handle readiness event.  Errors returned are fatal for this connection, resulting in removal from the EventLoop connection list.
    // The EventLoop will call this for any connection that has received an event, including external wake events to clear the outbound buffer.
    fn handle_event(&mut self, event: Option<&Event>, registry: &Registry) -> Result<bool> {
        if let Some(event) = event {
            assert_eq!(self.token, event.token());
        }
        debug!("[{:?}]: handling event {:?}", self.token, event);

        // If the connection is readable, read into inbound and pass to driver for processing until all ready data
        // has been consumed.
        //let done = if let Some(event) = event {
        let done = if let Some(event) = event {
            if event.is_readable() {
                loop {
                    trace!("[{:?}]: pre-recv inbound: {:?}", self.token, self.inbound);
                    let r = self.io.recv_msg(&mut self.inbound);
                    match r {
                        Ok(0) => {
                            trace!("[{:?}]: recv EOF", self.token);
                            assert!(self.inbound.is_empty()); // Ensure no unprocessed messages queued.
                            return Ok(true);
                        }
                        Ok(n) => {
                            trace!("[{:?}]: recv bytes: {}, process_inbound", self.token, n);
                            let r = self.driver.process_inbound(&mut self.inbound);
                            trace!("[{:?}]: process_inbound done: {:?}", self.token, r);
                            match r {
                                Ok(done) => {
                                    if done {
                                        break done;
                                    }
                                }
                                Err(e) => {
                                    error!("[{:?}]: process_inbound error: {:?}", self.token, e);
                                    assert!(self.inbound.is_empty()); // Ensure no unprocessed messages queued.
                                    return Err(e);
                                }
                            }
                        }
                        Err(ref e) if would_block(e) => {
                            trace!("[{:?}]: recv would_block: {:?}", self.token, e);
                            break false;
                        }
                        Err(ref e) if interrupted(e) => {
                            trace!("[{:?}]: recv interrupted: {:?}", self.token, e);
                            continue;
                        }
                        Err(e) => {
                            error!("[{:?}]: recv error: {:?}", self.token, e);
                            return Err(e);
                        }
                    }
                }
            } else {
                false
            }
        } else {
            false
        };

        // Enqueue outbound messages to the outbound buffer, then try to write out to connection.
        // There may be outbound messages even if there was no inbound processing, so always attempt
        // to enqueue and flush.
        trace!("[{:?}]: flush_outbound", self.token);
        let r = self.driver.flush_outbound(&mut self.outbound);
        trace!("[{:?}]: flush_outbound done: {:?}", self.token, r);
        match r {
            Err(e) => {
                error!("[{:?}]: flush_outbound error: {:?}", self.token, e);
                return Err(e);
            }
            _ => {}
        }

        // Attempt to flush outbound buffer.  If the connection's write buffer is full, register for WRITABLE
        // and complete flushing when associated notitication arrives later.
        while !self.outbound.is_empty() {
            let r = self.io.send_msg(&mut self.outbound);
            match r {
                Ok(0) => {
                    trace!("[{:?}]: send EOF", self.token);
                    return Ok(true);
                }
                Ok(n) => {
                    trace!("[{:?}]: send bytes: {}", self.token, n);
                }
                Err(ref e) if would_block(e) => {
                    trace!("[{:?}]: send would_block: {:?}", self.token, e);
                    // Register for write events.
                    if !self.interest.is_writable() {
                        self.interest.add(Interest::WRITABLE);
                        self.update_registration(registry)?;
                    }
                    break;
                }
                Err(ref e) if interrupted(e) => {
                    trace!("[{:?}]: send interrupted: {:?}", self.token, e);
                    continue;
                }
                Err(e) => {
                    error!("[{:?}]: send error: {:?}", self.token, e);
                    return Err(e);
                }
            }
            trace!(
                "[{:?}]: post-send: outbound {:?}",
                self.token,
                self.outbound
            );
        }

        // If driver is done, stop reading.  We may have more outbound to flush.
        if done {
            trace!("[{:?}]: driver done, clearing read interest", self.token);
            self.interest.remove(Interest::READABLE);
            self.update_registration(registry)?;
        }

        // Outbound buffer flushed, clear registration for WRITABLE.
        // Note that Windows NamedPipes will cause an additional WRITABLE notification after a write, even if
        // we're no longer registered for WRITABLE.  Any user of Poll is expected to handle spurious events,
        // so this is fine.
        if let Some(event) = event {
            if event.is_writable() && self.outbound.is_empty() {
                trace!(
                    "[{:?}]: outbound empty, clearing write interest",
                    self.token
                );
                self.interest.remove(Interest::WRITABLE);
                self.update_registration(registry)?;
            }
        }

        debug!("[{:?}]: handling event done", self.token);
        Ok(done && self.outbound.is_empty())
    }
}

fn would_block(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::WouldBlock
}

fn interrupted(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::Interrupted
}

// Driver only has a single implementation, but must be hidden behind a Trait object to
// hide the varying FramedDriver sizes (due to different `T` values).
trait Driver {
    // Handle inbound messages.  Returns true if Driver is done; this will trigger Connection removal and cleanup.
    fn process_inbound(&mut self, inbound: &mut sys::ConnectionBuffer) -> Result<bool>;

    // Write outbound messages to `outbound`.
    fn flush_outbound(&mut self, outbound: &mut sys::ConnectionBuffer) -> Result<()>;
}

// Length-delimited connection framing and (de)serialization is handled by the inbound and outbound processing.
// Handlers can then process message Requests and Responses without knowledge of serialization or
// handle remoting.
impl<T> Driver for FramedDriver<T>
where
    T: Handler,
    T::In: DeserializeOwned + Debug + AssocRawPlatformHandle,
    T::Out: Serialize + Debug + AssocRawPlatformHandle,
{
    // Caller passes `inbound` data, this function will trim any complete messages from `inbound` and pass them to the handler for processing.
    fn process_inbound(&mut self, inbound: &mut sys::ConnectionBuffer) -> Result<bool> {
        debug!("process_inbound: {:?}", inbound);

        // Repeatedly call `decode` as long as it produces items, passing each produced item to the handler to action.
        #[allow(unused_mut)]
        while let Some(mut item) = self.codec.decode(&mut inbound.buf)? {
            #[cfg(unix)]
            {
                let mut handle = None;
                let b = inbound.cmsg.clone().freeze();
                for fd in cmsg::iterator(b) {
                    assert_eq!(fd.len(), 1);
                    assert!(handle.is_none());
                    handle = Some(fd[0]);
                }
                item.set_owned_handle(|| handle);
            }
            self.handler.consume(item)?;
        }

        Ok(false)
    }

    // Caller will try to write `outbound` to associated connection, queuing any data that can't be transmitted immediately.
    fn flush_outbound(&mut self, outbound: &mut sys::ConnectionBuffer) -> Result<()> {
        debug!("flush_outbound: {:?}", outbound.buf);

        // Repeatedly grab outgoing items from the handler, passing each to `encode` for serialization into `outbound`.
        while let Some(mut item) = self.handler.produce()? {
            let handle = item.take_handle_for_send();

            // On Windows, the handle is transferred by duplicating it into the target remote process during message send.
            #[cfg(windows)]
            if let Some((handle, target_pid)) = handle {
                let remote_handle = unsafe { duplicate_platform_handle(handle, Some(target_pid))? };
                trace!(
                    "item handle: {:?} remote_handle: {:?}",
                    handle,
                    remote_handle
                );
                // The new handle in the remote process is indicated by updating the handle stored in the item with the expected
                // value on the remote.
                item.set_remote_handle_value(|| Some(remote_handle));
            }
            // On Unix, the handle is encoded into a cmsg buffer for out-of-band transport via sendmsg.
            #[cfg(unix)]
            if let Some((handle, _)) = handle {
                item.set_remote_handle_value(|| Some(handle));
            }

            self.codec.encode(item, &mut outbound.buf)?;

            #[cfg(unix)]
            if handle.is_some() {
                cmsg::builder(&mut outbound.cmsg).rights(&[handle.0]);
            }
        }
        Ok(())
    }
}

struct FramedDriver<T: Handler> {
    codec: LengthDelimitedCodec<T::Out, T::In>,
    handler: T,
}

impl<T: Handler> FramedDriver<T> {
    fn new(handler: T) -> FramedDriver<T> {
        FramedDriver {
            codec: Default::default(),
            handler,
        }
    }
}

// TODO: Stand-in impls, replace with rpc::Client/rpc::Server
struct ClientTestImpl {}

impl RpcClient for ClientTestImpl {
    type Request = ServerMessage;
    type Response = ClientMessage;
}

struct ServerTestImpl {}

impl RpcServer for ServerTestImpl {
    type Request = ServerMessage;
    type Response = ClientMessage;

    fn process(&mut self, req: Self::Request) -> Self::Response {
        eprintln!("ServerTestImpl: got req: {:?}", req);
        ClientMessage::StreamStarted
    }
}

// TODO: Make test Server/Client and bounce a simple message back and forth
pub fn test_run() -> std::result::Result<(), std::io::Error> {
    // Setup server loop, register ServerTestImpl
    debug!("test_run: start server loop");
    let mut server_loop = EventLoop::new()?;
    let server_rpc = server_loop.handle();
    let server_thread: std::thread::JoinHandle<Result<()>> = std::thread::spawn(move || {
        while server_loop.poll()? {
            debug!("server poll step");
        }
        Ok(())
    });

    debug!("test_run: register server on server loop");
    let client_pipe = server_rpc.new_server(ServerTestImpl {}).unwrap();

    // Setup client loop, register ClientTestImpl
    debug!("test_run: start client loop");
    let mut client_loop = EventLoop::new()?;
    let client_rpc = client_loop.handle();
    let client_thread: std::thread::JoinHandle<Result<()>> = std::thread::spawn(move || {
        while client_loop.poll()? {
            debug!("client poll step");
        }
        Ok(())
    });

    let client_pipe = unsafe { sys::Pipe::from_raw_handle(client_pipe.into_raw()) };

    debug!("test_run: register client on client loop");
    let client_proxy = client_rpc
        .new_client::<ClientTestImpl>(client_pipe)
        .unwrap();

    debug!("test_run: send StreamStart(42), wait for response");
    let response = client_proxy
        .call(ServerMessage::StreamStart(42))
        .wait()
        .unwrap();
    debug!("test_run: got response = {:?}", response);

    client_rpc.shutdown();
    server_rpc.shutdown();
    server_thread.join().unwrap().unwrap();
    client_thread.join().unwrap().unwrap();
    Ok(())
}
