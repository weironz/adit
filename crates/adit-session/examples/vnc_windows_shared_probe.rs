//! Disposable Windows CI only: two production clients, consumed clicks and fresh RGB.
//! Public fixture password is read from stdin. No screenshots or pixel payloads are logged.
use adit_domain::{ConnectionProfile, Protocol, SessionId, SessionStatus, VncQuality, VncSecurity};
use adit_session::{RdpFrame, RdpInput, RdpMouseButton, SessionManager};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

type Check<T> = Result<T, &'static str>;

fn emit(value: Value) {
    println!("{value}");
    let _ = io::stdout().flush();
}

struct Config {
    port: u16,
    dir: PathBuf,
    target_pid: u64,
    duration: Duration,
    idle: Duration,
}

impl Config {
    fn read() -> Check<Self> {
        if !cfg!(windows)
            || std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true")
            || std::env::var("RUNNER_ENVIRONMENT").as_deref() != Ok("github-hosted")
            || std::env::var("RUNNER_OS").as_deref() != Ok("Windows")
            || !std::env::var("RUNNER_NAME").is_ok_and(|name| !name.is_empty())
        {
            return Err("disposable_windows_hosted_runner_required");
        }
        let (mut port, mut dir, mut target_pid) = (None, None, None);
        let (mut duration, mut idle) = (7200, 120);
        for arg in std::env::args().skip(1) {
            if let Some(value) = arg.strip_prefix("--port=") {
                port = Some(value.parse::<u16>().map_err(|_| "invalid_port")?);
            } else if let Some(value) = arg.strip_prefix("--state-dir=") {
                dir = Some(PathBuf::from(value));
            } else if let Some(value) = arg.strip_prefix("--target-pid=") {
                target_pid = Some(value.parse::<u64>().map_err(|_| "invalid_target_pid")?);
            } else if let Some(value) = arg.strip_prefix("--duration-seconds=") {
                duration = value.parse().map_err(|_| "invalid_duration")?;
            } else if let Some(value) = arg.strip_prefix("--idle-seconds=") {
                idle = value.parse().map_err(|_| "invalid_idle")?;
            } else {
                return Err("unknown_argument");
            }
        }
        if !(1..=28_800).contains(&duration) || !(1..=600).contains(&idle) {
            return Err("duration_or_idle_out_of_range");
        }
        let dir = dir.ok_or("state_dir_required")?;
        if !dir.is_absolute() {
            return Err("absolute_state_dir_required");
        }
        let dir = dir.canonicalize().map_err(|_| "state_dir_not_found")?;
        let temp = PathBuf::from(std::env::var_os("RUNNER_TEMP").ok_or("runner_temp_required")?)
            .canonicalize()
            .map_err(|_| "runner_temp_not_found")?;
        if !dir.starts_with(&temp) || dir == temp {
            return Err("owned_state_dir_must_be_beneath_runner_temp");
        }
        Ok(Self {
            port: port.filter(|p| *p != 0).ok_or("port_required")?,
            dir,
            target_pid: target_pid
                .filter(|pid| *pid != 0)
                .ok_or("target_pid_required")?,
            duration: Duration::from_secs(duration),
            idle: Duration::from_secs(idle),
        })
    }
}

#[derive(Clone)]
struct Target {
    sequence: u64,
    clicks: u64,
    rgb: [u8; 3],
    screen: (u16, u16),
}

fn number(value: &Value, name: &str) -> Check<u64> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .ok_or("target_numeric_field_missing")
}

fn target(config: &Config) -> Check<Target> {
    let bytes =
        std::fs::read(config.dir.join("state.json")).map_err(|_| "target_state_read_failed")?;
    if bytes.len() > 65_536 {
        return Err("target_state_too_large");
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "target_state_invalid_json")?;
    if value["ready"] != true
        || value["closed"] != false
        || number(&value, "pid")? != config.target_pid
    {
        return Err("target_not_owned_or_ready");
    }
    let geometry = &value["geometry"];
    if number(geometry, "x")? != 0
        || number(geometry, "y")? != 0
        || number(geometry, "width")? != 800
        || number(geometry, "height")? != 600
    {
        return Err("target_geometry_changed");
    }
    let shared = &value["shared_probe"];
    if shared["protocol"] != "adit-public-shared-v1" {
        return Err("target_protocol_mismatch");
    }
    if shared["point"] != json!([752, 52]) {
        return Err("target_click_point_mismatch");
    }
    let rgb: Vec<u8> = shared["rgb"]
        .as_array()
        .ok_or("target_rgb_missing")?
        .iter()
        .map(|v| {
            v.as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .ok_or("target_rgb_invalid")
        })
        .collect::<Check<_>>()?;
    let sequence = number(shared, "sequence")?;
    let clicks = number(shared, "clicks")?;
    let rgb: [u8; 3] = rgb.try_into().map_err(|_| "target_rgb_length")?;
    if sequence > 511 || clicks != sequence {
        return Err("target_marker_sequence_out_of_range");
    }
    let expected = std::array::from_fn::<_, 3, _>(|i| 16 + 32 * ((sequence >> (i * 3)) & 7) as u8);
    if rgb != expected {
        return Err("target_marker_color_protocol_mismatch");
    }
    Ok(Target {
        sequence,
        clicks,
        rgb,
        screen: (
            u16::try_from(number(geometry, "screen_width")?)
                .map_err(|_| "target_screen_invalid")?,
            u16::try_from(number(geometry, "screen_height")?)
                .map_err(|_| "target_screen_invalid")?,
        ),
    })
}

struct Client {
    manager: SessionManager,
    session: SessionId,
    frame: Option<RdpFrame>,
    connected: bool,
    disconnects: u64,
    max_pending: u8,
}

impl Client {
    fn open(port: u16, password: &str) -> Check<Self> {
        let mut profile =
            ConnectionProfile::new("public Windows shared fixture", "127.0.0.1", port, "");
        profile.protocol = Protocol::Vnc;
        profile.vnc.security = VncSecurity::RequireVncAuth;
        profile.vnc.quality = VncQuality::Lossless;
        profile.vnc.auto_reconnect = false;
        profile.vnc.read_only = false;
        profile.vnc.clipboard_send = false;
        profile.vnc.clipboard_receive = false;
        let id = profile.id;
        let mut manager = SessionManager::with_profiles(vec![profile]);
        let session = manager
            .open_live_vnc_session(id, password.into())
            .map_err(|_| "client_open_failed")?;
        Ok(Self {
            manager,
            session,
            frame: None,
            connected: false,
            disconnects: 0,
            max_pending: 0,
        })
    }

    fn poll(&mut self) -> Check<()> {
        self.manager.poll_events();
        let status = self
            .manager
            .active_session_summary()
            .ok_or("client_session_missing")?
            .status;
        if self.manager.sessions().len() != 1 {
            return Err("client_session_count_changed");
        }
        if self.connected && status != SessionStatus::Connected {
            self.disconnects += 1;
            return Err("client_disconnected_during_acceptance");
        }
        if status == SessionStatus::Error || self.manager.active_desktop_error().is_some() {
            return Err("client_connection_error");
        }
        self.connected |= status == SessionStatus::Connected;
        if let Some(diagnostics) = self.manager.active_vnc_diagnostics() {
            self.max_pending = self.max_pending.max(diagnostics.pending_updates);
            if diagnostics.pending_updates > 1 {
                return Err("refresh_requests_piled_up");
            }
            if self.connected && diagnostics.security != "VncAuth" {
                return Err("unexpected_security");
            }
        }
        let generation = self.frame.as_ref().map_or(0, |frame| frame.generation);
        if let Some(frame) = self.manager.active_rdp_frame_if_newer(generation) {
            self.frame = Some(frame);
        }
        Ok(())
    }

    fn matches(&self, target: &Target, newer_than: Option<u64>) -> bool {
        let Some(frame) = &self.frame else {
            return false;
        };
        if !self.connected
            || (frame.width, frame.height) != target.screen
            || newer_than.is_some_and(|generation| frame.generation <= generation)
            || frame.rgba.len() != usize::from(frame.width) * usize::from(frame.height) * 4
            || !frame.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255)
        {
            return false;
        }
        [
            (40_usize, 40_usize, [224, 48, 32], 24),
            (670, 470, [32, 80, 224], 24),
            (752, 52, target.rgb, 8),
        ]
        .into_iter()
        .all(|(x, y, rgb, tolerance)| {
            if x >= usize::from(frame.width) || y >= usize::from(frame.height) {
                return false;
            }
            let offset = (y * usize::from(frame.width) + x) * 4;
            frame.rgba[offset..offset + 3]
                .iter()
                .zip(rgb)
                .all(|(&actual, expected)| actual.abs_diff(expected) <= tolerance)
        })
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.manager.close(self.session);
    }
}

#[derive(Default)]
struct Resources {
    rss_peak: u64,
    cpu_start_ms: Option<u64>,
    cpu_total_ms: u64,
    cpu_peak_percent: f64,
    last: Option<(Instant, u64)>,
}

impl Resources {
    fn sample(&mut self) -> Check<()> {
        if self
            .last
            .is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(1))
        {
            return Ok(());
        }
        let (rss, cpu_ms) = process_usage()?;
        self.rss_peak = self.rss_peak.max(rss);
        self.cpu_total_ms = cpu_ms.saturating_sub(*self.cpu_start_ms.get_or_insert(cpu_ms));
        let now = Instant::now();
        if let Some((at, previous)) = self.last {
            self.cpu_peak_percent = self.cpu_peak_percent.max(
                cpu_ms.saturating_sub(previous) as f64
                    / now.duration_since(at).as_secs_f64()
                    / 10.0,
            );
        }
        self.last = Some((now, cpu_ms));
        Ok(())
    }

    fn evidence(&self) -> Value {
        json!({"probe_process_peak_working_set_bytes": self.rss_peak,
            "probe_cpu_ms_since_first_sample": self.cpu_total_ms,
            "probe_cpu_peak_percent_one_core": self.cpu_peak_percent,
            "cpu_sample_min_seconds": 1,
            "scope": "whole probe process, including both production clients; not per-client attribution"})
    }
}

#[cfg(windows)]
fn process_usage() -> Check<(u64, u64)> {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Memory {
        cb: u32,
        faults: u32,
        peak: usize,
        working: usize,
        peak_paged: usize,
        paged: usize,
        peak_nonpaged: usize,
        nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessTimes(
            process: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(process: *mut c_void, counters: *mut Memory, size: u32) -> i32;
    }
    let mut memory = Memory {
        cb: std::mem::size_of::<Memory>() as u32,
        ..Memory::default()
    };
    let (mut created, mut exited, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    // Both APIs read only this process through the documented pseudo-handle.
    let success = unsafe {
        let process = GetCurrentProcess();
        GetProcessMemoryInfo(process, &mut memory, std::mem::size_of::<Memory>() as u32) != 0
            && GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) != 0
    };
    if !success {
        return Err("process_resource_query_failed");
    }
    let ticks = |t: FileTime| (u64::from(t.high) << 32) | u64::from(t.low);
    Ok((memory.peak as u64, (ticks(kernel) + ticks(user)) / 10_000))
}

#[cfg(not(windows))]
fn process_usage() -> Check<(u64, u64)> {
    Err("windows_process_metrics_required")
}

struct Probe {
    config: Config,
    clients: Vec<Client>,
    resources: Resources,
    start: Instant,
    last_report: Instant,
    cycles: u64,
    idle_windows: u64,
    phase: &'static str,
}

impl Probe {
    fn poll(&mut self) -> Check<()> {
        for client in &mut self.clients {
            client.poll()?;
        }
        self.resources.sample()?;
        if self.last_report.elapsed() >= Duration::from_secs(5) {
            emit(json!({"event": "progress", "phase": self.phase,
                "elapsed_ms": self.start.elapsed().as_millis(), "cycles": self.cycles,
                "max_pending": self.clients.iter().map(|c| c.max_pending).collect::<Vec<_>>(),
                "resources": self.resources.evidence()}));
            self.last_report = Instant::now();
        }
        Ok(())
    }

    fn await_pixels(&mut self, expected: &Target, generations: Option<[u64; 2]>) -> Check<()> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.poll()?;
            if self
                .clients
                .iter()
                .enumerate()
                .all(|(i, client)| client.matches(expected, generations.map(|values| values[i])))
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("fresh_shared_rgb_deadline");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn click(&mut self, actor: usize) -> Check<()> {
        self.phase = "consumed_click_and_fresh_shared_rgb";
        let before = target(&self.config)?;
        if before.sequence == 511 {
            return Err("unique_marker_budget_exhausted");
        }
        let old: [u64; 2] =
            std::array::from_fn(|i| self.clients[i].frame.as_ref().map_or(0, |f| f.generation));
        self.clients[actor]
            .manager
            .send_rdp_input_to_active(RdpInput::MouseMove { x: 752, y: 52 });
        self.clients[actor]
            .manager
            .send_rdp_input_to_active(RdpInput::MouseButton {
                button: RdpMouseButton::Left,
                pressed: true,
            });
        self.clients[actor]
            .manager
            .send_rdp_input_to_active(RdpInput::MouseButton {
                button: RdpMouseButton::Left,
                pressed: false,
            });
        // Keep a server-rendered cursor away from the public marker sample.
        self.clients[actor]
            .manager
            .send_rdp_input_to_active(RdpInput::MouseMove { x: 400, y: 300 });
        let deadline = Instant::now() + Duration::from_secs(5);
        let after = loop {
            self.poll()?;
            let current = target(&self.config)?;
            if current.sequence == before.sequence + 1 && current.clicks == before.clicks + 1 {
                if current.rgb == before.rgb {
                    return Err("consumed_click_did_not_change_marker");
                }
                break current;
            }
            if current.sequence != before.sequence || current.clicks != before.clicks {
                return Err("unexpected_target_sequence_or_duplicate_click");
            }
            if Instant::now() >= deadline {
                return Err("target_click_consumption_deadline");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        self.await_pixels(&after, Some(old))?;
        emit(
            json!({"event": "shared_click", "actor": actor, "sequence": after.sequence,
            "consumed_clicks": after.clicks, "both_fresh_rgb": true,
            "full_coverage_and_static_canaries": true,
            "elapsed_ms": self.start.elapsed().as_millis()}),
        );
        Ok(())
    }

    fn run(&mut self, password: &str) -> Check<()> {
        self.resources.sample()?;
        let initial = target(&self.config)?;
        self.phase = "first_client_complete_rgb";
        self.clients.push(Client::open(self.config.port, password)?);
        self.await_pixels(&initial, None)?;
        self.phase = "second_client_shared_complete_rgb";
        self.clients.push(Client::open(self.config.port, password)?);
        self.await_pixels(&initial, None)?;
        emit(
            json!({"event": "two_clients_ready", "security": "VncAuth", "auto_reconnect": false,
            "shared": true, "full_coverage_and_static_canaries": true,
            "elapsed_ms": self.start.elapsed().as_millis()}),
        );
        let soak_start = Instant::now();
        while self.cycles == 0 || soak_start.elapsed() < self.config.duration {
            self.phase = "idle_without_input";
            let before_idle = target(&self.config)?;
            let idle_start = Instant::now();
            while idle_start.elapsed() < self.config.idle {
                self.poll()?;
                std::thread::sleep(Duration::from_millis(20));
            }
            let after_idle = target(&self.config)?;
            if before_idle.sequence != after_idle.sequence
                || before_idle.clicks != after_idle.clicks
            {
                return Err("target_changed_during_input_idle");
            }
            self.idle_windows += 1;
            emit(
                json!({"event": "idle_completed", "window": self.idle_windows,
                "idle_ms": idle_start.elapsed().as_millis(), "no_synthetic_input": true}),
            );
            self.click(0)?;
            self.click(1)?;
            self.cycles += 1;
        }
        emit(
            json!({"event": "soak_completed", "actual_soak_ms": soak_start.elapsed().as_millis(),
            "requested_seconds": self.config.duration.as_secs(), "idle_seconds": self.config.idle.as_secs(),
            "long_term_acceptance": self.config.duration.as_secs() >= 7200 && self.config.idle.as_secs() >= 120,
            "cycles": self.cycles, "idle_windows": self.idle_windows,
            "scope": "loopback Windows shared synthetic desktop; not weak network, OS sleep, service, DPI or physical input"}),
        );
        Ok(())
    }

    fn close(&mut self) -> bool {
        let mut closed = true;
        for client in &mut self.clients {
            closed &= client.manager.disconnect(client.session).is_ok();
            client.manager.close(client.session);
            closed &= client.manager.sessions().is_empty();
        }
        // Closing a record requests worker shutdown; the orchestrator additionally
        // verifies vendor disconnects/process cleanup. No OS socket-close ack is claimed.
        std::thread::sleep(Duration::from_millis(600));
        closed
    }
}

fn main() {
    let config = match Config::read() {
        Ok(config) => config,
        Err(error) => {
            emit(json!({"event": "result", "passed": false, "error": error}));
            std::process::exit(2);
        }
    };
    let mut password = Zeroizing::new(String::new());
    if io::stdin()
        .lock()
        .take(128)
        .read_line(&mut password)
        .is_err()
        || password.trim_end_matches(['\r', '\n']) != "adit-ci"
    {
        emit(
            json!({"event": "result", "passed": false, "error": "public_fixture_password_required_on_stdin"}),
        );
        std::process::exit(2);
    }
    let mut probe = Probe {
        config,
        clients: Vec::new(),
        resources: Resources::default(),
        start: Instant::now(),
        last_report: Instant::now(),
        cycles: 0,
        idle_windows: 0,
        phase: "startup",
    };
    let result = probe.run(password.trim_end_matches(['\r', '\n']));
    drop(password);
    let closed = probe.close();
    let _ = probe.resources.sample();
    emit(
        json!({"event": "result", "passed": result.is_ok() && closed,
        "error": result.err(), "phase": probe.phase, "close_requested_and_records_removed": closed,
        "worker_shutdown_grace_ms": 600, "os_socket_cleanup_verified": false,
        "cycles": probe.cycles, "idle_windows": probe.idle_windows,
        "disconnects": probe.clients.iter().map(|c| c.disconnects).collect::<Vec<_>>(),
        "max_pending": probe.clients.iter().map(|c| c.max_pending).collect::<Vec<_>>(),
        "resources": probe.resources.evidence(), "elapsed_ms": probe.start.elapsed().as_millis()}),
    );
    if result.is_err() || !closed {
        std::process::exit(1);
    }
}
