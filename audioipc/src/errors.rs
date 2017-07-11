use bincode;
use cubeb_core;
use nix;
use std;

error_chain! {
    // Maybe replace with chain_err to improve the error info.
    foreign_links {
        Bincode(bincode::Error);
        Io(std::io::Error);
        Nix(nix::Error);
        Cubeb(cubeb_core::Error);
    }

    // Replace bail!(str) with explicit errors.
    errors {
        Disconnected
    }
}
