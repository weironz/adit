//! Feature-only access to the actual library hook for minimal GUI comparisons.
pub fn arm() -> bool {
    super::keyboard_hook::arm_observer()
}

pub fn input_tag() -> usize {
    super::keyboard_hook::probe_input_tag()
}

pub fn snapshot() -> serde_json::Value {
    let (armed, vnc, counters) = super::keyboard_hook::diagnostics();
    let (turns, exit_reason) = super::keyboard_hook::worker_health();
    serde_json::json!({
        "armed":armed, "vnc":vnc, "counters":counters,
        "probe_events":super::keyboard_hook::probe_events(),
        "worker_thread":super::keyboard_hook::worker_thread_id(),
        "pump_turns":turns, "pump_exit":exit_reason,
    })
}

pub fn disarm() {
    super::keyboard_hook::disarm();
}

pub fn raw_input_snapshot() -> serde_json::Value {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::Input::*;
        let mut devices = [RAWINPUTDEVICE::default(); 64];
        let mut count = devices.len() as u32;
        let got = GetRegisteredRawInputDevices(
            Some(devices.as_mut_ptr()),
            &mut count,
            size_of::<RAWINPUTDEVICE>() as u32,
        );
        if got == u32::MAX || got as usize > devices.len() {
            return serde_json::json!({"read":false});
        }
        let mut devices = devices[..got as usize].to_vec();
        devices.sort_by_key(|d| (d.usUsagePage, d.usUsage));
        serde_json::json!({"read":true,"devices":devices.iter().map(|d|
            serde_json::json!({"page":d.usUsagePage,"usage":d.usUsage,"flags":d.dwFlags.0,
                "target":d.hwndTarget.0 as usize})).collect::<Vec<_>>()})
    }
    #[cfg(not(windows))]
    serde_json::json!({"read":false})
}
