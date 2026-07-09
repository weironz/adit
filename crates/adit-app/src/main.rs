// Release builds are GUI apps — suppress the console window that would otherwise
// flash on launch. Debug builds keep the console so logs remain visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> iced::Result {
    adit_ui::run()
}
