use {RecvFd, SendFd};
use async::{Async, AsyncRecvFd};
use bytes::BytesMut;
use codec::{Decoder, encode};
use errors::*;
use mio::{Poll, PollOpt, Ready, Token};
use mio::event::Evented;
use mio::unix::EventedFd;
use serde::de::DeserializeOwned;
use serde::ser::Serialize;
use std::fmt::Debug;
use std::io::{self, Read};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net;
use std::os::unix::prelude::*;

// Because of the trait implementation rules in Rust, this needs to be
// a wrapper class to allow implementation of a trait from another
// crate on a struct from yet another crate.
//
// This class is effectively net::UnixStream.

#[derive(Debug)]
pub struct Connection {
    stream: net::UnixStream,
    recv_buffer: BytesMut,
    send_buffer: BytesMut,
    decoder: Decoder
}

impl Connection {
    pub fn new(stream: net::UnixStream) -> Connection {
        info!("Create new connection");
        stream.set_nonblocking(false).unwrap();
        Connection {
            stream: stream,
            recv_buffer: BytesMut::with_capacity(32 * 1024),
            send_buffer: BytesMut::with_capacity(32 * 1024),
            decoder: Decoder::new()
        }
    }

    /// Creates an unnamed pair of connected sockets.
    ///
    /// Returns two `Connection`s which are connected to each other.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use audioipc::Connection;
    ///
    /// let (conn1, conn2) = match Connection::pair() {
    ///     Ok((conn1, conn2)) => (conn1, conn2),
    ///     Err(e) => {
    ///         println!("Couldn't create a pair of connections: {:?}", e);
    ///         return
    ///     }
    /// };
    /// ```
    pub fn pair() -> io::Result<(Connection, Connection)> {
        let (s1, s2) = net::UnixStream::pair()?;
        Ok((Connection::new(s1), Connection::new(s2)))
    }

    pub fn receive<RT>(&mut self) -> Result<RT>
    where
        RT: DeserializeOwned + Debug,
    {
        match self.receive_with_fd() {
            Ok((r, None)) => Ok(r),
            Ok((_, Some(_))) => panic!("unexpected fd received"),
            Err(e) => Err(e),
        }
    }

    pub fn receive_with_fd<RT>(&mut self) -> Result<(RT, Option<RawFd>)>
    where
        RT: DeserializeOwned + Debug,
    {
        self.recv_buffer.reserve(32 * 1024);

        // TODO: Read until block, EOF, or error.
        // TODO: Switch back to recv_fd.
        match self.stream.recv_buf_fd(&mut self.recv_buffer) {
            Ok(Async::Ready((0, _))) => Err(ErrorKind::Disconnected.into()),
            // TODO: Handle partial read?
            Ok(Async::Ready((_, fd))) => {
                let r = self.decoder.decode(&mut self.recv_buffer);
                debug!("receive {:?}", r);
                match r {
                    Ok(r) => Ok((r.unwrap(), fd)),
                    Err(e) => Err(e).chain_err(|| "Failed to deserialize message"),
                }
            },
            Ok(Async::NotReady) => bail!("Socket should be blocking."),
            // TODO: Handle dropped message.
            // Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => panic!("wouldblock"),
            _ => bail!("socket write"),
        }
    }

    pub fn send<ST>(&mut self, msg: ST) -> Result<usize>
    where
        ST: Serialize + Debug,
    {
        self.send_with_fd::<ST, Connection>(msg, None)
    }

    pub fn send_with_fd<ST, FD>(&mut self, msg: ST, fd_to_send: Option<FD>) -> Result<usize>
    where
        ST: Serialize + Debug,
        FD: IntoRawFd + Debug,
    {
        info!("send_with_fd {:?}, {:?}", msg, fd_to_send);
        try!(encode(&mut self.send_buffer, &msg));
        let fd_to_send = fd_to_send.map(|fd| fd.into_raw_fd());
        let send = self.send_buffer.take().freeze();
        self.stream.send_fd(send.as_ref(), fd_to_send).chain_err(
            || "Failed to send message with fd"
        )
    }
}

impl Evented for Connection {
    fn register(&self, poll: &Poll, token: Token, events: Ready, opts: PollOpt) -> io::Result<()> {
        EventedFd(&self.stream.as_raw_fd()).register(poll, token, events, opts)
    }

    fn reregister(&self, poll: &Poll, token: Token, events: Ready, opts: PollOpt) -> io::Result<()> {
        EventedFd(&self.stream.as_raw_fd()).reregister(poll, token, events, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        EventedFd(&self.stream.as_raw_fd()).deregister(poll)
    }
}

impl Read for Connection {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream.read(bytes)
    }
}

// TODO: Is this required?
impl<'a> Read for &'a Connection {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        (&self.stream).read(bytes)
    }
}

impl RecvFd for Connection {
    fn recv_fd(&mut self, buf_to_recv: &mut [u8]) -> io::Result<(usize, Option<RawFd>)> {
        self.stream.recv_fd(buf_to_recv)
    }
}

impl FromRawFd for Connection {
    unsafe fn from_raw_fd(fd: RawFd) -> Connection {
        Connection::new(net::UnixStream::from_raw_fd(fd))
    }
}

impl IntoRawFd for Connection {
    fn into_raw_fd(self) -> RawFd {
        self.stream.into_raw_fd()
    }
}

impl SendFd for Connection {
    fn send_fd(&mut self, buf_to_send: &[u8], fd_to_send: Option<RawFd>) -> io::Result<usize> {
        self.stream.send_fd(buf_to_send, fd_to_send)
    }
}
