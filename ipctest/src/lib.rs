// Copyright © 2026 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details.

pub mod client;

use std::os::raw::c_void;

use audioipc::PlatformHandleType;

/// RAII wrapper around the audioipc server lifecycle.
pub struct TestServer {
    handle: *mut c_void,
}

impl Default for TestServer {
    fn default() -> Self {
        Self::new()
    }
}

impl TestServer {
    pub fn new() -> Self {
        let init_params = audioipc_server::AudioIpcServerInitParams {
            thread_create_callback: None,
            thread_destroy_callback: None,
        };
        let handle = unsafe {
            audioipc_server::audioipc2_server_start(
                std::ptr::null(),
                std::ptr::null(),
                &init_params,
            )
        };
        assert!(!handle.is_null(), "audioipc2_server_start failed");
        TestServer { handle }
    }

    pub fn new_client(&self, remote_pid: u32) -> PlatformHandleType {
        let fd = audioipc_server::audioipc2_server_new_client(self.handle, remote_pid, 0);
        assert!(
            fd != audioipc::INVALID_HANDLE_VALUE,
            "audioipc2_server_new_client failed"
        );
        fd
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        audioipc_server::audioipc2_server_stop(self.handle);
    }
}

#[cfg(unix)]
pub fn send_fd(socket: i32, fd: i32) {
    unsafe {
        let iov = libc::iovec {
            iov_base: &[0u8; 1] as *const _ as *mut _,
            iov_len: 1,
        };

        let cmsg_space = libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) as usize;
        let mut cmsg_buf = vec![0u8; cmsg_space];

        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &iov as *const _ as *mut _;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
        msg.msg_controllen = cmsg_space as _;

        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as _;
        std::ptr::copy_nonoverlapping(
            &fd as *const _ as *const u8,
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<i32>(),
        );

        let result = libc::sendmsg(socket, &msg, 0);
        assert!(
            result >= 0,
            "sendmsg failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(unix)]
pub fn recv_fd(socket: i32) -> i32 {
    unsafe {
        let mut buf = [0u8; 1];
        let iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut _,
            iov_len: 1,
        };

        let cmsg_space = libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) as usize;
        let mut cmsg_buf = vec![0u8; cmsg_space];

        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &iov as *const _ as *mut _;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
        msg.msg_controllen = cmsg_space as _;

        let result = libc::recvmsg(socket, &mut msg, 0);
        assert!(
            result >= 0,
            "recvmsg failed: {}",
            std::io::Error::last_os_error()
        );

        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        assert!(!cmsg.is_null());

        let mut fd: i32 = -1;
        std::ptr::copy_nonoverlapping(
            libc::CMSG_DATA(cmsg),
            &mut fd as *mut _ as *mut u8,
            std::mem::size_of::<i32>(),
        );
        fd
    }
}
