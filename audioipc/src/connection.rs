use bincode::{self, deserialize, serialize};
use errors::*;
use mio::{Poll, PollOpt, Ready, Token};
use mio::event::Evented;
use mio::unix::EventedFd;
use nix::Error;
use nix::sys::socket::{CmsgSpace, ControlMessage, MsgFlags, recvmsg, sendmsg};
use nix::sys::uio::IoVec;
use serde::de::DeserializeOwned;
use serde::ser::Serialize;
use std::fmt::Debug;
use std::io::{self, Read};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net;
use std::os::unix::prelude::*;

pub trait RecvFd {
    fn recv_fd(&mut self, bytes: &mut [u8]) -> io::Result<(usize, Option<RawFd>)>;
}

pub trait SendFd {
    fn send_fd(&mut self, bytes: &[u8], fd: Option<RawFd>) -> io::Result<(usize)>;
}

// Because of the trait implementation rules in Rust, this needs to be
// a wrapper class to allow implementation of a trait from another
// crate on a struct from yet another crate.
//
// This class is effectively mds_uds::UnixStream.

#[derive(Debug)]
pub struct Connection {
    stream: net::UnixStream
}

impl Connection {
    pub fn new(stream: net::UnixStream) -> Connection {
        info!("Create new connection");
        Connection {
            stream: stream
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
        Ok((
            Connection {
                stream: s1
            },
            Connection {
                stream: s2
            }
        ))
    }

    pub fn receive<RT>(&mut self) -> Result<RT>
    where
        RT: DeserializeOwned + Debug,
    {
        match self.receive_with_fd() {
            // TODO: Just dropping any received fd on the floor.  Make it an error?
            Ok((r, _)) => Ok(r),
            Err(e) => Err(e),
        }
    }

    pub fn receive_with_fd<RT>(&mut self) -> Result<(RT, Option<RawFd>)>
    where
        RT: DeserializeOwned + Debug,
    {
        // TODO: Check deserialize_from and serialize_into.
        let mut encoded = vec![0; 32 * 1024]; // TODO: Get max size from bincode, or at least assert.
        // TODO: Read until block, EOF, or error.
        // TODO: Switch back to recv_fd.
        match self.stream.recv_fd(&mut encoded) {
            Ok((0, _)) => Err(ErrorKind::Disconnected.into()),
            // TODO: Handle partial read?
            Ok((n, fd)) => {
                let r = deserialize(&encoded[..n]);
                debug!("receive {:?}", r);
                match r {
                    Ok(r) => Ok((r, fd)),
                    Err(e) => Err(e).chain_err(|| "Failed to deserialize message"),
                }
            },
            // TODO: Handle dropped message.
            // Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => panic!("wouldblock"),
            _ => bail!("socket write"),
        }
    }

    pub fn send<ST>(&mut self, msg: ST) -> Result<usize>
    where
        ST: Serialize + Debug,
    {
        self.send_with_fd(msg, None)
    }

    pub fn send_with_fd<ST, FD>(&mut self, msg: ST, fd_to_send: FD) -> Result<usize>
    where
        ST: Serialize + Debug,
        FD: Into<Option<RawFd>>,
    {
        let encoded: Vec<u8> = serialize(&msg, bincode::Infinite)?;
        // TODO: Switch back to send_fd.
        // TODO: Pass fd with StreamCreated message, not separately OOB.
        // let msg = vec![0; 0];
        let fd = fd_to_send.into();
        info!("send_with_fd {:?}, {:?}", msg, fd);
        // super::send_fd(&mut stream.stream, &msg, remote_fd.into_raw_fd()).unwrap();
        self.stream.send_fd(&encoded, fd).chain_err(
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

impl RecvFd for net::UnixStream {
    fn recv_fd(&mut self, buf_to_recv: &mut [u8]) -> io::Result<(usize, Option<RawFd>)> {
        let iov = [IoVec::from_mut_slice(&mut buf_to_recv[..])];
        let mut cmsgspace: CmsgSpace<[RawFd; 1]> = CmsgSpace::new();
        let msg = recvmsg(
            self.as_raw_fd(),
            &iov,
            Some(&mut cmsgspace),
            MsgFlags::empty()
        )?;
        let mut fd = None;
        for cmsg in msg.cmsgs() {
            if let ControlMessage::ScmRights(fds) = cmsg {
                if fds.len() == 1 {
                    fd = Some(fds[0]);
                    break;
                }
            }
        }
        Ok((msg.bytes, fd))
    }
}

impl RecvFd for Connection {
    fn recv_fd(&mut self, buf_to_recv: &mut [u8]) -> io::Result<(usize, Option<RawFd>)> {
        self.stream.recv_fd(buf_to_recv)
    }
}

impl FromRawFd for Connection {
    unsafe fn from_raw_fd(fd: RawFd) -> Connection {
        Connection {
            stream: net::UnixStream::from_raw_fd(fd)
        }
    }
}

impl IntoRawFd for Connection {
    fn into_raw_fd(self) -> RawFd {
        self.stream.into_raw_fd()
    }
}

impl SendFd for net::UnixStream {
    fn send_fd(&mut self, buf_to_send: &[u8], fd_to_send: Option<RawFd>) -> io::Result<usize> {
        let iov = [IoVec::from_slice(buf_to_send)];

        let send_result = if fd_to_send.is_some() {
            let fds = [fd_to_send.unwrap()];
            let cmsg = ControlMessage::ScmRights(&fds);
            sendmsg(self.as_raw_fd(), &iov, &[cmsg], MsgFlags::empty(), None)
        } else {
            sendmsg(self.as_raw_fd(), &iov, &[], MsgFlags::empty(), None)
        };
        match send_result {
            Ok(n) => Ok(n),
            Err(Error::Sys(errno)) => Err(io::Error::from_raw_os_error(errno as _)),
            Err(_) => unreachable!(),
        }
    }
}

impl SendFd for Connection {
    fn send_fd(&mut self, buf_to_send: &[u8], fd_to_send: Option<RawFd>) -> io::Result<usize> {
        self.stream.send_fd(buf_to_send, fd_to_send)
    }
}
