//! End-to-end deadline regression using the production worker and loopback peers.
use super::*;
use adit_ssh::{AuthOptions, HostKeyDecision, HostKeyPrompt};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

const SHORT_RFB_TIMEOUT: Duration = Duration::from_secs(2);
const TEST_BOUND: Duration = Duration::from_secs(12);
const CHILD_ROOT: &str = "ADIT_VNC_GATEWAY_DEADLINE_TEST_ROOT";

// Environment variables are process-global. Run each scenario in its own test
// process so parallel tests cannot see, or modify, each other's home/config.
struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "adit-vnc-227-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(name);
        std::fs::create_dir(&root).expect("create unique VNC test sandbox");
        Self(root)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn isolated_child(test_name: &str) -> Option<PathBuf> {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        for name in [
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
            "ADIT_CONFIG_DIR",
            "TEMP",
            "TMP",
            "TMPDIR",
        ] {
            assert_eq!(std::env::var_os(name).as_deref(), Some(root.as_os_str()));
        }
        assert!(std::env::var_os("SSH_AUTH_SOCK").is_none());
        assert_eq!(
            std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap(),
            std::fs::canonicalize(&root).unwrap()
        );
        return Some(root);
    }

    let sandbox = Sandbox::new();
    let mut child = ProcessCommand::new(std::env::current_exe().unwrap());
    child
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env_clear()
        .env(CHILD_ROOT, &sandbox.0)
        .current_dir(&sandbox.0);
    for name in [
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "ADIT_CONFIG_DIR",
        "TEMP",
        "TMP",
        "TMPDIR",
    ] {
        child.env(name, &sandbox.0);
    }
    // Windows needs its system directory to load system libraries; no user
    // configuration, agent sockets, or other inherited environment is needed.
    for name in ["SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            child.env(name, value);
        }
    }
    let output = child.output().expect("run isolated VNC regression");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
        "isolated test was not selected: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.status.success(),
        "isolated test failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    None
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

struct Peer {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct Gateway {
    target_port: u16,
    opened: mpsc::UnboundedSender<()>,
}

impl russh::server::Handler for Gateway {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<russh::server::Auth, Self::Error> {
        Ok(
            if user == "fixture-user" && password == "fixture-password" {
                russh::server::Auth::Accept
            } else {
                russh::server::Auth::reject()
            },
        )
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: russh::Channel<russh::server::Msg>,
        host: &str,
        port: u32,
        _: &str,
        _: u32,
        _: &mut russh::server::Session,
    ) -> Result<bool, Self::Error> {
        if host != "127.0.0.1" || port != u32::from(self.target_port) {
            return Ok(false);
        }
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", self.target_port)).await?;
        let _ = self.opened.send(());
        tokio::spawn(async move {
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
        });
        Ok(true)
    }
}

async fn gateway(target_port: u16) -> (Peer, mpsc::UnboundedReceiver<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let key = russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[227; 32]);
    let config = Arc::new(russh::server::Config {
        keys: vec![key.into()],
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    let (opened, observed) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let server = Gateway {
            target_port,
            opened,
        };
        if let Ok(session) = russh::server::run_stream(config, socket, server).await {
            let _ = session.await;
        }
    });
    (Peer { port, task }, observed)
}

async fn read<const N: usize>(stream: &mut tokio::net::TcpStream) -> [u8; N] {
    let mut bytes = [0; N];
    stream.read_exact(&mut bytes).await.unwrap();
    bytes
}

// The stalled peer exchanges the RFB version before deliberately stopping;
// the working peer completes RFB 3.8 and paints one raw 2x2 framebuffer.
async fn rfb_peer(stalled: bool) -> (Peer, oneshot::Receiver<Instant>, oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (started, start_rx) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream.write_all(b"RFB 003.008\n").await.unwrap();
        assert_eq!(&read::<12>(&mut stream).await, b"RFB 003.008\n");
        if stalled {
            let _ = started.send(Instant::now());
            let _ = released.await;
            return;
        }
        stream.write_all(&[1, 1]).await.unwrap(); // security type None
        assert_eq!(read::<1>(&mut stream).await, [1]);
        stream.write_all(&0_u32.to_be_bytes()).await.unwrap();
        assert_eq!(read::<1>(&mut stream).await, [1]); // shared ClientInit
        stream.write_all(&2_u16.to_be_bytes()).await.unwrap();
        stream.write_all(&2_u16.to_be_bytes()).await.unwrap();
        stream
            .write_all(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0])
            .await
            .unwrap();
        stream.write_all(&4_u32.to_be_bytes()).await.unwrap();
        stream.write_all(b"test").await.unwrap();
        loop {
            match read::<1>(&mut stream).await[0] {
                0 => {
                    read::<19>(&mut stream).await;
                }
                2 => {
                    let header = read::<3>(&mut stream).await;
                    let mut encodings =
                        vec![0; usize::from(u16::from_be_bytes([header[1], header[2]])) * 4];
                    stream.read_exact(&mut encodings).await.unwrap();
                }
                3 => {
                    read::<9>(&mut stream).await;
                    break;
                }
                tag => panic!("unexpected RFB client message {tag}"),
            }
        }
        let mut frame = vec![0, 0, 0, 1];
        for value in [0_u16, 0, 2, 2] {
            frame.extend(value.to_be_bytes());
        }
        frame.extend(0_i32.to_be_bytes()); // raw encoding
        for _ in 0..4 {
            frame.extend([20, 40, 60, 0]);
        }
        stream.write_all(&frame).await.unwrap();
        let _ = started.send(Instant::now());
        let _ = released.await;
    });
    (Peer { port, task }, start_rx, release)
}

fn worker(gateway_port: u16, rfb_port: u16, sandbox: &Path) -> (VncClientHandle, PathBuf) {
    let known_hosts = sandbox.join("known_hosts");
    assert!(!known_hosts.exists());
    let mut request = LiveShellRequest::new(
        "127.0.0.1",
        gateway_port,
        "fixture-user",
        "fixture-password",
    );
    request.known_hosts_path = known_hosts.clone();
    request.connect_timeout_secs = 1;
    request.auth = AuthOptions {
        try_password: true,
        try_agent: false,
        try_default_keys: false,
        identity_file: None,
        ..Default::default()
    };
    let options = VncOptions {
        security: VncSecurity::AllowNone,
        auto_reconnect: false,
        ..Default::default()
    };
    let handle = spawn_vnc_session_with_timeout(
        "127.0.0.1".into(),
        rfb_port,
        String::new(),
        options,
        None,
        Some(request),
        SHORT_RFB_TIMEOUT,
    )
    .unwrap();
    (handle, known_hosts)
}

async fn event(handle: &VncClientHandle) -> RdpClientEvent {
    tokio::time::timeout(TEST_BOUND, async {
        loop {
            if let Some(event) = handle.try_recv() {
                return event;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("VNC worker event deadline")
}

async fn prompt(handle: &VncClientHandle, gateway_port: u16) -> HostKeyPrompt {
    match event(handle).await {
        RdpClientEvent::GatewayHostKey(prompt) => {
            assert_eq!(prompt.host, "127.0.0.1");
            assert_eq!(prompt.port, gateway_port);
            assert!(prompt.previous_fingerprint.is_none());
            prompt
        }
        other => panic!("expected gateway host-key prompt, got {other:?}"),
    }
}

#[test]
fn trust_wait_outlives_rfb_timeout() {
    let Some(sandbox) =
        isolated_child("vnc_client::gateway_deadline_tests::trust_wait_outlives_rfb_timeout")
    else {
        return;
    };
    runtime().block_on(async {
        let (rfb, ready, release) = rfb_peer(false).await;
        let (gateway, mut opened) = gateway(rfb.port).await;
        let (handle, known_hosts) = worker(gateway.port, rfb.port, &sandbox);
        let prompt = prompt(&handle, gateway.port).await;
        let started = Instant::now();
        tokio::time::sleep(SHORT_RFB_TIMEOUT + Duration::from_millis(800)).await;
        let waited = started.elapsed();
        assert!(waited > SHORT_RFB_TIMEOUT);
        assert!(prompt.is_pending(), "trust prompt expired during user wait");
        assert!(
            handle.try_recv().is_none(),
            "worker failed during trust wait"
        );
        assert!(matches!(
            opened.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(prompt.respond(HostKeyDecision::TrustOnce));
        tokio::time::timeout(TEST_BOUND, opened.recv())
            .await
            .expect("SSH direct-tcpip deadline")
            .expect("SSH direct-tcpip was not opened");
        match event(&handle).await {
            RdpClientEvent::Connected { width, height } => assert_eq!((width, height), (2, 2)),
            other => panic!("expected connected event, got {other:?}"),
        }
        tokio::time::timeout(TEST_BOUND, ready)
            .await
            .expect("RFB frame deadline")
            .expect("RFB peer closed before painting");
        tokio::time::timeout(TEST_BOUND, async {
            loop {
                if handle.frame_if_newer(0).is_some_and(|frame| {
                    (frame.width, frame.height) == (2, 2)
                        && frame
                            .rgba
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .all(|p| *p == [20, 40, 60, 255])
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("VNC worker did not paint the RFB frame");
        assert!(handle.diagnostics().security.starts_with("SSH + "));
        assert!(!known_hosts.exists(), "trust-once wrote known_hosts");
        eprintln!(
            "gateway trust waited {:?}; SSH tunnel connected and RFB frame painted",
            waited
        );
        handle.close();
        assert!(matches!(event(&handle).await, RdpClientEvent::Closed));
        let _ = release.send(());
    });
}

#[test]
fn stalled_rfb_after_gateway_still_times_out() {
    let Some(sandbox) = isolated_child(
        "vnc_client::gateway_deadline_tests::stalled_rfb_after_gateway_still_times_out",
    ) else {
        return;
    };
    runtime().block_on(async {
        let (rfb, started, release) = rfb_peer(true).await;
        let (gateway, mut opened) = gateway(rfb.port).await;
        let (handle, known_hosts) = worker(gateway.port, rfb.port, &sandbox);
        let prompt = prompt(&handle, gateway.port).await;
        assert!(prompt.respond(HostKeyDecision::TrustOnce));
        tokio::time::timeout(TEST_BOUND, opened.recv())
            .await
            .expect("SSH direct-tcpip deadline")
            .expect("SSH direct-tcpip was not opened");
        let started = tokio::time::timeout(TEST_BOUND, started)
            .await
            .expect("RFB version exchange deadline")
            .expect("RFB peer closed during version exchange");
        match event(&handle).await {
            RdpClientEvent::Error(message) => {
                assert!(
                    message.contains("VNC connect/handshake timeout"),
                    "{message}"
                );
            }
            other => panic!("expected RFB timeout, got {other:?}"),
        }
        assert!(started.elapsed() >= Duration::from_millis(900));
        assert!(
            !rfb.task.is_finished(),
            "stalled peer closed before the timeout"
        );
        assert!(matches!(event(&handle).await, RdpClientEvent::Closed));
        assert!(!known_hosts.exists(), "trust-once wrote known_hosts");
        eprintln!(
            "stalled RFB handshake timed out after {:?}",
            started.elapsed()
        );
        let _ = release.send(());
    });
}
