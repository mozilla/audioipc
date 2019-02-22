// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details.

#![recursion_limit = "1024"]
#[macro_use]
extern crate error_chain;

extern crate audioipc;
extern crate audioipc_client;
extern crate audioipc_server as server;
extern crate cubeb;
extern crate env_logger;
extern crate futures;
extern crate libc;
#[macro_use]
extern crate log;
extern crate winapi;

use std::process::exit;

mod client;

mod errors {
    error_chain! {
        links {
            AudioIPC(::audioipc::errors::Error, ::audioipc::errors::ErrorKind);
            Server(::server::errors::Error, ::server::errors::ErrorKind);
        }
    }
}

use errors::*;

// Run with 'RUST_LOG=run,audioipc cargo run -p ipctest'
#[cfg(unix)]
fn run() -> Result<()> {
    let handle = server::audioipc_server_start();
    let fd = server::audioipc_server_new_client(handle);

    match unsafe { libc::fork() } {
        -1 => bail!("fork() failed"),
        0 => {
            return client::client_test(fd);
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

    server::audioipc_server_stop(handle);

    Ok(())
}

#[cfg(unix)]
fn run_client(_pid: u32, _handle: usize) -> Result<()> {
    bail!("not used on non-Windows");
}

#[cfg(windows)]
fn run() -> Result<()> {
    let handle = server::audioipc_server_start();
    let fd = server::audioipc_server_new_client(handle);

    let args: Vec<String> = std::env::args().collect();

    let mut child = std::process::Command::new(&args[0])
        .arg("--client")
        .env("PID", format!("{}", std::process::id()))
        .env("HANDLE", format!("{}", fd as usize))
        .spawn()
        .expect("child process failed");

    child.wait().expect("child process wait failed");

    server::audioipc_server_stop(handle);

    Ok(())
}

#[cfg(windows)]
fn run_client(pid: u32, handle: usize) -> Result<()> {
    use winapi::um::{processthreadsapi, winnt, handleapi, winnt::HANDLE};
    use winapi::shared::minwindef::FALSE;

    let mut target_handle = std::ptr::null_mut();
    unsafe {
        let source = processthreadsapi::OpenProcess(winnt::PROCESS_DUP_HANDLE,
                                                    FALSE,
                                                    pid);
        let target = processthreadsapi::GetCurrentProcess();

        let ok = handleapi::DuplicateHandle(source,
                                            handle as HANDLE,
                                            target,
                                            &mut target_handle,
                                            0,
                                            FALSE,
                                            winnt::DUPLICATE_SAME_ACCESS);
        if ok == FALSE {
            bail!("DuplicateHandle failed");
        }
    }

    client::client_test(target_handle)
}

fn main() {
    env_logger::init().unwrap();

    let args: Vec<String> = std::env::args().collect();

    let client = args.len() == 2 && args[1] == "--client";

    let result = if !client {
        println!("Cubeb AudioServer...");
        run()
    } else {
        let pid: u32 = std::env::var("PID").unwrap().parse().unwrap();
        let handle: usize = std::env::var("HANDLE").unwrap().parse().unwrap();
        run_client(pid, handle)
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
