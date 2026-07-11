//! End-to-end integration tests against a real OpenSSH `sshd` running in Docker.
//!
//! These prove interop with the server users actually connect to (banner/kex/
//! cipher negotiation, the SFTP subsystem, `direct-tcpip` forwarding) — which the
//! in-process russh unit tests can't. They drive Adit at the `adit-ssh` boundary
//! (the `spawn_*` handle + `try_recv` event API), never the GUI.
//!
//! Gated behind the `integration` feature so the default `cargo test --workspace`
//! (Windows CI, no Docker) skips them. Run locally / in the Linux CI job with:
//!
//! ```text
//! cargo test -p adit-ssh --features integration -- --include-ignored --test-threads=4
//! ```
//!
//! Requires a working Docker daemon. The test builds a throwaway sshd image
//! (`docker/test-sshd/`) on first use (layer-cached), starts one container per
//! test on a random published port, and removes it on drop.
#![cfg(feature = "integration")]

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, thread};

use adit_ssh::{
    spawn_password_shell, spawn_sftp_session, spawn_tunnel_session, AuthOptions, LiveShellCommand,
    LiveShellEvent, LiveShellRequest, SftpCommand, SftpEvent, SftpRequest, TunnelEvent,
    TunnelKind, TunnelRequest,
};

const IMAGE: &str = "adit-test-sshd:latest";
const USER: &str = "adit";
const PASS: &str = "aditpw";

// ===== docker plumbing =======================================================

fn docker(args: &[&str]) -> Result<String, String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|error| format!("failed to run docker {args:?}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "docker {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Build the test sshd image once per test run (layer-cached, so it's cheap).
fn ensure_image() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("docker")
            .join("test-sshd");
        let dir = dir.to_string_lossy().to_string();
        docker(&["build", "-t", IMAGE, &dir]).expect("docker build test-sshd image");
    });
}

/// A throwaway sshd container; removed on drop.
struct TestServer {
    id: String,
    port: u16,
}

impl TestServer {
    fn start() -> TestServer {
        ensure_image();
        let id = docker(&["run", "-d", "-P", IMAGE]).expect("docker run");
        let port = published_port(&id, 2222);
        let server = TestServer { id, port };
        server.wait_ready(Duration::from_secs(30));
        server
    }

    /// Wait until the sshd inside the container answers with an SSH banner.
    fn wait_ready(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(banner) = read_line(self.port, Duration::from_millis(800)) {
                if banner.starts_with("SSH-") {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(300));
        }
        panic!(
            "sshd never became ready on port {} (logs: {})",
            self.port,
            docker(&["logs", &self.id]).unwrap_or_default()
        );
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", "-v", &self.id]);
    }
}

/// The host port that the container's `2222/tcp` was published to.
fn published_port(id: &str, container_port: u16) -> u16 {
    let mapping = docker(&["port", id, &format!("{container_port}/tcp")]).expect("docker port");
    // e.g. "0.0.0.0:49153" (possibly several lines for v4/v6) — take the first.
    let first = mapping.lines().next().unwrap_or_default();
    first
        .rsplit(':')
        .next()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or_else(|| panic!("could not parse published port from {mapping:?}"))
}

// ===== small helpers =========================================================

fn unique() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    t ^ (n << 40) ^ (std::process::id() as u64)
}

fn temp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("adit-it-{tag}-{}", unique()))
}

fn temp_known_hosts() -> PathBuf {
    temp_path("known_hosts")
}

/// Pump events from `recv` until `step` breaks or the timeout elapses.
fn pump_until<E, T>(
    timeout: Duration,
    mut recv: impl FnMut() -> Option<E>,
    mut step: impl FnMut(E) -> ControlFlow<T>,
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match recv() {
            Some(event) => {
                if let ControlFlow::Break(value) = step(event) {
                    return Some(value);
                }
            }
            None => thread::sleep(Duration::from_millis(20)),
        }
    }
    None
}

fn free_local_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Connect to `127.0.0.1:port`, read one line (up to `\n`), return it.
fn read_line(port: u16, timeout: Duration) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(timeout))?;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while buf.len() < 512 {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(String::from_utf8_lossy(&buf).trim_end_matches('\r').to_string())
}

/// Pump SFTP events until the next `Done` (panicking on `Error`).
fn wait_sftp_done(handle: &adit_ssh::SftpHandle, timeout: Duration) {
    let done = pump_until(
        timeout,
        || handle.try_recv(),
        |event| match event {
            SftpEvent::Done { .. } => ControlFlow::Break(true),
            SftpEvent::Error(error) => panic!("sftp error: {error}"),
            _ => ControlFlow::Continue(()),
        },
    );
    assert_eq!(done, Some(true), "sftp transfer did not complete in time");
}

// ===== tests =================================================================

#[test]
fn password_auth_opens_shell_and_runs_a_command() {
    let server = TestServer::start();
    let known_hosts = temp_known_hosts();

    // `echo NONCE-$((6*7))` — the *typed command* contains "$((6*7))", so seeing
    // "NONCE-42" in the output proves the shell actually executed it (not echo).
    let mut request = LiveShellRequest::new("127.0.0.1", server.port, USER, PASS);
    request.known_hosts_path = known_hosts.clone();
    request.auto_accept_host_keys = true;
    request.startup_command = String::from("echo NONCE-$((6*7))");

    let handle = spawn_password_shell(request).expect("spawn shell");
    let mut transcript = String::new();
    let found = pump_until(
        Duration::from_secs(20),
        || handle.try_recv(),
        |event| match event {
            LiveShellEvent::Output(bytes) => {
                transcript.push_str(&String::from_utf8_lossy(&bytes));
                if transcript.contains("NONCE-42") {
                    ControlFlow::Break(true)
                } else {
                    ControlFlow::Continue(())
                }
            }
            LiveShellEvent::Error(error) => panic!("ssh error: {error}"),
            LiveShellEvent::Closed => ControlFlow::Break(false),
            _ => ControlFlow::Continue(()),
        },
    );
    assert_eq!(found, Some(true), "shell should run the command; transcript:\n{transcript}");
    let _ = handle.send(LiveShellCommand::Disconnect);

    // TOFU recorded the container's host key.
    let recorded = fs::read_to_string(&known_hosts).unwrap_or_default();
    assert!(!recorded.trim().is_empty(), "host key should have been recorded to known_hosts");
}

#[test]
fn wrong_password_is_rejected() {
    let server = TestServer::start();
    let mut request = LiveShellRequest::new("127.0.0.1", server.port, USER, "WRONG-PASSWORD");
    request.known_hosts_path = temp_known_hosts();
    request.auto_accept_host_keys = true;
    // Only password, so it fails fast as an auth rejection (no agent/key fallback).
    request.auth = AuthOptions {
        try_password: true,
        try_agent: false,
        try_default_keys: false,
        identity_file: None,
    };

    let handle = spawn_password_shell(request).expect("spawn shell");
    let message = pump_until(
        Duration::from_secs(20),
        || handle.try_recv(),
        |event| match event {
            LiveShellEvent::Error(error) => ControlFlow::Break(error),
            LiveShellEvent::Closed => ControlFlow::Break(String::from("closed")),
            _ => ControlFlow::Continue(()),
        },
    )
    .expect("a wrong password should surface an error");
    let lower = message.to_lowercase();
    assert!(
        lower.contains("auth") || lower.contains("reject"),
        "expected an auth-rejection error, got: {message}"
    );
}

#[test]
fn sftp_round_trips_a_file() {
    let server = TestServer::start();
    let mut request = SftpRequest::new("127.0.0.1", server.port, USER, PASS);
    request.known_hosts_path = temp_known_hosts();
    let handle = spawn_sftp_session(request).expect("spawn sftp");

    let ready = pump_until(
        Duration::from_secs(20),
        || handle.try_recv(),
        |event| match event {
            SftpEvent::Ready { .. } => ControlFlow::Break(true),
            SftpEvent::Error(error) => panic!("sftp error: {error}"),
            SftpEvent::Closed => ControlFlow::Break(false),
            _ => ControlFlow::Continue(()),
        },
    );
    assert_eq!(ready, Some(true), "sftp session should become ready");

    let content = b"hello adit integration \x01\x02\x03 end\n";
    let local_up = temp_path("upload.bin");
    fs::write(&local_up, content).unwrap();
    let remote = format!("/tmp/adit-it-{}.bin", unique());

    handle
        .send(SftpCommand::Upload { local: local_up, remote: remote.clone() })
        .unwrap();
    wait_sftp_done(&handle, Duration::from_secs(20));

    let local_down = temp_path("download.bin");
    handle
        .send(SftpCommand::Download { remote, local: local_down.clone() })
        .unwrap();
    wait_sftp_done(&handle, Duration::from_secs(20));

    assert_eq!(fs::read(&local_down).unwrap(), content, "downloaded bytes must match uploaded");
}

#[test]
fn sftp_round_trips_a_directory_tree() {
    let server = TestServer::start();
    let mut request = SftpRequest::new("127.0.0.1", server.port, USER, PASS);
    request.known_hosts_path = temp_known_hosts();
    let handle = spawn_sftp_session(request).expect("spawn sftp");
    pump_until(
        Duration::from_secs(20),
        || handle.try_recv(),
        |event| match event {
            SftpEvent::Ready { .. } => ControlFlow::Break(()),
            SftpEvent::Error(error) => panic!("sftp error: {error}"),
            _ => ControlFlow::Continue(()),
        },
    )
    .expect("sftp ready");

    // Build a local tree:  tree/a.txt, tree/sub/b.txt
    let tree = temp_path("tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("a.txt"), b"file a\n").unwrap();
    fs::write(tree.join("sub").join("b.txt"), b"file b nested\n").unwrap();

    let name = format!("adit-it-tree-{}", unique());
    let remote = format!("/tmp/{name}");
    handle
        .send(SftpCommand::Upload { local: tree, remote: remote.clone() })
        .unwrap();
    wait_sftp_done(&handle, Duration::from_secs(30));

    let down_root = temp_path("down");
    fs::create_dir_all(&down_root).unwrap();
    let down = down_root.join(&name);
    handle
        .send(SftpCommand::Download { remote, local: down.clone() })
        .unwrap();
    wait_sftp_done(&handle, Duration::from_secs(30));

    assert_eq!(fs::read(down.join("a.txt")).unwrap(), b"file a\n");
    assert_eq!(fs::read(down.join("sub").join("b.txt")).unwrap(), b"file b nested\n");
}

#[test]
fn local_forward_reaches_the_remote_sshd() {
    let server = TestServer::start();
    let bind_port = free_local_port();
    // Forward local bind_port -> (from the server) 127.0.0.1:2222, i.e. the
    // container's own sshd. A round-trip through the tunnel yields its SSH banner.
    let mut request = TunnelRequest::new(
        "127.0.0.1",
        server.port,
        USER,
        PASS,
        TunnelKind::Local,
        "127.0.0.1",
        bind_port,
        "127.0.0.1",
        2222,
    );
    request.known_hosts_path = temp_known_hosts();
    let handle = spawn_tunnel_session(request).expect("spawn tunnel");

    let listening = pump_until(
        Duration::from_secs(20),
        || handle.try_recv(),
        |event| match event {
            TunnelEvent::Listening { .. } => ControlFlow::Break(true),
            TunnelEvent::Error(error) => panic!("tunnel error: {error}"),
            TunnelEvent::Stopped => ControlFlow::Break(false),
            _ => ControlFlow::Continue(()),
        },
    );
    assert_eq!(listening, Some(true), "tunnel should start listening");

    // Give the listener a beat, then round-trip through it.
    let banner = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(line) = read_line(bind_port, Duration::from_secs(5)) {
                if !line.is_empty() {
                    break line;
                }
            }
            if Instant::now() > deadline {
                panic!("no data through the local forward");
            }
            thread::sleep(Duration::from_millis(200));
        }
    };
    assert!(
        banner.starts_with("SSH-2.0"),
        "expected the remote sshd banner through the forward, got: {banner:?}"
    );
}
