//! Actual X11 selection exchange, not a mocked Extended Clipboard peer.
//! Only isolated loopback fixtures and synthetic text; see tests/vnc/README.md.
use adit_domain::{ConnectionProfile, Protocol, SessionStatus};
use adit_session::SessionManager;
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

struct ClipboardPeer {
    child: Child,
    input: ChildStdin,
    output: Receiver<String>,
}
impl ClipboardPeer {
    fn start(display: &str) -> Self {
        let mut command = if std::env::var_os("ADIT_VNC_CLIP_LOCAL").is_some() {
            let mut c = Command::new("python3");
            c.arg("-u")
                .arg(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../tests/vnc/clipboard_target.py"),
                )
                .arg(display);
            c
        } else {
            let mut c = Command::new("docker");
            c.args([
                "exec",
                "-i",
                "adit-vnc-matrix",
                "python3",
                "-u",
                "/tmp/adit-clipboard-target.py",
                display,
            ]);
            c
        };
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let (sender, output_rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in output.lines() {
                let Ok(line) = line else {
                    break;
                };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut peer = Self {
            child,
            input,
            output: output_rx,
        };
        assert_eq!(peer.read(), "READY");
        peer
    }
    fn read(&mut self) -> String {
        self.output
            .recv_timeout(Duration::from_secs(5))
            .expect("clipboard helper exited or timed out")
    }
    fn request(&mut self, command: &str) -> String {
        writeln!(self.input, "{command}").unwrap();
        self.input.flush().unwrap();
        self.read()
    }
    fn set(&mut self, text: &str) {
        let hex: String = text.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(self.request(&format!("SET {hex}")), "OK");
    }
    fn get(&mut self) -> String {
        let reply = self.request("GET");
        if reply == "NONE" {
            return String::new();
        }
        let hex = reply
            .strip_prefix("TEXT ")
            .expect("unexpected helper result");
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|n| u8::from_str_radix(&hex[n..n + 2], 16).unwrap())
            .collect();
        String::from_utf8(bytes)
            .unwrap()
            .replace("\r\n", "\n")
            .replace('\r', "\n")
    }
}
impl Drop for ClipboardPeer {
    fn drop(&mut self) {
        let _ = writeln!(self.input, "QUIT");
        let _ = self.input.flush();
        for _ in 0..50 {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn until(manager: &mut SessionManager, mut ready: impl FnMut(&mut SessionManager) -> bool) {
    let start = Instant::now();
    loop {
        manager.poll_events();
        if ready(manager) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "clipboard exchange timed out: {:?}",
            manager.active_vnc_diagnostics()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "requires isolated x11vnc/TigerVNC/TurboVNC and tests/vnc/clipboard_target.py"]
fn real_extended_clipboard_exchanges_unicode_empty_text_and_direction_policies() {
    let port: u16 = std::env::var("ADIT_VNC_CLIP_PORT")
        .unwrap_or_else(|_| "15903".into())
        .parse()
        .unwrap();
    let display = std::env::var("ADIT_VNC_CLIP_DISPLAY").unwrap_or_else(|_| "100".into());
    assert!(port >= 15000, "fixture port required");
    let mut peer = ClipboardPeer::start(&display);
    peer.set("initial synthetic clipboard");
    let mut profile = ConnectionProfile::new("clipboard fixture", "127.0.0.1", port, "");
    profile.protocol = Protocol::Vnc;
    profile.vnc.auto_reconnect = false;
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    let session = manager.open_live_vnc_session(id, "adit-ci".into()).unwrap();
    until(&mut manager, |m| {
        m.active_session_summary().unwrap().status == SessionStatus::Connected
            && m.active_vnc_diagnostics().is_some_and(|d| d.clipboard_utf8)
    });
    for text in ["中文😀\r\n第二行", "café\nmultiline", ""] {
        let expected = text.replace("\r\n", "\n");
        manager.offer_clipboard_to_active_rdp(text.into());
        until(&mut manager, |_| peer.get() == expected);
        // Ensure clearing is a genuine change even when the previous local
        // transfer already left an empty X11 selection.
        manager.offer_clipboard_to_active_rdp("local synthetic marker".into());
        until(&mut manager, |_| peer.get() == "local synthetic marker");
        peer.set(text);
        until(&mut manager, |m| {
            m.take_rdp_clipboard().as_deref() == Some(expected.as_str())
        });
    }
    // Transfer ownership to the server before the next synthetic X11 copy.
    manager.offer_clipboard_to_active_rdp("read-only setup marker".into());
    until(&mut manager, |_| peer.get() == "read-only setup marker");
    manager.set_active_vnc_read_only(true);
    peer.set("remote allowed 中文");

    until(&mut manager, |m| {
        m.take_rdp_clipboard().as_deref() == Some("remote allowed 中文")
    });
    manager.offer_clipboard_to_active_rdp("blocked local text".into());
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(peer.get(), "remote allowed 中文");
    manager.set_rdp_clipboard(false);
    peer.set("disabled remote text");
    let end = Instant::now() + Duration::from_millis(400);
    while Instant::now() < end {
        manager.poll_events();
        assert!(manager.take_rdp_clipboard().is_none());
        std::thread::sleep(Duration::from_millis(20));
    }
    manager.close(session);
}
