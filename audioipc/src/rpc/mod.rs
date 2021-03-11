// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use futures::{Sink, Stream};
use std::io;
use std::task::Poll;

mod client;
mod driver;
mod server;

pub use self::client::{bind_client, Client, ClientProxy, Response};
pub use self::server::{bind_server, Server};

pub trait Handler {
    /// Message type read from transport
    type In;
    /// Message type written to transport
    type Out;
    type Transport: 'static
        + Stream<Item = Result<Self::In, io::Error>>
        + Sink<Self::Out, Error = io::Error>;

    /// Mutable reference to the transport
    fn transport(&mut self) -> &mut Self::Transport;

    /// Consume a request
    fn consume(&mut self, message: Self::In) -> io::Result<()>;

    /// Produce a response
    fn produce(&mut self) -> Poll<Result<Option<Self::Out>, io::Error>>;

    /// RPC currently in flight
    fn has_in_flight(&self) -> bool;
}
