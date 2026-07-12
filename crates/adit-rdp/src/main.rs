//! The `adit-rdp-host` helper executable.
//!
//! Adit spawns this per RDP session and drives it over stdin/stdout (see
//! [`adit_rdp_proto`]). It exists as a separate process because IronRDP can't
//! share a Cargo.lock with the main app's `russh` (conflicting RustCrypto pins).

use std::process::ExitCode;

fn main() -> ExitCode {
    match adit_rdp::run_host() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // stdout is the framed protocol; diagnostics go to stderr only.
            eprintln!("adit-rdp-host: {error}");
            ExitCode::FAILURE
        }
    }
}
