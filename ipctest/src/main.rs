// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details.
#![warn(unused_extern_crates)]
#[macro_use]
extern crate log;

use std::process::exit;

use audioipc::errors::Result;
use ipctest::TestServer;

// Run with 'RUST_LOG=run,audioipc cargo run -p ipctest'
#[cfg(unix)]
fn run(wait_for_debugger: bool) -> Result<()> {
    use audioipc::errors::Error;

    let server = TestServer::new();

    // Create a socketpair for passing the client fd after fork.
    let mut sock_fds = [0i32; 2];
    assert_eq!(
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sock_fds.as_mut_ptr()) },
        0
    );

    match unsafe { libc::fork() } {
        -1 => return Err(Error::Other("fork() failed".into())),
        0 => {
            // Child: receive client fd from parent via SCM_RIGHTS.
            unsafe { libc::close(sock_fds[1]) };
            let fd = ipctest::recv_fd(sock_fds[0]);
            unsafe { libc::close(sock_fds[0]) };

            eprintln!("AudioIPC client (pid {})", std::process::id());
            if wait_for_debugger {
                eprintln!("Waiting for debugger to attach; hit enter to continue.");
                let mut input = String::new();
                let _ = std::io::stdin().read_line(&mut input);
            }

            std::process::exit(match ipctest::client::client_test(fd) {
                Ok(()) => 0,
                Err(e) => {
                    error!("error: {e}");
                    1
                }
            });
        }
        n => {
            // Parent: create server connection with child's pid, send fd to child.
            unsafe { libc::close(sock_fds[0]) };
            let fd = server.new_client(n as u32);
            ipctest::send_fd(sock_fds[1], fd);
            unsafe {
                libc::close(fd);
                libc::close(sock_fds[1]);
            }

            let mut status: libc::c_int = 0;
            unsafe { libc::waitpid(n, &mut status, 0) };
            if libc::WIFSIGNALED(status) {
                let signum = libc::WTERMSIG(status);
                if libc::WCOREDUMP(status) {
                    return Err(Error::Other(format!(
                        "Child process {n} exited with sig {signum}. Core dumped."
                    )));
                } else {
                    return Err(Error::Other(format!(
                        "Child process {n} exited with sig {signum}."
                    )));
                }
            }
        }
    };

    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
#[cfg(windows)]
fn run(wait_for_debugger: bool) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, FALSE, HANDLE},
        System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE},
    };

    let server = TestServer::new();

    let args: Vec<String> = std::env::args().collect();

    let mut cmd = Command::new(&args[0]);
    cmd.arg("--client").stdin(Stdio::piped());
    if wait_for_debugger {
        cmd.arg("--wait-for-debugger");
    }
    let mut child = cmd.spawn().expect("child process failed");
    let child_pid = child.id();

    let fd = server.new_client(child_pid);

    // Duplicate the handle into the child process and send the value via stdin.
    let client_handle = unsafe {
        let child_process = OpenProcess(PROCESS_DUP_HANDLE, FALSE, child_pid);
        assert!(!child_process.is_null(), "OpenProcess failed");

        let mut target_handle: HANDLE = std::ptr::null_mut();
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

    child.wait().expect("child process wait failed");

    Ok(())
}

#[cfg(windows)]
fn run_client() -> Result<()> {
    use audioipc::PlatformHandleType;

    // Read the pre-duplicated handle value from stdin.
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .expect("failed to read handle from stdin");
    let handle: usize = line.trim().parse().expect("invalid handle value");

    ipctest::client::client_test(handle as PlatformHandleType)
}

fn main() {
    env_logger::init();

    let mut is_client = false;
    let mut wait_for_debugger = false;

    for arg in std::env::args() {
        if arg == "--client" {
            is_client = true;
        }
        if arg == "--wait-for-debugger" {
            wait_for_debugger = true;
        }
    }

    let result = if !is_client {
        eprintln!("AudioIPC server (pid {})", std::process::id());
        run(wait_for_debugger)
    } else {
        eprintln!("AudioIPC client (pid {})", std::process::id());
        if wait_for_debugger {
            eprintln!("Waiting for debugger to attach; hit enter to continue.");
            let mut input = String::new();
            let _ = std::io::stdin().read_line(&mut input);
        }
        #[cfg(windows)]
        {
            run_client()
        }
        #[cfg(not(windows))]
        {
            unreachable!("Unix client runs directly in forked child");
        }
    };

    if let Err(ref e) = result {
        error!("error: {e}");

        let mut source = std::error::Error::source(e);
        while let Some(err) = source {
            info!("caused by: {err}");
            source = std::error::Error::source(err);
        }

        exit(1);
    }
}
