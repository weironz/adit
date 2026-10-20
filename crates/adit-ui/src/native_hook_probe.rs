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
