use bincode::{self, deserialize, serialize};
use errors::*;
use nix::sys::socket::{CmsgSpace, ControlMessage, GetSockOpt, MsgFlags, recvmsg, sendmsg};
use nix::sys::socket::sockopt::SndBuf;
use nix::sys::uio::IoVec;
use serde::de::DeserializeOwned;
use serde::ser::Serialize;
use std::fmt::Debug;
use std::io::Read;
use std::os::unix::io::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;

pub struct Connection {
    stream: UnixStream
}

impl Connection {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream: stream
        }
    }

    pub fn receive<RT>(&mut self) -> Result<RT>
    where
        RT: DeserializeOwned + Debug,
    {
        // TODO: Check deserialize_from and serialize_into.
        let mut encoded = vec![0; 1024]; // TODO: Get max size from bincode, or at least assert.
        // TODO: Read until block, EOF, or error.
        // TODO: Switch back to recv_fd.
        match self.stream.read(&mut encoded) {
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

    // TODO: Avoid duplication with server-side version (only difference is message type + mio).
    pub fn send<ST>(&self, msg: ST) -> Result<()>
    where
        ST: Serialize + Debug,
    {
        let encoded: Vec<u8> = serialize(&msg, bincode::Infinite)?;
        // TODO: Switch back to send_fd.
        // // TODO: Pass fd with StreamCreated message, not separately OOB.
        // let msg = vec![0; 0];
        // info!("send_fd {:?}", remote_fd.as_raw_fd());
        // super::send_fd(&mut stream.stream, &msg, remote_fd.into_raw_fd()).unwrap();
        Self::send_fd(&self.stream, &encoded, None)
    }


    pub fn send_with_fd<ST>(&self, msg: ST, fd_to_send: RawFd) -> Result<()>
    where
        ST: Serialize + Debug,
    {
        let encoded: Vec<u8> = serialize(&msg, bincode::Infinite)?;
        // TODO: Switch back to send_fd.
        // // TODO: Pass fd with StreamCreated message, not separately OOB.
        // let msg = vec![0; 0];
        // info!("send_fd {:?}", remote_fd.as_raw_fd());
        // super::send_fd(&mut stream.stream, &msg, remote_fd.into_raw_fd()).unwrap();
        Self::send_fd(&self.stream, &encoded, Some(fd_to_send))
    }

    fn recv_fd<S: AsRawFd>(stream: &mut S, buf_to_recv: &mut [u8]) -> Result<(usize, Option<RawFd>)> {
        let iov = [IoVec::from_mut_slice(&mut buf_to_recv[..])];
        let mut cmsgspace: CmsgSpace<[RawFd; 1]> = CmsgSpace::new();
        let msg = recvmsg(
            stream.as_raw_fd(),
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

    fn send_fd<S: AsRawFd>(stream: &S, buf_to_send: &[u8], fd_to_send: Option<RawFd>) -> Result<()> {
        let iov = [IoVec::from_slice(buf_to_send)];

        let send_result = if fd_to_send.is_some() {
            let fds = [fd_to_send.unwrap()];
            let cmsg = ControlMessage::ScmRights(&fds);
            sendmsg(stream.as_raw_fd(), &iov, &[cmsg], MsgFlags::empty(), None).chain_err(|| "send_fd")
        } else {
            sendmsg(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None).chain_err(|| "send_fd")
        };
        match send_result {
            Ok(0) => Err(ErrorKind::Disconnected.into()),
            Ok(n) => {
                assert_eq!(n, buf_to_send.len());
                debug!("send {:?}", buf_to_send);
                Ok(())
            },
            // TODO: Handle dropped message?
            //            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => panic!("wouldblock"),
            _ => bail!("socket write"),
        }
    }
}
