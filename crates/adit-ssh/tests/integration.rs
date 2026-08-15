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

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, thread};

use adit_ssh::{
    spawn_password_shell, spawn_sftp_session, spawn_sftp_session_on, spawn_tunnel_session,
    spawn_tunnel_session_on, AuthOptions, JumpHop, LiveShellCommand, LiveShellEvent,
    LiveShellHandle, LiveShellRequest, SftpCommand, SftpEvent, SftpHandle, SftpRequest,
    SharedSession, TunnelEvent, TunnelKind, TunnelRequest,
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

/// A throwaway user-defined bridge network; removed on drop. Containers on it
/// resolve each other by `--network-alias`, which is how a jump host reaches a
/// target that isn't published to the host at all.
struct TestNetwork {
    name: String,
}

impl TestNetwork {
    fn create() -> TestNetwork {
        let name = format!("adit-it-net-{}", unique());
        docker(&["network", "create", &name]).expect("docker network create");
        TestNetwork { name }
    }
}

impl Drop for TestNetwork {
    fn drop(&mut self) {
        let _ = docker(&["network", "rm", &self.name]);
    }
}

/// Host-side paths to a keypair generated inside a container.
struct KeyPair {
    private: PathBuf,
    public: PathBuf,
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

    /// Start a container attached to `net` under `alias`. When `publish` is set
    /// it also gets a random host port (so the host can read its banner); when
    /// not, it is reachable only from inside `net` — the shape a real bastion
    /// hides its targets behind.
    fn start_on_network(net: &TestNetwork, alias: &str, publish: bool) -> TestServer {
        ensure_image();
        // `--hostname alias` makes the container self-identifying: a shell that
        // runs `hostname` there prints `alias`, so the test transcript itself
        // proves *which* server was reached (bastion vs target).
        let mut args = vec![
            "run",
            "-d",
            "--network",
            net.name.as_str(),
            "--network-alias",
            alias,
            "--hostname",
            alias,
        ];
        if publish {
            args.push("-P");
        }
        args.push(IMAGE);
        let id = docker(&args).expect("docker run on network");
        let port = if publish {
            published_port(&id, 2222)
        } else {
            0
        };
        TestServer { id, port }
    }

    /// Wait until the container's own sshd answers from inside it. Used for a
    /// non-published server whose banner the host cannot read directly.
    fn wait_ready_internal(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if docker(&[
                "exec",
                &self.id,
                "sh",
                "-c",
                "ssh-keyscan -T 2 -p 2222 127.0.0.1 2>/dev/null | grep -q ssh",
            ])
            .is_ok()
            {
                return;
            }
            thread::sleep(Duration::from_millis(300));
        }
        panic!(
            "sshd never became ready inside container {} (logs: {})",
            self.id,
            docker(&["logs", &self.id]).unwrap_or_default()
        );
    }

    /// Generate a keypair inside the container, authorize its public half for
    /// `adit` here, and copy both halves to host temp files. The private key can
    /// then be presented for auth, and the public key authorized on other
    /// containers via [`authorize_key`](Self::authorize_key).
    fn generate_key(&self) -> KeyPair {
        docker(&[
            "exec",
            &self.id,
            "sh",
            "-c",
            "mkdir -p /home/adit/.ssh \
             && ssh-keygen -t ed25519 -N '' -q -f /home/adit/.ssh/id_ed25519 \
             && cp /home/adit/.ssh/id_ed25519.pub /home/adit/.ssh/authorized_keys \
             && chown -R adit:adit /home/adit/.ssh \
             && chmod 700 /home/adit/.ssh \
             && chmod 600 /home/adit/.ssh/authorized_keys",
        ])
        .expect("generate + authorize key");
        let private = temp_path("key");
        let public = temp_path("key.pub");
        docker(&[
            "cp",
            &format!("{}:/home/adit/.ssh/id_ed25519", self.id),
            &private.to_string_lossy(),
        ])
        .expect("copy private key to host");
        docker(&[
            "cp",
            &format!("{}:/home/adit/.ssh/id_ed25519.pub", self.id),
            &public.to_string_lossy(),
        ])
        .expect("copy public key to host");
        KeyPair { private, public }
    }

    /// Generate a *passphrase-encrypted* keypair inside the container, authorize
    /// it for `adit` here, and copy the private half to the host. Used to prove
    /// the distinct key-passphrase path (right passphrase connects; wrong one
    /// fails with a clear error).
    fn generate_encrypted_key(&self, passphrase: &str) -> PathBuf {
        let script = format!(
            "mkdir -p /home/adit/.ssh \
             && ssh-keygen -t ed25519 -N '{passphrase}' -q -f /home/adit/.ssh/id_ed25519 \
             && cp /home/adit/.ssh/id_ed25519.pub /home/adit/.ssh/authorized_keys \
             && chown -R adit:adit /home/adit/.ssh \
             && chmod 700 /home/adit/.ssh \
             && chmod 600 /home/adit/.ssh/authorized_keys"
        );
        docker(&["exec", &self.id, "sh", "-c", &script]).expect("generate encrypted key");
        let private = temp_path("enckey");
        docker(&[
            "cp",
            &format!("{}:/home/adit/.ssh/id_ed25519", self.id),
            &private.to_string_lossy(),
        ])
        .expect("copy encrypted private key to host");
        private
    }

    /// Authorize `pub_key` (a host-side public-key file) for `adit` on this
    /// container, so a key generated elsewhere can log in here too.
    fn authorize_key(&self, pub_key: &std::path::Path) {
        docker(&["exec", &self.id, "mkdir", "-p", "/home/adit/.ssh"])
            .expect("mkdir .ssh on target");
        docker(&[
            "cp",
            &pub_key.to_string_lossy(),
            &format!("{}:/home/adit/.ssh/authorized_keys", self.id),
        ])
        .expect("copy authorized_keys into target");
        docker(&[
            "exec",
            &self.id,
            "sh",
            "-c",
            "chown -R adit:adit /home/adit/.ssh \
             && chmod 700 /home/adit/.ssh \
             && chmod 600 /home/adit/.ssh/authorized_keys",
        ])
        .expect("fix authorized_keys perms on target");
    }

    /// Established TCP connections to this container's sshd, counted from
    /// *inside* the container.
    ///
    /// The client cannot tell one connection from two — both look identical from
    /// its side, which is exactly why a second dial went unnoticed until an MFA
    /// host started demanding a second one-time code. The server is the only
    /// vantage point from which "SFTP rode on the shell's connection" and
    /// "nothing was left behind" can be asserted at all.
    fn established_connections(&self) -> usize {
        let out = docker(&["exec", &self.id, "netstat", "-tn"]).unwrap_or_else(|error| {
            panic!("could not count connections inside the test container: {error}")
        });
        out.lines()
            .filter(|line| line.contains("ESTABLISHED") && line.contains(":2222"))
            .count()
    }

    /// Everything sshd has logged.
    ///
    /// Not `docker(&["logs", ..])`: sshd runs with `-e` so it logs to *stderr*,
    /// and `docker logs` relays each stream to its own, so the stdout that
    /// helper returns is always empty. Both are merged here.
    fn logs(&self) -> String {
        let output = Command::new("docker")
            .args(["logs", &self.id])
            .output()
            .expect("docker logs");
        let mut merged = String::from_utf8_lossy(&output.stdout).into_owned();
        merged.push_str(&String::from_utf8_lossy(&output.stderr));
        merged
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
            self.logs()
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

/// Pump SFTP events until the session reports itself ready.
fn wait_sftp_ready(handle: &SftpHandle, timeout: Duration) {
    let ready = pump_until(
        timeout,
        || handle.try_recv(),
        |event| match event {
            SftpEvent::Ready { .. } => ControlFlow::Break(true),
            SftpEvent::Error(error) => panic!("sftp error: {error}"),
            SftpEvent::Closed => ControlFlow::Break(false),
            _ => ControlFlow::Continue(()),
        },
    );
    assert_eq!(ready, Some(true), "sftp session should become ready");
}

/// Wait for the shell to publish the connection it just authenticated.
fn wait_for_session(handle: &LiveShellHandle, timeout: Duration) -> SharedSession {
    pump_until(
        timeout,
        || handle.try_recv(),
        |event| match event {
            LiveShellEvent::SessionReady(shared) => ControlFlow::Break(shared),
            LiveShellEvent::Error(error) | LiveShellEvent::AuthRejected(error) => {
                panic!("ssh error before the session was ready: {error}")
            }
            LiveShellEvent::Closed => panic!("the shell closed before publishing a session"),
            _ => ControlFlow::Continue(()),
        },
    )
    .expect("the shell should publish its authenticated session")
}

/// A local listener that greets every connection, as the service a remote
/// forward publishes back to the server. Returns its port and the greeting.
///
/// It serves repeatedly rather than once because the probe retries while the
/// forward is still being wired up.
fn greeting_listener() -> (u16, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the local target");
    let port = listener.local_addr().expect("local_addr").port();
    let greeting = format!("ADIT-REMOTE-{}", unique());
    let sent = greeting.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    let _ = stream.write_all(format!("{sent}\n").as_bytes());
                }
                Err(_) => break,
            }
        }
    });
    (port, greeting)
}

/// A `-R` request pointing the container's `remote_port` at a local `target_port`.
fn remote_forward_request(server: &TestServer, remote_port: u16, target_port: u16) -> TunnelRequest {
    let mut request = TunnelRequest::new(
        "127.0.0.1",
        server.port,
        USER,
        PASS,
        TunnelKind::Remote,
        // Where sshd listens, inside the container...
        "127.0.0.1",
        remote_port,
        // ...and what this client dials when the server forwards a connection.
        "127.0.0.1",
        target_port,
    );
    request.known_hosts_path = temp_known_hosts();
    request
}

fn wait_tunnel_listening(handle: &adit_ssh::TunnelHandle, timeout: Duration) {
    let listening = pump_until(
        timeout,
        || handle.try_recv(),
        |event| match event {
            TunnelEvent::Listening { .. } => ControlFlow::Break(true),
            TunnelEvent::Error(error) => panic!("tunnel error: {error}"),
            TunnelEvent::Stopped => ControlFlow::Break(false),
            _ => ControlFlow::Continue(()),
        },
    );
    assert_eq!(listening, Some(true), "the server should accept the remote forward");
}

/// Dial the container's forwarded port from inside the container, which is the
/// only place it exists. Returns whether the greeting came back, and what did.
///
/// `Listening` proves only that sshd accepted the request; the forwarded channel
/// can still resolve to no target on the client and be dropped without a word,
/// which is exactly how this failed on a shared connection.
fn probe_forward_from_inside(server: &TestServer, remote_port: u16, greeting: &str) -> (bool, String) {
    let mut got = String::new();
    let reached = wait_until(Duration::from_secs(20), || {
        got = docker(&[
            "exec",
            &server.id,
            "sh",
            "-c",
            &format!("nc -w 5 127.0.0.1 {remote_port} < /dev/null || true"),
        ])
        .unwrap_or_default();
        got.contains(greeting)
    });
    (reached, got)
}

/// Poll `check` until it holds, or the timeout elapses.
fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        thread::sleep(Duration::from_millis(200));
    }
    false
}

/// Upload a distinctive payload and download it back, asserting it survives.
///
/// A channel that opens is not a subsystem that works, and every bug this file
/// is guarding against leaves the former looking fine.
fn assert_sftp_round_trip(handle: &SftpHandle, tag: &str) {
    let content = format!("adit {tag} {}\n", unique()).into_bytes();
    let local_up = temp_path("up.bin");
    fs::write(&local_up, &content).unwrap();
    let remote = format!("/tmp/adit-it-{}.bin", unique());

    handle
        .send(SftpCommand::Upload { id: 0, local: local_up, remote: remote.clone() })
        .unwrap();
    wait_sftp_done(handle, Duration::from_secs(20));

    let local_down = temp_path("down.bin");
    handle
        .send(SftpCommand::Download { id: 1, remote, local: local_down.clone() })
        .unwrap();
    wait_sftp_done(handle, Duration::from_secs(20));

    assert_eq!(
        fs::read(&local_down).unwrap(),
        content,
        "{tag}: bytes must survive the round trip"
    );
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

/// `exit` in the remote shell must report `Closed` even while something is
/// still holding the connection.
///
/// This mirrors what the session layer does: it keeps the `SharedSession` from
/// `SessionReady` so SFTP and tunnels can ride the connection, and releases it
/// only once it sees `Closed`. The shell thread used to wait for every claim to
/// drop *before* announcing `Closed`, so the two waited on each other for good
/// — `exit` left a tab reading connected that Enter could not reconnect,
/// because nothing had marked the session dead.
///
/// Holding the claim here is the whole point: without it the claim drops as
/// soon as the event is ignored, the wait ends on its own, and the bug cannot
/// reproduce.
#[test]
fn exit_reports_closed_while_the_connection_is_still_held() {
    let server = TestServer::start();
    let mut request = LiveShellRequest::new("127.0.0.1", server.port, USER, PASS);
    request.known_hosts_path = temp_known_hosts();
    request.auto_accept_host_keys = true;
    request.startup_command = String::from("exit");

    let handle = spawn_password_shell(request).expect("spawn shell");
    let mut held: Option<SharedSession> = None;
    let closed = pump_until(
        Duration::from_secs(20),
        || handle.try_recv(),
        |event| match event {
            LiveShellEvent::SessionReady(shared) => {
                held = Some(shared);
                ControlFlow::Continue(())
            }
            LiveShellEvent::Closed => ControlFlow::Break(true),
            LiveShellEvent::Error(error) => panic!("ssh error: {error}"),
            _ => ControlFlow::Continue(()),
        },
    );

    assert_eq!(
        closed,
        Some(true),
        "`exit` must report Closed while the connection is held, not after",
    );
    assert!(held.is_some(), "the connection should have been offered to hold");
    drop(held);
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
        passphrase: None,
    };

    let handle = spawn_password_shell(request).expect("spawn shell");
    let message = pump_until(
        Duration::from_secs(20),
        || handle.try_recv(),
        |event| match event {
            // Auth failures now surface as a distinct typed event (drives the UI's
            // password re-prompt); treat it like the error it is here.
            LiveShellEvent::Error(error) | LiveShellEvent::AuthRejected(error) => {
                ControlFlow::Break(error)
            }
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
        .send(SftpCommand::Upload { id: 0, local: local_up, remote: remote.clone() })
        .unwrap();
    wait_sftp_done(&handle, Duration::from_secs(20));

    let local_down = temp_path("download.bin");
    handle
        .send(SftpCommand::Download { id: 1, remote, local: local_down.clone() })
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
        .send(SftpCommand::Upload { id: 0, local: tree, remote: remote.clone() })
        .unwrap();
    wait_sftp_done(&handle, Duration::from_secs(30));

    let down_root = temp_path("down");
    fs::create_dir_all(&down_root).unwrap();
    let down = down_root.join(&name);
    handle
        .send(SftpCommand::Download { id: 1, remote, local: down.clone() })
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

/// Drive a shell whose target sits behind `bastion` (published) on an
/// internal-only network, asserting the shell actually landed on the target and
/// that both host keys were recorded. Shared by the password- and key-auth jump
/// tests, which differ only in `auth`.
fn assert_jump_shell_reaches_target(bastion_port: u16, auth: AuthOptions, password: &str) {
    let known_hosts = temp_known_hosts();
    // Final host is the bastion-internal alias — unreachable except via the jump.
    let mut request = LiveShellRequest::new("target", 2222, USER, password);
    request.known_hosts_path = known_hosts.clone();
    request.auto_accept_host_keys = true;
    // `hostname` prints the container's --hostname, so a transcript containing
    // "JUMP-target" proves the shell ran on the *target* (not the bastion, which
    // would print "JUMP-bastion") — the transcript alone attests to the jump.
    request.startup_command = String::from("echo JUMP-$(hostname)");
    request.auth = auth;
    request.jumps = vec![JumpHop {
        host: String::from("127.0.0.1"),
        port: bastion_port,
        username: String::from(USER),
    }];

    let handle = spawn_password_shell(request).expect("spawn shell via bastion");
    let mut transcript = String::new();
    let found = pump_until(
        Duration::from_secs(30),
        || handle.try_recv(),
        |event| match event {
            LiveShellEvent::Output(bytes) => {
                transcript.push_str(&String::from_utf8_lossy(&bytes));
                if transcript.contains("JUMP-target") {
                    ControlFlow::Break(true)
                } else {
                    ControlFlow::Continue(())
                }
            }
            LiveShellEvent::Error(error) => panic!("jump ssh error: {error}"),
            LiveShellEvent::Closed => ControlFlow::Break(false),
            _ => ControlFlow::Continue(()),
        },
    );
    assert_eq!(
        found,
        Some(true),
        "shell reached through the bastion should run on the target; transcript:\n{transcript}"
    );
    let _ = handle.send(LiveShellCommand::Disconnect);

    // Both the bastion and the target host keys should have been recorded.
    let recorded = fs::read_to_string(&known_hosts).unwrap_or_default();
    let lines = recorded.lines().filter(|l| !l.trim().is_empty()).count();
    assert!(
        lines >= 2,
        "expected bastion + target host keys recorded, got {lines} line(s):\n{recorded}"
    );
}

#[test]
fn jump_host_reaches_a_target_behind_a_bastion() {
    // Password auth through the whole chain — the profile's default. Both the
    // bastion and the target accept the same password (as Adit stores one
    // credential per profile), which is exactly the common case that must work.
    let net = TestNetwork::create();
    let target = TestServer::start_on_network(&net, "target", false);
    target.wait_ready_internal(Duration::from_secs(30));
    let bastion = TestServer::start_on_network(&net, "bastion", true);
    bastion.wait_ready(Duration::from_secs(30));

    assert_jump_shell_reaches_target(
        bastion.port,
        AuthOptions {
            try_password: true,
            try_agent: false,
            try_default_keys: false,
            identity_file: None,
            passphrase: None,
        },
        PASS,
    );
}

#[test]
fn jump_host_authenticates_with_a_key() {
    // Key auth end-to-end: one keypair authorized on both the bastion and the
    // target, no password anywhere. Proves the jump chain does public-key auth
    // on every hop and on the tunneled target.
    let net = TestNetwork::create();
    let target = TestServer::start_on_network(&net, "target", false);
    target.wait_ready_internal(Duration::from_secs(30));
    let bastion = TestServer::start_on_network(&net, "bastion", true);
    bastion.wait_ready(Duration::from_secs(30));

    let keys = bastion.generate_key();
    target.authorize_key(&keys.public);

    assert_jump_shell_reaches_target(
        bastion.port,
        AuthOptions {
            try_password: false,
            try_agent: false,
            try_default_keys: false,
            identity_file: Some(keys.private),
            passphrase: None,
        },
        "", // no password — key only
    );
}

/// Build a key-auth request against `server` using `key`, decrypting it with
/// `passphrase` (in its own field, not the login password).
fn encrypted_key_request(server: &TestServer, key: PathBuf, passphrase: &str) -> LiveShellRequest {
    let mut request = LiveShellRequest::new("127.0.0.1", server.port, USER, "");
    request.known_hosts_path = temp_known_hosts();
    request.auto_accept_host_keys = true;
    request.auth = AuthOptions {
        try_password: false,
        try_agent: false,
        try_default_keys: false,
        identity_file: Some(key),
        passphrase: (!passphrase.is_empty()).then(|| passphrase.to_string()),
    };
    request
}

#[test]
fn encrypted_key_connects_with_its_passphrase() {
    let server = TestServer::start();
    let key = server.generate_encrypted_key("s3cr3t-phrase");
    let mut request = encrypted_key_request(&server, key, "s3cr3t-phrase");
    request.startup_command = String::from("echo KEY-$((6*7))");

    let handle = spawn_password_shell(request).expect("spawn shell");
    let mut transcript = String::new();
    let found = pump_until(
        Duration::from_secs(20),
        || handle.try_recv(),
        |event| match event {
            LiveShellEvent::Output(bytes) => {
                transcript.push_str(&String::from_utf8_lossy(&bytes));
                if transcript.contains("KEY-42") {
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
    assert_eq!(found, Some(true), "encrypted key + right passphrase should log in");
    let _ = handle.send(LiveShellCommand::Disconnect);
}

#[test]
fn encrypted_key_wrong_passphrase_gives_a_clear_error() {
    let server = TestServer::start();
    let key = server.generate_encrypted_key("s3cr3t-phrase");
    // Wrong passphrase: the key fails to decrypt locally, and — because it is an
    // explicitly configured identity file — that surfaces as a clear passphrase
    // error rather than a silent fall-through to another method.
    let request = encrypted_key_request(&server, key, "WRONG-phrase");

    let handle = spawn_password_shell(request).expect("spawn shell");
    let message = pump_until(
        Duration::from_secs(20),
        || handle.try_recv(),
        |event| match event {
            // Auth failures now surface as a distinct typed event (drives the UI's
            // password re-prompt); treat it like the error it is here.
            LiveShellEvent::Error(error) | LiveShellEvent::AuthRejected(error) => {
                ControlFlow::Break(error)
            }
            LiveShellEvent::Closed => ControlFlow::Break(String::from("closed")),
            _ => ControlFlow::Continue(()),
        },
    )
    .expect("a wrong passphrase should surface an error");
    assert!(
        message.to_lowercase().contains("passphrase"),
        "expected a passphrase error, got: {message}"
    );
}

// ===== the shared connection =================================================
//
// SFTP and tunnels moved onto the shell's already-authenticated connection so an
// MFA host is asked for one one-time code instead of one per surface. Every
// failure mode of that change is invisible from the client: a second dial works
// fine against a password host, and a connection left behind produces no error
// and no UI change. So these assert from inside the container.

#[test]
fn sftp_rides_on_the_shells_connection() {
    let server = TestServer::start();
    let mut request = LiveShellRequest::new("127.0.0.1", server.port, USER, PASS);
    request.known_hosts_path = temp_known_hosts();
    request.auto_accept_host_keys = true;

    let shell = spawn_password_shell(request).expect("spawn shell");
    let shared = wait_for_session(&shell, Duration::from_secs(20));
    assert_eq!(
        server.established_connections(),
        1,
        "the shell alone should be a single connection"
    );

    let sftp = spawn_sftp_session_on(shared.clone()).expect("spawn sftp on the shared session");
    wait_sftp_ready(&sftp, Duration::from_secs(20));
    assert_sftp_round_trip(&sftp, "shared-session");

    // The payload. Everything above passes just as well if SFTP quietly dialled
    // its own connection — only the count can tell, and on an MFA host that
    // second connection is what demands a second one-time code and gets refused.
    assert_eq!(
        server.established_connections(),
        1,
        "SFTP must ride on the shell's connection instead of opening a second one"
    );

    let _ = sftp.send(SftpCommand::Disconnect);
    let _ = shell.send(LiveShellCommand::Disconnect);
}

#[test]
fn the_connection_outlives_the_shell_and_dies_with_the_last_user() {
    let server = TestServer::start();
    let mut request = LiveShellRequest::new("127.0.0.1", server.port, USER, PASS);
    request.known_hosts_path = temp_known_hosts();
    request.auto_accept_host_keys = true;

    let shell = spawn_password_shell(request).expect("spawn shell");
    let shared = wait_for_session(&shell, Duration::from_secs(20));
    let sftp = spawn_sftp_session_on(shared.clone()).expect("spawn sftp on the shared session");
    wait_sftp_ready(&sftp, Duration::from_secs(20));

    // Close the shell tab, and take sshd's word for it rather than a client
    // event. Neither client-side signal works here: ExitStatus races Eof, so
    // whichever lands first ends the read loop and the other is never reported;
    // and LiveShellEvent::Closed is emitted only after the shell thread parks
    // for the last SharedSession — which this test is still holding, so waiting
    // for it would deadlock.
    let _ = shell.send(LiveShellCommand::Disconnect);
    assert!(
        wait_until(Duration::from_secs(20), || server
            .logs()
            .contains("Close session")),
        "sshd should report the shell's session channel closed; logs:\n{}",
        server.logs()
    );

    assert!(
        !shared.is_closed(),
        "closing the shell must not tear down the connection an SFTP panel is still using"
    );
    // "Not closed" is easy to satisfy by accident; a transfer is not.
    assert_sftp_round_trip(&sftp, "after-the-shell-exited");
    assert_eq!(server.established_connections(), 1, "the connection should still be up");

    // Now let go of every user of it.
    let _ = sftp.send(SftpCommand::Disconnect);
    drop(sftp);
    drop(shared);

    // A leak here is completely silent on the client — no error, no UI change,
    // just a connection the server keeps until it times out. This assertion is
    // the only thing standing between that and a release.
    assert!(
        wait_until(Duration::from_secs(20), || server.established_connections() == 0),
        "the connection must be torn down once nothing is using it"
    );
}

#[test]
fn remote_forward_lets_the_server_reach_a_local_listener() {
    let server = TestServer::start();

    // `-R` is the direction where the *server* opens the channel, so unlike `-L`
    // it needs a live client-side handler to pipe into.
    let (target_port, greeting) = greeting_listener();
    // Any port will do — the container belongs to this test alone.
    let remote_port: u16 = 18022;

    let handle = spawn_tunnel_session(remote_forward_request(&server, remote_port, target_port))
        .expect("spawn tunnel");
    wait_tunnel_listening(&handle, Duration::from_secs(20));

    let (reached, got) = probe_forward_from_inside(&server, remote_port, &greeting);
    assert!(
        reached,
        "expected the local listener's greeting back through the remote forward, got: {got:?}"
    );
}

#[test]
fn remote_forward_works_on_the_shells_connection() {
    let server = TestServer::start();
    let mut request = LiveShellRequest::new("127.0.0.1", server.port, USER, PASS);
    request.known_hosts_path = temp_known_hosts();
    request.auto_accept_host_keys = true;

    let shell = spawn_password_shell(request).expect("spawn shell");
    let shared = wait_for_session(&shell, Duration::from_secs(20));

    let (target_port, greeting) = greeting_listener();
    let remote_port: u16 = 18023;
    let handle = spawn_tunnel_session_on(
        shared.clone(),
        remote_forward_request(&server, remote_port, target_port),
    )
    .expect("spawn the remote forward on the shared session");
    wait_tunnel_listening(&handle, Duration::from_secs(20));

    // This is the assertion the sibling test above cannot make. `-R` on a shared
    // connection used to reach exactly this far and no further: sshd listened,
    // Listening was reported, the UI showed a working forward — and every
    // forwarded channel was dropped, because the target was baked into the
    // handler at construction and the handler here belongs to the shell.
    let (reached, got) = probe_forward_from_inside(&server, remote_port, &greeting);
    assert!(
        reached,
        "a remote forward on the shell's connection must reach the local target, got: {got:?}"
    );

    // And it rode on the shell's connection rather than dialling its own.
    assert_eq!(
        server.established_connections(),
        1,
        "the forward must ride on the shell's connection"
    );

    drop(handle);
    let _ = shell.send(LiveShellCommand::Disconnect);
}
