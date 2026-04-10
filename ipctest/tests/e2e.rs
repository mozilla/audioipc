// Copyright © 2026 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details.

/// Multi-process end-to-end test: start server, spawn self as a child client
/// process, pass the IPC handle via SCM_RIGHTS over a socketpair, and run
/// client_test in the child.
#[cfg(unix)]
#[test]
fn multi_process_client_test() {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    // Child path: we've been re-spawned; the socketpair fd is in env var.
    if let Ok(fd_str) = std::env::var("AUDIOIPC_TEST_CLIENT_FD") {
        let sock_fd: i32 = fd_str.parse().expect("invalid fd");
        let fd = ipctest::recv_fd(sock_fd);
        unsafe { libc::close(sock_fd) };
        std::process::exit(match ipctest::client::client_test(fd) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("client_test failed: {e}");
                1
            }
        });
    }

    let _ = env_logger::try_init();

    let server = ipctest::TestServer::new();

    let mut sock_fds = [0i32; 2];
    assert_eq!(
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sock_fds.as_mut_ptr()) },
        0
    );
    let (parent_fd, child_fd) = (sock_fds[0], sock_fds[1]);

    // Spawn self as a child, running just this test with the fd env var set.
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.env("AUDIOIPC_TEST_CLIENT_FD", child_fd.to_string())
        .arg("--exact")
        .arg("multi_process_client_test")
        .arg("--nocapture");
    // Clear FD_CLOEXEC on child_fd in the post-fork/pre-exec child so the
    // socketpair fd survives exec. pre_exec only mutates the child's FD table.
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(child_fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(child_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("child process failed");
    let child_pid = child.id();

    // Parent doesn't need its copy of the child-side fd anymore.
    unsafe { libc::close(child_fd) };

    let fd = server.new_client(child_pid);
    ipctest::send_fd(parent_fd, fd);
    unsafe {
        libc::close(fd);
        libc::close(parent_fd);
    }

    let status = child.wait().expect("child process wait failed");
    assert!(status.success(), "child exited with: {}", status);
}

/// Multi-process end-to-end test for Windows: spawn self as a child process,
/// duplicate the IPC handle into the child, and send the handle value via stdin.
#[cfg(windows)]
#[test]
fn multi_process_client_test() {
    use audioipc::PlatformHandleType;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, FALSE, HANDLE},
        System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE},
    };

    // Child path: if this env var is set, we're the spawned child.
    if std::env::var("AUDIOIPC_TEST_CLIENT").is_ok() {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .expect("failed to read handle from stdin");
        let handle: usize = line.trim().parse().expect("invalid handle value");
        ipctest::client::client_test(handle as PlatformHandleType)
            .expect("client_test failed in child");
        return;
    }

    let _ = env_logger::try_init();

    let server = ipctest::TestServer::new();

    // Spawn self as a child, running just this test with the client env var set.
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.env("AUDIOIPC_TEST_CLIENT", "1")
        .arg("--exact")
        .arg("multi_process_client_test")
        .arg("--nocapture")
        .stdin(Stdio::piped());
    let mut child = cmd.spawn().expect("child process failed");
    let child_pid = child.id();

    let fd = server.new_client(child_pid);

    let client_handle = unsafe {
        let child_process = OpenProcess(PROCESS_DUP_HANDLE, FALSE, child_pid);
        assert!(child_process != 0, "OpenProcess failed");

        let mut target_handle: HANDLE = 0;
        let ok = DuplicateHandle(
            GetCurrentProcess(),
            fd as HANDLE,
            child_process,
            &mut target_handle,
            0,
            FALSE,
            DUPLICATE_SAME_ACCESS,
        );
        CloseHandle(child_process);
        assert!(ok != FALSE, "DuplicateHandle failed");
        target_handle
    };

    writeln!(child.stdin.take().unwrap(), "{}", client_handle as usize)
        .expect("failed to send handle to child");

    let status = child.wait().expect("child process wait failed");
    assert!(status.success(), "child exited with: {}", status);
}
