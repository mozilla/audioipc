// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use crate::rpc::Handler;
use futures::sink::Sink;
use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task;
use std::task::Poll;

pub struct Driver<T>
where
    T: Handler,
{
    // Glue
    handler: T,

    // True as long as the connection has more request frames to read.
    run: bool,

    // True when the transport is fully flushed
    is_flushed: bool,
}

impl<T> Driver<T>
where
    T: Handler,
{
    /// Create a new rpc driver with the given service and transport.
    pub fn new(handler: T) -> Driver<T> {
        Driver {
            handler,
            run: true,
            is_flushed: true,
        }
    }

    /// Returns true if the driver has nothing left to do
    fn is_done(&self) -> bool {
        !self.run && self.is_flushed && !self.has_in_flight()
    }

    /// Process incoming messages off the transport.
    fn receive_incoming(&mut self) -> io::Result<()> {
        while self.run {
            if let Poll::Ready(req) = self.handler.transport().poll()? {
                self.process_incoming(req)?;
            } else {
                break;
            }
        }
        Ok(())
    }

    /// Process an incoming message
    fn process_incoming(&mut self, req: Option<T::In>) -> io::Result<()> {
        trace!("process_incoming");
        // At this point, the service & transport are ready to process the
        // request, no matter what it is.
        match req {
            Some(message) => {
                trace!("received message");

                if let Err(e) = self.handler.consume(message) {
                    // TODO: Should handler be infalliable?
                    panic!("unimplemented error handling: {:?}", e);
                }
            }
            None => {
                trace!("received None");
                // At this point, we just return. This works
                // because poll with be called again and go
                // through the receive-cycle again.
                self.run = false;
            }
        }

        Ok(())
    }

    /// Send outgoing messages to the transport.
    fn send_outgoing(&mut self) -> io::Result<()> {
        trace!("send_responses");
        loop {
            match self.handler.produce()? {
                Poll::Ready(Some(message)) => {
                    trace!("  --> got message");
                    self.process_outgoing(message)?;
                }
                Poll::Ready(None) => {
                    trace!("  --> got None");
                    // The service is done with the connection.
                    self.run = false;
                    break;
                }
                // Nothing to dispatch
                Poll::Pending => break,
            }
        }

        Ok(())
    }

    fn process_outgoing(&mut self, message: T::Out) -> io::Result<()> {
        trace!("process_outgoing");
        self.handler.transport().start_send(message)?;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.is_flushed = self.handler.transport().poll_complete()?.is_ready();

        // TODO:
        Ok(())
    }

    fn has_in_flight(&self) -> bool {
        self.handler.has_in_flight()
    }
}

impl<T> Future for Driver<T>
where
    T: Handler,
{
    type Output = Result<(), io::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Self::Output> {
        trace!("rpc::Driver::tick");

        // First read off data from the socket
        self.receive_incoming()?;

        // Handle completed responses
        self.send_outgoing()?;

        // Try flushing buffered writes
        self.flush()?;

        if self.is_done() {
            trace!("  --> is done.");
            return Poll::Ready(());
        }

        // Tick again later
        Poll::Pending
    }
}

impl<T> fmt::Debug for Driver<T>
where
    T: Handler + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("rpc::Handler")
            .field("handler", &self.handler)
            .field("run", &self.run)
            .field("is_flushed", &self.is_flushed)
            .finish()
    }
}
