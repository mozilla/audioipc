// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details.
#![warn(unused_extern_crates)]
#![recursion_limit = "1024"]
#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate log;

use std::process::exit;

mod client;

mod errors {
    #![allow(clippy::upper_case_acronyms)]
    error_chain! {
        links {
            AudioIPC(::audioipc::errors::Error, ::audioipc::errors::ErrorKind);
            Server(::audioipc_server::errors::Error, ::audioipc_server::errors::ErrorKind);
        }
    }
}

use crate::errors::*;

// Run with 'RUST_LOG=run,audioipc cargo run -p ipctest'
#[cfg(unix)]
fn run(wait_for_debugger: bool) -> Result<()> {
    use std::ffi::CString;
    let init_params = audioipc_server::AudioIpcServerInitParams {
        thread_create_callback: None,
        thread_destroy_callback: None,
    };
    let handle = unsafe {
        audioipc_server::audioipc2_server_start(std::ptr::null(), std::ptr::null(), &init_params)
    };
    let fd = audioipc_server::audioipc2_server_new_client(handle, 0);
    let fd = unsafe {
        let new_fd = libc::dup(fd);
        libc::close(fd);
        new_fd
    };
    assert!(fd > audioipc::INVALID_HANDLE_VALUE);

    let args: Vec<String> = std::env::args().collect();

    match unsafe { libc::fork() } {
        -1 => bail!("fork() failed"),
        0 => {
            let self_path = CString::new(&*args[0]).unwrap();
            let child_arg1 = CString::new("--client").unwrap();
            let child_arg2 = CString::new("--fd").unwrap();
            let child_arg3 = CString::new(format!("{}", fd)).unwrap();
            let child_arg4 = if wait_for_debugger {
                CString::new("--wait-for-debugger").unwrap()
            } else {
                CString::new("").unwrap()
            };
            let child_args = [
                self_path.as_ptr(),
                child_arg1.as_ptr(),
                child_arg2.as_ptr(),
                child_arg3.as_ptr(),
                child_arg4.as_ptr(),
                std::ptr::null(),
            ];
            let r = unsafe { libc::execv(self_path.as_ptr(), &child_args as *const _) };
            assert_eq!(r, 0);
        }
        n => unsafe {
            let mut status: libc::c_int = 0;
            libc::waitpid(n, &mut status, 0);
            if libc::WIFSIGNALED(status) {
                let signum = libc::WTERMSIG(status);
                if libc::WCOREDUMP(status) {
                    bail!(
                        "Child process {} exited with sig {}. Core dumped.",
                        n,
                        signum
                    );
                } else {
                    bail!("Child process {} exited with sig {}.", n, signum);
                }
            }
        },
    };

    audioipc_server::audioipc2_server_stop(handle);

    Ok(())
}

#[cfg(unix)]
fn run_client() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(args[2], "--fd");
    let target_fd: i32 = args[3].parse().unwrap();
    client::client_test(target_fd)
}

#[allow(clippy::unnecessary_wraps)]
#[cfg(windows)]
fn run(wait_for_debugger: bool) -> Result<()> {
    let init_params = audioipc_server::AudioIpcServerInitParams {
        thread_create_callback: None,
        thread_destroy_callback: None,
    };
    let handle = unsafe {
        audioipc_server::audioipc2_server_start(std::ptr::null(), std::ptr::null(), &init_params)
    };
    let fd = audioipc_server::audioipc2_server_new_client(handle, 0);

    let args: Vec<String> = std::env::args().collect();

    let mut cmd = std::process::Command::new(&args[0]);
    cmd.env("AUDIOIPC_PID", format!("{}", std::process::id()))
        .env("AUDIOIPC_HANDLE", format!("{}", fd as usize))
        .arg("--client");
    if wait_for_debugger {
        cmd.arg("--wait-for-debugger");
    }
    let mut child = cmd.spawn().expect("child process failed");

    child.wait().expect("child process wait failed");

    audioipc_server::audioipc2_server_stop(handle);

    Ok(())
}

#[cfg(windows)]
fn run_client() -> Result<()> {
    use winapi::shared::minwindef::FALSE;
    use winapi::um::{handleapi, processthreadsapi, winnt, winnt::HANDLE};

    let pid: u32 = std::env::var("AUDIOIPC_PID").unwrap().parse().unwrap();
    let handle: usize = std::env::var("AUDIOIPC_HANDLE").unwrap().parse().unwrap();

    let mut target_handle = std::ptr::null_mut();
    unsafe {
        let source = processthreadsapi::OpenProcess(winnt::PROCESS_DUP_HANDLE, FALSE, pid);
        let target = processthreadsapi::GetCurrentProcess();

        let ok = handleapi::DuplicateHandle(
            source,
            handle as HANDLE,
            target,
            &mut target_handle,
            0,
            FALSE,
            winnt::DUPLICATE_SAME_ACCESS,
        );
        handleapi::CloseHandle(source);
        if ok == FALSE {
            bail!("DuplicateHandle failed");
        }
    }

    client::client_test(target_handle)
}

fn main() {
    env_logger::init();

    let mut client = false;
    let mut wait_for_debugger = false;

    for arg in std::env::args() {
        if arg == "--client" {
            client = true;
        }
        if arg == "--wait-for-debugger" {
            wait_for_debugger = true;
        }
    }

    let result = if !client {
        eprintln!("AudioIPC server (pid {})", std::process::id());
        run(wait_for_debugger)
    } else {
        eprintln!("AudioIPC client (pid {})", std::process::id());
        if wait_for_debugger {
            eprintln!("Waiting for debugger to attach; hit enter to continue.");
            let mut input = String::new();
            let _ = std::io::stdin().read_line(&mut input);
        }
        run_client()
    };

    if let Err(ref e) = result {
        error!("error: {}", e);

        for e in e.iter().skip(1) {
            info!("caused by: {}", e);
        }

        // Requires RUST_BACKTRACE=1 in the environment.
        if let Some(backtrace) = e.backtrace() {
            info!("backtrace: {:?}", backtrace);
        }

        exit(1);
    }
}
