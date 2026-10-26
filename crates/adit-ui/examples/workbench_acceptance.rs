//! Run with --features native-workbench-acceptance --example workbench_acceptance.
//! Set ADIT_CONFIG_DIR to an empty directory under OS temp before launching.
//! ADIT_WORKBENCH_INTERACTIVE=1 keeps successful runs open for local pointer/focus
//! smoke. Close the window to exit; the saved report covers only capture actions.
//! See src/workbench_acceptance.rs for capture, theme, scale and view controls.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    adit_ui::run_workbench_acceptance()
}
