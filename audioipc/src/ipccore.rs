// Copyright © 2021 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use std::io::Result;
use std::sync::{mpsc, Arc};
use std::thread;

use mio::{event::Event, Events, Interest, Poll, Registry, Token, Waker};
use slab::Slab;

use crate::messages::AssocRawPlatformHandle;
use crate::rpccore::{make_client, make_server, Client, Handler, Proxy, Server};
use crate::{
    codec::Codec,
    codec::LengthDelimitedCodec,
    sys::{self, RecvMsg, SendMsg},
};

use serde::{de::DeserializeOwned, Serialize};
use std::fmt::Debug;

#[cfg(windows)]
use crate::duplicate_platform_handle;
#[cfg(unix)]
use crate::sys::cmsg;

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
#[derive(Clone, Debug)]
pub struct EventLoopHandle {
    waker: Arc<Waker>,
    requests_tx: mpsc::Sender<Request>,
}

impl EventLoopHandle {
    pub fn bind_client<C: Client + 'static>(
        &self,
        connection: sys::Pipe,
    ) -> Result<Proxy<<C as Client>::ServerMessage, <C as Client>::ClientMessage>>
    where
        <C as Client>::ServerMessage: Serialize + Debug + AssocRawPlatformHandle + Send,
        <C as Client>::ClientMessage: DeserializeOwned + Debug + AssocRawPlatformHandle + Send,
    {
        let (handler, mut proxy) = make_client::<C>();
        let driver = Box::new(FramedDriver::new(handler));
        let token = self.add_connection(connection, driver)?;
        proxy.connect_event_loop(self.clone(), token);
        Ok(proxy)
    }

    pub fn bind_server<S: Server + Send + 'static>(
        &self,
        server: S,
        connection: sys::Pipe,
    ) -> Result<()>
    where
        <S as Server>::ServerMessage: DeserializeOwned + Debug + AssocRawPlatformHandle + Send,
        <S as Server>::ClientMessage: Serialize + Debug + AssocRawPlatformHandle + Send,
    {
        let handler = make_server::<S>(server);
        let driver = Box::new(FramedDriver::new(handler));
        let r = self.add_connection(connection, driver);
        trace!("EventLoop::bind_server {:?}", r);
        Ok(())
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
    pub(crate) fn wake_connection(&self, token: Token) {
        self.requests_tx
            .send(Request::WakeConnection(token))
            .expect("EventLoop::shutdown");
        self.waker.wake().expect("wake failed");
    }
}

// EventLoop owns all registered connections, and is responsible for calling each connection's
// `handle_event` or `handle_wake` any time a readiness or wake event associated with that connection is
// produced.
struct EventLoop {
    poll: Poll,
    events: Events,
    waker: Arc<Waker>,
    connections: Slab<Connection>,
    requests_rx: mpsc::Receiver<Request>,
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
            requests_rx: rx,
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
        assert_ne!(token, WAKE_TOKEN);
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
                        match connection.handle_event(event, self.poll.registry()) {
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
        while let Ok(req) = self.requests_rx.try_recv() {
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
                        match connection.handle_wake(self.poll.registry()) {
                            Ok(done) => assert!(!done),
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
    fn update_registration(&mut self, interest: Interest, registry: &Registry) -> Result<()> {
        self.interest = interest;
        registry.reregister(&mut self.io, self.token, self.interest)
    }

    // Handle readiness event.  Errors returned are fatal for this connection, resulting in removal from the EventLoop connection list.
    // The EventLoop will call this for any connection that has received an event.
    fn handle_event(&mut self, event: &Event, registry: &Registry) -> Result<bool> {
        debug!("[{:?}]: handling event {:?}", self.token, event);
        assert_eq!(self.token, event.token());
        let done = if event.is_readable() {
            self.recv_inbound()?
        } else {
            trace!("[{:?}]: not readable", self.token);
            false
        };
        self.flush_outbound()?;
        if self.send_outbound(registry)? {
            // Hit EOF during send
            return Ok(true);
        }
        // If driver is done, stop reading.  We may have more outbound to flush.
        if done {
            trace!("[{:?}]: driver done, clearing read interest", self.token);
            self.update_registration(self.interest.remove(Interest::READABLE).unwrap(), registry)?;
        }
        debug!(
            "[{:?}]: handling event done (done={}, outbound={})",
            self.token,
            done,
            self.outbound.is_empty()
        );
        Ok(done && self.outbound.is_empty())
    }

    // Handle wake event.  Errors returned are fatal for this connection, resulting in removal from the EventLoop connection list.
    // The EventLoop will call this to clear the outbound buffer for any connection that has received a wake event.
    fn handle_wake(&mut self, registry: &Registry) -> Result<bool> {
        debug!("[{:?}]: handling wake", self.token);
        self.flush_outbound()?;
        if self.send_outbound(registry)? {
            // Hit EOF during send
            return Ok(true);
        }
        debug!("[{:?}]: handling wake done", self.token);
        Ok(false)
    }

    fn recv_inbound(&mut self) -> Result<bool> {
        // If the connection is readable, read into inbound and pass to driver for processing until all ready data
        // has been consumed.
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
                                return Ok(done);
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
                    return Ok(false);
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
    }

    fn flush_outbound(&mut self) -> Result<()> {
        // Enqueue outbound messages to the outbound buffer, then try to write out to connection.
        // There may be outbound messages even if there was no inbound processing, so always attempt
        // to enqueue and flush.
        trace!("[{:?}]: flush_outbound", self.token);
        let r = self.driver.flush_outbound(&mut self.outbound);
        trace!("[{:?}]: flush_outbound done: {:?}", self.token, r);
        if let Err(e) = r {
            error!("[{:?}]: flush_outbound error: {:?}", self.token, e);
            return Err(e);
        }
        Ok(())
    }

    fn send_outbound(&mut self, registry: &Registry) -> Result<bool> {
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
                        self.update_registration(self.interest.add(Interest::WRITABLE), registry)?;
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
        // Outbound buffer flushed, clear registration for WRITABLE.
        // Note that Windows NamedPipes will cause an additional WRITABLE notification after a write, even if
        // we're no longer registered for WRITABLE.  Any user of Poll is expected to handle spurious events,
        // so this is fine.
        if self.outbound.is_empty() {
            trace!(
                "[{:?}]: outbound empty, clearing write interest",
                self.token
            );
            self.update_registration(self.interest.remove(Interest::WRITABLE).unwrap(), registry)?;
        }
        Ok(false)
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
                // TODO: Clean this up to only expect a single fd per message.
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
            if let Some((handle, _)) = handle {
                // TODO: Rework builder to commit directly to outbound buffer.
                match cmsg::builder(&mut outbound.cmsg).rights(&[handle]).finish() {
                    Ok(handle_bytes) => outbound.cmsg.extend_from_slice(&handle_bytes),
                    Err(e) => debug!("cmsg::builder failed: {:?}", e),
                }
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

#[derive(Debug)]
pub struct EventLoopThread {
    thread: Option<thread::JoinHandle<Result<()>>>,
    handle: EventLoopHandle,
}

impl EventLoopThread {
    pub fn new<F1, F2>(
        name: String,
        stack_size: Option<usize>,
        after_start: F1,
        before_stop: F2,
    ) -> Result<Self>
    where
        F1: Fn() + Send + 'static,
        F2: Fn() + Send + 'static,
    {
        let mut event_loop = EventLoop::new()?;
        let handle = event_loop.handle();

        let builder = thread::Builder::new()
            .name(name.clone())
            .stack_size(stack_size.unwrap_or(64 * 4096));

        let thread = builder.spawn(move || {
            after_start();
            let _thread_exit_guard = scopeguard::guard((), |_| before_stop());

            while event_loop.poll()? {
                trace!("{}: event loop poll", name);
            }

            trace!("{}: event loop shutdown", name);
            Ok(())
        })?;

        Ok(EventLoopThread {
            thread: Some(thread),
            handle,
        })
    }

    pub fn handle(&self) -> &EventLoopHandle {
        &self.handle
    }
}

impl Drop for EventLoopThread {
    // Shut down event loop and executor thread.  Blocks until complete.
    fn drop(&mut self) {
        trace!("EventLoopThread shutdown");
        self.handle.shutdown();
        let thread = self.thread.take().expect("event loop thread");
        if let Err(e) = thread.join() {
            warn!("EventLoopThread failed: {:?}", e);
        }
        trace!("EventLoopThread shutdown done");
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde_derive::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    enum TestServerMessage {
        TestRequest,
    }
    impl AssocRawPlatformHandle for TestServerMessage {}

    struct TestServerImpl {}

    impl Server for TestServerImpl {
        type ServerMessage = TestServerMessage;
        type ClientMessage = TestClientMessage;

        fn process(&mut self, req: Self::ServerMessage) -> Self::ClientMessage {
            assert_eq!(req, TestServerMessage::TestRequest);
            TestClientMessage::TestResponse
        }
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    enum TestClientMessage {
        TestResponse,
    }

    impl AssocRawPlatformHandle for TestClientMessage {}

    struct TestClientImpl {}

    impl Client for TestClientImpl {
        type ServerMessage = TestServerMessage;
        type ClientMessage = TestClientMessage;
    }

    fn setup() -> (
        EventLoopThread,
        EventLoopThread,
        Proxy<TestServerMessage, TestClientMessage>,
    ) {
        // Server setup and registration.
        let server_elt = EventLoopThread::new("test-server".to_string(), None, || {}, || {})
            .expect("server EventLoopThread");
        let server_handle = server_elt.handle();

        let (server_pipe, client_pipe) = sys::make_pipe_pair().expect("server make_pipe_pair");
        server_handle
            .bind_server(TestServerImpl {}, server_pipe)
            .expect("server bind_server");

        // Client setup and registration.
        let client_elt = EventLoopThread::new("test-client".to_string(), None, || {}, || {})
            .expect("client EventLoopThread");
        let client_handle = client_elt.handle();

        let client_pipe = unsafe { sys::Pipe::from_raw_handle(client_pipe) };
        let client_proxy = client_handle
            .bind_client::<TestClientImpl>(client_pipe)
            .expect("client bind_client");

        (server_elt, client_elt, client_proxy)
    }

    // Verify basic EventLoopThread functionality works.  Create a server and client EventLoopThread, then send
    // a single message from the client to the server and wait for the expected response.
    #[test]
    fn basic() {
        let (server, client, client_proxy) = setup();

        // RPC message from client to server.
        let response = client_proxy.call(TestServerMessage::TestRequest);
        let response = response.wait().expect("client response");
        assert_eq!(response, TestClientMessage::TestResponse);

        // Explicit shutdown.
        drop(client);
        drop(server);
    }

    // Same as `basic`, but shut down server before client.
    #[test]
    fn basic_reverse_drop_order() {
        let (server, client, client_proxy) = setup();

        // RPC message from client to server.
        let response = client_proxy.call(TestServerMessage::TestRequest);
        let response = response.wait().expect("client response");
        assert_eq!(response, TestClientMessage::TestResponse);

        // Explicit shutdown.
        drop(server);
        drop(client);
    }

    #[test]
    fn basic_event_loop_thread_callbacks() {
        let after_start1 = Arc::new(AtomicBool::new(false));
        let after_start2 = after_start1.clone();
        let before_stop1 = Arc::new(AtomicBool::new(false));
        let before_stop2 = before_stop1.clone();

        let server_elt = EventLoopThread::new(
            "test-thread-callbacks".to_string(),
            None,
            move || {
                after_start1.store(true, Ordering::Release);
            },
            move || {
                before_stop1.store(true, Ordering::Release);
            },
        )
        .expect("server EventLoopThread");

        // Explicit shutdown.
        drop(server_elt);

        assert!(after_start2.load(Ordering::Acquire));
        assert!(before_stop2.load(Ordering::Acquire));
    }
}
