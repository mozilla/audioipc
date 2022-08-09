// Copyright © 2021 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use std::collections::VecDeque;
use std::io::{self, Result};
use std::mem::ManuallyDrop;
use std::sync::Arc;

use crossbeam::channel::{self, Receiver, Sender};
use mio::Token;

use crate::ipccore::EventLoopHandle;

// RPC message handler.  Implemented by ClientHandler (for Client)
// and ServerHandler (for Server).
pub(crate) trait Handler {
    type In;
    type Out;

    // Consume a request
    fn consume(&mut self, request: Self::In) -> Result<()>;

    // Produce a response
    fn produce(&mut self) -> Result<Option<Self::Out>>;
}

// Client RPC definition.  This supplies the expected message
// request and response types.
pub trait Client {
    type ServerMessage;
    type ClientMessage;
}

// Server RPC definition.  This supplies the expected message
// request and response types.  `process` is passed inbound RPC
// requests by the ServerHandler to be responded to by the server.
pub trait Server {
    type ServerMessage;
    type ClientMessage;

    fn process(&mut self, req: Self::ServerMessage) -> Self::ClientMessage;
}

// RPC Client Proxy implementation.  ProxyRequest's Sender is connected to ProxyReceiver's Receiver,
// allowing the ProxyReceiver to wait on a response from the proxy.
type ProxyRequest<Request, Response> = (Request, Arc<Sender<Result<Response>>>);
type ProxyReceiver<Request, Response> = Receiver<ProxyRequest<Request, Response>>;

// RPC Proxy that may be `clone`d for use by multiple owners/threads.
// A Proxy `call` arranges for the supplied request to be transmitted
// to the associated Server via RPC.  Blocks until complete.
#[derive(Debug)]
pub struct Proxy<Request, Response> {
    handle: Option<(EventLoopHandle, Token)>,
    response_rx: Receiver<Result<Response>>,
    response_tx: Arc<Sender<Result<Response>>>,
    handler_tx: ManuallyDrop<Sender<ProxyRequest<Request, Response>>>,
}

impl<Request, Response> Proxy<Request, Response> {
    pub fn new(handler_tx: Sender<ProxyRequest<Request, Response>>) -> Self {
        let (tx, rx) = channel::bounded(1);
        Self {
            handle: None,
            response_rx: rx,
            response_tx: Arc::new(tx),
            handler_tx: ManuallyDrop::new(handler_tx),
        }
    }

    pub fn call(&self, request: Request) -> Result<Response> {
        match self.handler_tx.send((request, self.response_tx.clone())) {
            Ok(_) => self.wake_connection(),
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "proxy send error",
                ))
            }
        }
        // `response_tx` keeps the `Sender` alive, so we can't expect a
        // channel disconnection if the handler is shut down while a Proxy
        // is attached.  Instead, the handler *must* clear its processing queues
        // on drop and send any waiting Proxy calls an error.
        match self.response_rx.recv() {
            Ok(resp) => resp,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "proxy recv error",
            )),
        }
    }

    pub(crate) fn connect_event_loop(&mut self, handle: EventLoopHandle, token: Token) {
        self.handle = Some((handle, token));
    }

    fn wake_connection(&self) {
        let (handle, token) = self
            .handle
            .as_ref()
            .expect("proxy not connected to event loop");
        handle.wake_connection(*token);
    }
}

impl<Request, Response> Clone for Proxy<Request, Response> {
    fn clone(&self) -> Self {
        let (tx, rx) = channel::bounded(1);
        Self {
            handle: self.handle.clone(),
            response_rx: rx,
            response_tx: Arc::new(tx),
            handler_tx: self.handler_tx.clone(),
        }
    }
}

impl<Request, Response> Drop for Proxy<Request, Response> {
    fn drop(&mut self) {
        trace!("Proxy drop, waking EventLoop");
        // Must drop Sender before waking the connection, otherwise
        // the wake may be processed before Sender is closed.
        unsafe {
            ManuallyDrop::drop(&mut self.handler_tx);
        }
        if self.handle.is_some() {
            self.wake_connection()
        }
    }
}

// Client-specific Handler implementation.
// The IPC EventLoop Driver calls this to execute client-specific
// RPC handling.  Serialized messages sent via a Proxy are queued
// for transmission when `produce` is called.
// Deserialized messages are passed via `consume` to
// trigger response completion by sending the response via a channel
// connected to a ProxyResponse.
pub(crate) struct ClientHandler<C: Client> {
    messages: ProxyReceiver<C::ServerMessage, C::ClientMessage>,
    in_flight: VecDeque<Arc<Sender<Result<C::ClientMessage>>>>,
}

impl<C: Client> Handler for ClientHandler<C> {
    type In = C::ClientMessage;
    type Out = C::ServerMessage;

    fn consume(&mut self, response: Self::In) -> Result<()> {
        trace!("ClientHandler::consume");
        if let Some(complete) = self.in_flight.pop_front() {
            drop(complete.send(Ok(response)));
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "request/response mismatch",
            ));
        }

        Ok(())
    }

    fn produce(&mut self) -> Result<Option<Self::Out>> {
        trace!("ClientHandler::produce");

        // Try to get a new message
        match self.messages.try_recv() {
            Ok((request, response_tx)) => {
                trace!("  --> received request");
                self.in_flight.push_back(response_tx);
                Ok(Some(request))
            }
            Err(channel::TryRecvError::Empty) => {
                trace!("  --> no request");
                Ok(None)
            }
            Err(e) => {
                trace!("  --> client disconnected");
                Err(io::Error::new(io::ErrorKind::ConnectionAborted, e))
            }
        }
    }
}

impl<C: Client> Drop for ClientHandler<C> {
    fn drop(&mut self) {
        // Clear handler queues and send disconnection errors to
        // any waiting Proxy calls.
        while let Ok((_, complete)) = self.messages.try_recv() {
            drop(complete.send(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "clienthandler dropped",
            ))));
        }
        while let Some(complete) = self.in_flight.pop_front() {
            drop(complete.send(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "clienthandler dropped",
            ))));
        }
    }
}

pub(crate) fn make_client<C: Client>(
) -> (ClientHandler<C>, Proxy<C::ServerMessage, C::ClientMessage>) {
    let (tx, rx) = channel::bounded(32);

    let handler = ClientHandler::<C> {
        messages: rx,
        in_flight: VecDeque::with_capacity(32),
    };

    (handler, Proxy::new(tx))
}

// Server-specific Handler implementation.
// The IPC EventLoop Driver calls this to execute server-specific
// RPC handling.  Deserialized messages are passed via `consume` to the
// associated `server` for processing.  Server responses are then queued
// for RPC to the associated client when `produce` is called.
pub(crate) struct ServerHandler<S: Server> {
    server: S,
    in_flight: VecDeque<S::ClientMessage>,
}

impl<S: Server> Handler for ServerHandler<S> {
    type In = S::ServerMessage;
    type Out = S::ClientMessage;

    fn consume(&mut self, message: Self::In) -> Result<()> {
        trace!("ServerHandler::consume");
        let response = self.server.process(message);
        self.in_flight.push_back(response);
        Ok(())
    }

    fn produce(&mut self) -> Result<Option<Self::Out>> {
        trace!("ServerHandler::produce");

        // Return the ready response
        match self.in_flight.pop_front() {
            Some(res) => {
                trace!("  --> received response");
                Ok(Some(res))
            }
            None => {
                trace!("  --> no response ready");
                Ok(None)
            }
        }
    }
}

pub(crate) fn make_server<S: Server>(server: S) -> ServerHandler<S> {
    ServerHandler::<S> {
        server,
        in_flight: VecDeque::with_capacity(32),
    }
}
