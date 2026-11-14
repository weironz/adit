//! Native pointer-coordinate acceptance for #233.
//!
//! The password is read from stdin, never from argv: argv is readable through
//! the process list by anything on the machine.
//!
//! The host comes from `ADIT_RDP_FIXTURE_HOST` (`host:port`) and the user
//! name from `ADIT_RDP_FIXTURE_USER`.
fn main() -> iced::Result {
    adit_ui::run_native_rdp_pointer_acceptance()
}
