//! Native GUI acceptance for #92: the server's pointer shape, the hidden
//! local pointer, and the restored local pointer.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() -> iced::Result {
    adit_ui::run_native_rdp_acceptance()
}
