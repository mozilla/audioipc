// Copyright © 2026 Mozilla Foundation
// SPDX-License-Identifier: ISC

use super::test::RejectDelivery;
use super::*;
use crate::messages::{ClientMessage, SerializableHandle, ServerMessage, StreamCreate};
use crate::PlatformHandle;
use std::fs::File;
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct StreamReplies {
    produced: Arc<AtomicUsize>,
    total: usize,
}

impl Handler for StreamReplies {
    type In = ServerMessage;
    type Out = ClientMessage;

    fn consume(&mut self, _: ServerMessage) -> Result<()> {
        unreachable!();
    }

    fn produce(&mut self) -> Result<Option<ClientMessage>> {
        let token = self.produced.load(Ordering::Relaxed);
        if token == self.total {
            return Ok(None);
        }
        self.produced.store(token + 1, Ordering::Relaxed);
        Ok(Some(ClientMessage::StreamCreated(StreamCreate {
            token,
            shm_handle: SerializableHandle::new(PlatformHandle::from(File::open("/dev/null")?), 0),
            shm_area_size: 0,
        })))
    }
}

struct CollectReplies(Arc<Mutex<Vec<usize>>>);

impl Handler for CollectReplies {
    type In = ClientMessage;
    type Out = ServerMessage;

    fn consume(&mut self, message: ClientMessage) -> Result<()> {
        match message {
            ClientMessage::StreamCreated(mut data) => {
                let handle = data.shm_handle.take_handle();
                let file = unsafe { File::from_raw_fd(handle.into_raw()) };
                file.metadata()?;
                self.0.lock().unwrap().push(data.token);
                Ok(())
            }
            _ => unreachable!(),
        }
    }

    fn produce(&mut self) -> Result<Option<ServerMessage>> {
        Ok(None)
    }
}

#[test]
fn scm_rights_pipelined_replies_survive_partial_send_and_backpressure() {
    let poll = mio::Poll::new().unwrap();
    let (server_pipe, client_handle) = sys::make_pipe_pair().unwrap();
    let client_pipe = unsafe { sys::Pipe::from_raw_handle(client_handle) };
    let produced = Arc::new(AtomicUsize::new(0));
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut sender = Connection::new(
        server_pipe,
        Token(1),
        Box::new(FramedDriver::new(StreamReplies {
            produced: produced.clone(),
            total: 64,
        })),
        poll.registry(),
    )
    .unwrap();
    let mut receiver = Connection::new(
        client_pipe,
        Token(2),
        Box::new(FramedDriver::new(CollectReplies(received.clone()))),
        poll.registry(),
    )
    .unwrap();

    // Send one byte with the first frame's descriptor, then report WouldBlock.
    // A later wake must finish this frame before taking another response.
    sender.io.partial_send_for_test(1);
    assert!(!sender.send_outbound(poll.registry()).unwrap());
    assert_eq!(produced.load(Ordering::Relaxed), 1);
    assert!(!sender.outbound.is_empty());
    assert!(!receiver.recv_inbound().unwrap());
    assert!(received.lock().unwrap().is_empty());

    for _ in 0..128 {
        assert!(!sender.send_outbound(poll.registry()).unwrap());
        assert!(!receiver.recv_inbound().unwrap());
        if received.lock().unwrap().len() == 64 {
            break;
        }
    }
    assert_eq!(*received.lock().unwrap(), (0..64).collect::<Vec<_>>());
    assert!(sender.outbound.is_empty());
}

fn rejection_connection<T>() -> (Connection, sys::Pipe, mio::Poll)
where
    T: DeserializeOwned + Debug + AssociateHandleForMessage + Send + 'static,
{
    let poll = mio::Poll::new().unwrap();
    let (receiver, peer_handle) = sys::make_pipe_pair().unwrap();
    let peer = unsafe { sys::Pipe::from_raw_handle(peer_handle) };
    let connection = Connection::new(
        receiver,
        Token(1),
        Box::new(FramedDriver::new(RejectDelivery::<T>(
            std::marker::PhantomData,
        ))),
        poll.registry(),
    )
    .unwrap();
    (connection, peer, poll)
}

fn attach_descriptor(outbound: &mut sys::ConnectionBuffer) {
    outbound
        .push_handle(unsafe {
            crate::messages::RemoteHandle::new(
                PlatformHandle::from(File::open("/dev/null").unwrap()).into_raw(),
                0,
            )
        })
        .unwrap();
}

fn assert_handle_free_protocol_rejects_partial_frame<T>()
where
    T: DeserializeOwned + Debug + AssociateHandleForMessage + Send + 'static,
{
    let (mut receiver, mut peer, _poll) = rejection_connection::<T>();
    let mut outbound = sys::ConnectionBuffer::with_capacity(128);
    // One byte cannot complete a header; the descriptor must be rejected now.
    outbound.buf.extend_from_slice(b"x");
    attach_descriptor(&mut outbound);
    assert_eq!(peer.send_msg(&mut outbound).unwrap(), 1);
    let error = receiver.recv_inbound().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn scm_rights_handle_free_protocols_reject_descriptors_on_partial_frames() {
    use crate::messages::{CallbackReq, CallbackResp, DeviceCollectionReq, DeviceCollectionResp};

    assert_handle_free_protocol_rejects_partial_frame::<ServerMessage>();
    assert_handle_free_protocol_rejects_partial_frame::<CallbackReq>();
    assert_handle_free_protocol_rejects_partial_frame::<CallbackResp>();
    assert_handle_free_protocol_rejects_partial_frame::<DeviceCollectionReq>();
    assert_handle_free_protocol_rejects_partial_frame::<DeviceCollectionResp>();
}

#[test]
fn scm_rights_descriptor_on_handle_free_frame_is_rejected_before_delivery() {
    let (mut receiver, mut peer, _poll) = rejection_connection::<ClientMessage>();
    let mut outbound = sys::ConnectionBuffer::with_capacity(128);
    let mut codec = LengthDelimitedCodec::<ClientMessage, ClientMessage>::default();
    codec
        .encode(ClientMessage::ClientConnected, &mut outbound.buf)
        .unwrap();
    attach_descriptor(&mut outbound);
    peer.send_msg(&mut outbound).unwrap();
    let error = receiver.recv_inbound().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn scm_rights_extra_descriptor_is_rejected_before_stream_reply_delivery() {
    let (mut receiver, mut peer, _poll) = rejection_connection::<ClientMessage>();
    let mut outbound = sys::ConnectionBuffer::with_capacity(128);
    let mut codec = LengthDelimitedCodec::<ClientMessage, ClientMessage>::default();
    codec
        .encode(
            ClientMessage::StreamCreated(StreamCreate {
                token: 0,
                shm_handle: SerializableHandle::Empty,
                shm_area_size: 0,
            }),
            &mut outbound.buf,
        )
        .unwrap();
    attach_descriptor(&mut outbound);
    attach_descriptor(&mut outbound);
    peer.send_msg(&mut outbound).unwrap();
    let error = receiver.recv_inbound().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

// Darwin can deliver SCM_RIGHTS even when sendmsg carries no ordinary bytes.
#[cfg(target_os = "macos")]
#[test]
fn scm_rights_zero_byte_transfer_is_rejected_without_waiting_for_output() {
    let (mut receiver, mut peer, _poll) = rejection_connection::<ServerMessage>();
    let mut outbound = sys::ConnectionBuffer::with_capacity(128);
    outbound.buf.extend_from_slice(b"x");
    attach_descriptor(&mut outbound);
    peer.partial_send_for_test(0);
    assert_eq!(peer.send_msg(&mut outbound).unwrap(), 0);
    let error = receiver.recv_inbound().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}
