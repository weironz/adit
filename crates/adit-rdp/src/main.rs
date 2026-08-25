//! The `adit-rdp-host` helper executable.
//!
//! Adit spawns this per RDP session and drives it over stdin/stdout (see
//! [`adit_rdp_proto`]). It exists as a separate process because IronRDP can't
//! share a Cargo.lock with the main app's `russh` (conflicting RustCrypto pins).

// MSVC prints "Creating library …adit_rdp_host.lib and object …exp" on stdout
// whenever the executable exports a symbol — here from a dependency, since this
// crate declares none itself. It is a status line, not a diagnostic, and there
// is nothing in our code to change in response to it.
//
// `linker_messages` (on by default since 1.98) surfaces linker stdout wholesale,
// so it cannot tell that apart from a real linker complaint. Allowed on this
// target only: a genuine linker problem in the app crate would still be shown.
#![allow(linker_messages)]

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
