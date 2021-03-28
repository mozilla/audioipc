// Copyright © 2021 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use std::io::Result;
use std::{collections::VecDeque, sync::mpsc};

use mio_07::Token;

use crate::ipccore::EventLoopHandle;

// RPC message handler.  Implemented by ClientHandler (for RpcClient)
// and ServerHandler (for RpcServer).
pub trait Handler {
    type In;
    type Out;

    // Consume a request
    fn consume(&mut self, request: Self::In) -> Result<()>;

    // Produce a response
    fn produce(&mut self) -> Result<Option<Self::Out>>;
}

// TODO: Rename traits after tokio code removed.

// Client RPC definition.  This supplies the expected message
// request and response types.
pub trait RpcClient {
    type Request;
    type Response;
}

// Server RPC definition.  This supplies the expected message
// request and response types.  `process` is passed inbound RPC
// requests by the ServerHandler to be responded to by the server.
pub trait RpcServer {
    type Request;
    type Response;

    fn process(&mut self, req: Self::Request) -> Self::Response;
}

// RPC Client Proxy implementation
type ProxyRequest<R, Q> = (R, mpsc::Sender<Q>);
type ProxyReceiver<R, Q> = mpsc::Receiver<ProxyRequest<R, Q>>;

// Each RPC Proxy `call` returns a blocking waitable ProxyResponse.
// `wait` produces the response received over RPC from the associated
// Proxy `call`.
pub struct ProxyResponse<Q> {
    inner: mpsc::Receiver<Q>,
}

impl<Q> ProxyResponse<Q> {
    pub fn wait(&self) -> Result<Q> {
        match self.inner.recv() {
            Ok(resp) => Ok(resp),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "proxy recv error",
            )),
        }
    }
}

// RPC Proxy that may be `clone`d for use by multiple owners/threads.
// A Proxy `call` arranges for the supplied request to be transmitted
// to the associated Server via RPC.  The response can be retrieved by
// `wait`ing on the returned ProxyResponse.
pub struct Proxy<R, Q> {
    handle: Option<(EventLoopHandle, Token)>,
    tx: mpsc::Sender<ProxyRequest<R, Q>>,
}

impl<R, Q> Proxy<R, Q> {
    pub fn call(&self, request: R) -> ProxyResponse<Q> {
        let (tx, rx) = mpsc::channel();
        self.tx.send((request, tx)).expect("proxy send error");
        let (handle, token) = self
            .handle
            .as_ref()
            .expect("proxy not connected to event loop");
        handle.wake_connection(*token);
        ProxyResponse { inner: rx }
    }

    pub(crate) fn connect_event_loop(&mut self, handle: EventLoopHandle, token: Token) {
        self.handle = Some((handle, token));
    }
}

impl<R, Q> Clone for Proxy<R, Q> {
    fn clone(&self) -> Self {
        Proxy {
            handle: self.handle.clone(),
            tx: self.tx.clone(),
        }
    }
}

fn make_proxy<R, Q>() -> (Proxy<R, Q>, ProxyReceiver<R, Q>) {
    let (tx, rx) = mpsc::channel();
    let proxy = Proxy { handle: None, tx };
    (proxy, rx)
}

// Client-specific Handler implementation.
// The IPC Core `Driver` calls this to execute client-specific
// RPC handling.  Serialized messages sent via a Proxy are queued
// for transmission when `produce` is called.
// Deserialized messages are passed via `consume` to
// trigger response completion by sending the response via a channel
// connected to a ProxyResponse.
pub struct ClientHandler<C: RpcClient> {
    requests: ProxyReceiver<C::Request, C::Response>,
    in_flight: VecDeque<mpsc::Sender<C::Response>>,
}

impl<C: RpcClient> Handler for ClientHandler<C> {
    type In = C::Response;
    type Out = C::Request;

    fn consume(&mut self, response: Self::In) -> Result<()> {
        trace!("ClientHandler::consume");
        if let Some(complete) = self.in_flight.pop_front() {
            drop(complete.send(response));
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

        // Try to get a new request
        match self.requests.try_recv() {
            Ok((request, complete)) => {
                trace!("  --> received request");
                self.in_flight.push_back(complete);
                Ok(Some(request))
            }
            Err(mpsc::TryRecvError::Empty) => {
                trace!("  --> no request");
                Ok(None)
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                trace!("  --> client disconnected");
                Ok(None) // TODO: Report useful error?
            }
        }
    }
}

pub fn make_client<C: RpcClient>() -> (ClientHandler<C>, Proxy<C::Request, C::Response>) {
    let (tx, rx) = make_proxy();

    let handler = ClientHandler::<C> {
        requests: rx,
        in_flight: VecDeque::with_capacity(32),
    };

    (handler, tx)
}

// Server-specific Handler implementation.
// The IPC Core `Driver` calls this to execute server-specific
// RPC handling.  Deserialized messages are passed via `consume` to the
// associated `server` for processing.  Server responses are then queued
// for RPC to the associated client when `produce` is called.
pub struct ServerHandler<S: RpcServer> {
    server: S,
    in_flight: VecDeque<S::Response>,
}

impl<S: RpcServer> Handler for ServerHandler<S> {
    type In = S::Request;
    type Out = S::Response;

    fn consume(&mut self, request: Self::In) -> Result<()> {
        trace!("ServerHandler::consume");
        let response = self.server.process(request);
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

pub fn make_server<S: RpcServer>(server: S) -> ServerHandler<S> {
    let handler = ServerHandler::<S> {
        server,
        in_flight: VecDeque::with_capacity(32),
    };

    handler
}
