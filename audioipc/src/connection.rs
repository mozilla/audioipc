use bincode::{self, deserialize, serialize};
use errors::*;
use mio::{Poll, PollOpt, Ready, Token};
use mio::event::Evented;
use mio::unix::EventedFd;
use mio_uds;
use nix::Error;
use nix::sys::socket::{CmsgSpace, ControlMessage, MsgFlags, recvmsg, sendmsg};
use nix::sys::socket::sockopt::SndBuf;
use nix::sys::uio::IoVec;
use serde::de::DeserializeOwned;
use serde::ser::Serialize;
use std::fmt::Debug;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net;

pub trait RecvFd {
    fn recv_fd(&mut self, bytes: &mut [u8]) -> io::Result<(usize, Option<RawFd>)>;
}

pub trait SendFd {
    fn send_fd(&mut self, bytes: &[u8], fd: Option<RawFd>) -> io::Result<(usize)>;
}

pub struct Connection {
    stream: net::UnixStream
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

impl SendFd for mio_uds::UnixStream {
    fn send_fd(&mut self, buf_to_send: &[u8], fd_to_send: Option<RawFd>) -> io::Result<usize> {
        self.inner.send_fd(buf_to_send, fd_to_send)
    }
}

pub fn receive<S, RT>(stream: &mut S) -> Result<RT>
where
    S: Read,
    RT: DeserializeOwned + Debug,
{
    // TODO: Check deserialize_from and serialize_into.
    let mut encoded = vec![0; 1024]; // TODO: Get max size from bincode, or at least assert.
    // TODO: Read until block, EOF, or error.
    // TODO: Switch back to recv_fd.
    match stream.read(&mut encoded) {
        Ok(0) => Err(ErrorKind::Disconnected.into()),
        // TODO: Handle partial read?
        Ok(n) => {
            let r = deserialize(&encoded[..n]);
            debug!("receive {:?}", r);
            Ok(r?)
        },
        // TODO: Handle dropped message.
        // Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => panic!("wouldblock"),
        _ => bail!("socket write"),
    }
}

pub fn receive_with_fd<S, RT>(stream: &mut S) -> Result<(RT, Option<RawFd>)>
where
    S: RecvFd,
    RT: DeserializeOwned + Debug,
{
    // TODO: Check deserialize_from and serialize_into.
    let mut encoded = vec![0; 1024]; // TODO: Get max size from bincode, or at least assert.
    // TODO: Read until block, EOF, or error.
    // TODO: Switch back to recv_fd.
    match stream.recv_fd(&mut encoded) {
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

pub fn send_with_fd<S, ST, FD>(stream: &mut S, msg: ST, fd_to_send: FD) -> Result<usize>
where
    S: SendFd,
    ST: Serialize + Debug,
    FD: Into<Option<RawFd>>,
{
    let encoded: Vec<u8> = serialize(&msg, bincode::Infinite)?;
    // TODO: Switch back to send_fd.
    // // TODO: Pass fd with StreamCreated message, not separately OOB.
    // let msg = vec![0; 0];
    // info!("send_fd {:?}", remote_fd.as_raw_fd());
    // super::send_fd(&mut stream.stream, &msg, remote_fd.into_raw_fd()).unwrap();
    stream.send_fd(&encoded, fd_to_send.into()).chain_err(
        || "Failed to send message with fd"
    )
}
