//! Native compositor/input acceptance against the synthetic localhost fixture.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() -> iced::Result {
    adit_ui::run_native_vnc_acceptance()
}
