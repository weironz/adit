//! Real, isolated SSH -> loopback-only VNC integration. Start tests/vnc first.
//! Credentials here are disposable fixture constants, never real host secrets.
use adit_ssh::{
    desktop_gateway, HostKeyDecision, LiveShellCommand, LiveShellEvent, LiveShellRequest, SshError,
};
use std::{sync::mpsc, time::Duration};
use tokio::sync::mpsc as tmpsc;
use vnc::{VncConnector, VncEncoding, VncEvent};

async fn gateway(
    password: &str,
    decision: HostKeyDecision,
) -> Result<desktop_gateway::GatewayStream, SshError> {
    gateway_to(password, decision, 15900).await
}
async fn gateway_to(
    password: &str,
    decision: HostKeyDecision,
    port: u16,
) -> Result<desktop_gateway::GatewayStream, SshError> {
    let mut request = LiveShellRequest::new("127.0.0.1", 12222, "adit-test", password);
    request.known_hosts_path = std::env::temp_dir().join(format!(
        "adit-gateway-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    request.connect_timeout_secs = 5;
    let (tx, rx) = tmpsc::unbounded_channel();
    let (events, receiver) = mpsc::channel();
    let connect = desktop_gateway::connect(request, "127.0.0.1".into(), port, rx, events);
    tokio::pin!(connect);
    let mut prompted = false;
    loop {
        tokio::select! {
            result = &mut connect => { assert!(prompted, "unknown gateway must prompt even with terminal TOFU enabled"); return result; }
            _ = tokio::time::sleep(Duration::from_millis(5)) => for event in receiver.try_iter() {
                if matches!(event, LiveShellEvent::HostKeyPrompt(_)) {
                    prompted = true;
                    tx.send(LiveShellCommand::HostKeyDecision(decision)).unwrap();
                }
            }
        }
    }
}

#[tokio::test]
#[ignore = "needs isolated Docker fixture tests/vnc with host ports 12222 and 15901"]
async fn ssh_direct_tcpip_reaches_an_unpublished_vnc_port_and_checks_both_identities() {
    // Only 15901 is published. The VNC target 15900 is reachable via SSH only.
    assert!(tokio::net::TcpStream::connect("127.0.0.1:15900")
        .await
        .is_err());
    let stream = gateway("adit-test-isolated", HostKeyDecision::TrustOnce)
        .await
        .unwrap();
    let client = VncConnector::new(stream)
        .set_auth_method(async { Ok("adit-ci".to_owned()) })
        .add_encoding(VncEncoding::Tight)
        .add_encoding(VncEncoding::Raw)
        .set_pixel_format(vnc::PixelFormat::rgba())
        .allow_shared(true)
        .build()
        .unwrap()
        .try_start()
        .await
        .unwrap()
        .finish()
        .unwrap();
    let paint = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.recv_event().await.unwrap() {
                VncEvent::RawImage(rect, pixels) if rect.width > 0 && !pixels.is_empty() => break,
                VncEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await;
    assert!(paint.is_ok(), "SSH tunneled VNC did not paint");
    client.close().await.unwrap();
    assert!(matches!(
        gateway("wrong-ssh-password", HostKeyDecision::TrustOnce).await,
        Err(SshError::AuthenticationRejected)
    ));
    assert!(matches!(
        gateway("adit-test-isolated", HostKeyDecision::Reject).await,
        Err(SshError::Russh(_)) | Err(SshError::HostKeyRejected)
    ));
}

// Every process/port below belongs to the explicitly selected disposable fixture.
const FIXTURE: &str = "adit-vnc-integration";
const GATEWAY_VNC: u16 = 15909;
fn docker(args: &[&str]) -> String {
    let out = std::process::Command::new("docker")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fixture command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
struct ControlProcess {
    child: std::process::Child,
    input: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<String>,
    fixture: bool,
}
impl ControlProcess {
    fn new(args: &[&str], fixture: bool) -> Self {
        use std::io::BufRead;
        let mut child = std::process::Command::new("docker")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let input = child.stdin.take();
        let process = Self {
            child,
            input,
            lines,
            fixture,
        };
        assert_eq!(
            process.lines.recv_timeout(Duration::from_secs(10)).unwrap(),
            "READY"
        );
        process
    }
    fn exchange(&mut self, line: &str) -> String {
        use std::io::Write;
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{line}").unwrap();
        input.flush().unwrap();
        self.lines.recv_timeout(Duration::from_secs(5)).unwrap()
    }
}
impl Drop for ControlProcess {
    fn drop(&mut self) {
        if self.fixture {
            let _ = std::process::Command::new("docker").args(["exec", FIXTURE, "sh", "-c",
            "test ! -f /tmp/adit-gateway-fixture.pid || kill -TERM \"$(cat /tmp/adit-gateway-fixture.pid)\""
        ]).status();
        }
        drop(self.input.take());
        for _ in 0..50 {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn ssh_connections() -> std::collections::BTreeSet<String> {
    docker(&["exec", FIXTURE, "python3", "-c", "from pathlib import Path; print(chr(10).join(r.split()[2] for f in ['/proc/net/tcp','/proc/net/tcp6'] for r in Path(f).read_text().splitlines()[1:] if r.split()[1].endswith(':0016') and r.split()[3]=='01'))"])
        .lines().filter(|s| !s.is_empty()).map(str::to_owned).collect()
}
async fn wait_connections_closed(owned: &std::collections::BTreeSet<String>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if ssh_connections().is_disjoint(owned) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("SSH transport survived cancellation or VNC close");
}
fn hex(text: &str) -> String {
    text.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs isolated tests/vnc Docker fixture and python3-tk; owns :109 / 15909"]
async fn real_ssh_gateway_keyboard_clipboard_cancel_disconnect_and_no_direct_fallback() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    docker(&[
        "cp",
        root.join("tests/vnc/start-gateway.sh").to_str().unwrap(),
        "adit-vnc-integration:/tmp/adit-start-gateway.sh",
    ]);
    docker(&[
        "cp",
        root.join("tests/vnc/clipboard_target.py").to_str().unwrap(),
        "adit-vnc-integration:/tmp/adit-gateway-clipboard.py",
    ]);
    let _fixture = ControlProcess::new(
        &["exec", "-i", FIXTURE, "sh", "/tmp/adit-start-gateway.sh"],
        true,
    );
    let mut clipboard = ControlProcess::new(
        &[
            "exec",
            "-i",
            FIXTURE,
            "python3",
            "/tmp/adit-gateway-clipboard.py",
            "109",
        ],
        false,
    );
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", GATEWAY_VNC))
        .await
        .is_err());
    let before = ssh_connections();
    let stream = gateway_to(
        "adit-test-isolated",
        HostKeyDecision::TrustOnce,
        GATEWAY_VNC,
    )
    .await
    .unwrap();
    let connected: std::collections::BTreeSet<_> =
        ssh_connections().difference(&before).cloned().collect();
    assert_eq!(connected.len(), 1, "identify the owned SSH transport");
    let client = VncConnector::new(stream)
        .set_auth_method(async { Ok("adit-ci".into()) })
        .add_encoding(VncEncoding::Raw)
        .add_encoding(VncEncoding::ExtendedClipboard)
        .set_pixel_format(vnc::PixelFormat::rgba())
        .build()
        .unwrap()
        .try_start()
        .await
        .unwrap()
        .finish()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut painted = false;
        let mut utf8 = false;
        while !painted || !utf8 {
            match client.recv_event().await.unwrap() {
                VncEvent::RawImage(_, bytes) if !bytes.is_empty() => painted = true,
                VncEvent::ClipboardCapabilities { utf8: true } => utf8 = true,
                VncEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("TigerVNC over SSH must paint and negotiate UTF-8");
    for down in [true, false] {
        client
            .input(vnc::X11Event::KeyEvent((0x71, down).into()))
            .await
            .unwrap();
    }
    for buttons in [0, 1, 0] {
        client
            .input(vnc::X11Event::PointerEvent((40, 50, buttons).into()))
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let events = docker(&["exec", FIXTURE, "cat", "/tmp/adit-gateway-events.log"]);
            if events.contains("KeyPress event")
                && events.contains("KeyRelease event")
                && events.matches("keysym 0x71, q").count() >= 2
                && events.contains("MotionNotify event")
                && events.contains("ButtonPress event")
                && events.contains("ButtonRelease event")
                && events.matches("(40,50)").count() >= 3
                && events.matches("button 1").count() >= 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("keyboard and mouse edges/coordinates must reach the actual X11 target");
    let remote_text = "ADIT_GATEWAY:远端🙂\nsecond line";
    assert_eq!(
        clipboard.exchange(&format!("SET {}", hex(remote_text))),
        "OK"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.recv_event().await.unwrap() {
                VncEvent::Text(receipt) => {
                    if receipt.into_current_text().as_deref() == Some(remote_text) {
                        break;
                    }
                }
                VncEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("remote UTF-8 clipboard did not cross SSH");
    let local_text = "ADIT_GATEWAY:本地🚀\nline two";
    client
        .input(vnc::X11Event::CopyText(local_text.into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            while client.poll_event().await.unwrap().is_some() {}
            if clipboard.exchange("GET") == format!("TEXT {}", hex(local_text)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("client text must become the real X11 clipboard ownership");
    client.close().await.unwrap();
    drop(client);
    wait_connections_closed(&connected).await;

    changed_gateway_key_is_rejected_without_recording().await;
    // Cancel after SSH and RFB negotiation, while the password callback is pending.
    let before = ssh_connections();
    let stream = gateway_to(
        "adit-test-isolated",
        HostKeyDecision::TrustOnce,
        GATEWAY_VNC,
    )
    .await
    .unwrap();
    let handshake_connections: std::collections::BTreeSet<_> =
        ssh_connections().difference(&before).cloned().collect();
    assert_eq!(handshake_connections.len(), 1);
    let reached_auth = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = std::sync::Arc::clone(&reached_auth);
    let mut handshake = VncConnector::new(stream)
        .set_auth_method(async move {
            reached_auth.store(true, std::sync::atomic::Ordering::Release);
            std::future::pending::<Result<String, vnc::VncError>>().await
        })
        .add_encoding(VncEncoding::Raw)
        .build()
        .unwrap()
        .try_start();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop { tokio::select! {
            result = &mut handshake => panic!("handshake should wait for password: {:?}", result.err()),
            _ = tokio::time::sleep(Duration::from_millis(5)) => if observed.load(std::sync::atomic::Ordering::Acquire) { break; }
        } }
    }).await.expect("must enter VNC authentication before cancellation");
    drop(handshake);
    wait_connections_closed(&handshake_connections).await;

    let before = ssh_connections();
    let stream = gateway_to(
        "adit-test-isolated",
        HostKeyDecision::TrustOnce,
        GATEWAY_VNC,
    )
    .await
    .unwrap();
    let rejected_connections: std::collections::BTreeSet<_> =
        ssh_connections().difference(&before).cloned().collect();
    assert_eq!(rejected_connections.len(), 1);
    let rejected = VncConnector::new(stream)
        .set_auth_method(async { Ok("wrong-vnc-password".into()) })
        .add_encoding(VncEncoding::Raw)
        .set_pixel_format(vnc::PixelFormat::rgba())
        .build()
        .unwrap()
        .try_start()
        .await;
    assert!(
        matches!(rejected, Err(vnc::VncError::WrongPassword)),
        "SSH must not bypass VNC password authentication"
    );
    wait_connections_closed(&rejected_connections).await;
    // A local listener traps forbidden direct fallback when the remote port is closed.
    let trap = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let failure = gateway_to(
        "adit-test-isolated",
        HostKeyDecision::TrustOnce,
        trap.local_addr().unwrap().port(),
    )
    .await;
    assert!(failure.is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(200), trap.accept())
            .await
            .is_err(),
        "direct fallback leaked a connection"
    );
    for explicit_disconnect in [true, false] {
        let before = ssh_connections();
        let mut request =
            LiveShellRequest::new("127.0.0.1", 12222, "adit-test", "adit-test-isolated");
        request.known_hosts_path =
            std::env::temp_dir().join(format!("adit-gateway-cancel-{}", unique_id()));
        request.connect_timeout_secs = 5;
        let (commands, receiver) = tmpsc::unbounded_channel();
        let (events, received) = mpsc::channel();
        let mut pending = Box::pin(desktop_gateway::connect(
            request,
            "127.0.0.1".into(),
            GATEWAY_VNC,
            receiver,
            events,
        ));
        let cancelled = tokio::time::timeout(Duration::from_secs(5), async {
            loop { tokio::select! {
                result = &mut pending => panic!("connect returned before prompt: {:?}", result.err()),
                _ = tokio::time::sleep(Duration::from_millis(5)) => if received.try_iter().any(|e|matches!(e,LiveShellEvent::HostKeyPrompt(_))) {
                    let owned: std::collections::BTreeSet<_> = ssh_connections().difference(&before).cloned().collect();
                    assert_eq!(owned.len(), 1);
                    if explicit_disconnect { commands.send(LiveShellCommand::Disconnect).unwrap(); }
                    break owned;
                }
            } }
        }).await.unwrap();
        if explicit_disconnect {
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(1), &mut pending)
                    .await
                    .unwrap(),
                Err(SshError::AuthenticationCancelled)
            ));
        }
        drop(pending);
        wait_connections_closed(&cancelled).await;
    }
    established_tunnel_interruption_reaches_session_error().await;
}

fn unique_id() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
struct ScratchKnownHosts(std::path::PathBuf);
impl Drop for ScratchKnownHosts {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
async fn changed_gateway_key_is_rejected_without_recording() {
    // A deliberately different public key; no private key or user trust file is edited.
    let old_key = docker(&["exec", FIXTURE, "python3", "-c", "from pathlib import Path; import base64; p=Path('/etc/ssh/ssh_host_ed25519_key.pub').read_text().split(); b=bytearray(base64.b64decode(p[1])); b[-1]^=1; print(p[0]+' '+base64.b64encode(b).decode())"]);
    let scratch = ScratchKnownHosts(
        std::env::temp_dir().join(format!("adit-gateway-changed-{}", unique_id())),
    );
    let previous = format!("[127.0.0.1]:12222 {}\n", old_key.trim());
    std::fs::write(&scratch.0, &previous).unwrap();
    let mut request = LiveShellRequest::new("127.0.0.1", 12222, "adit-test", "adit-test-isolated");
    request.known_hosts_path = scratch.0.clone();
    request.connect_timeout_secs = 5;
    let (commands, receiver) = tmpsc::unbounded_channel();
    let (events, received) = mpsc::channel();
    let mut connect = Box::pin(desktop_gateway::connect(
        request,
        "127.0.0.1".into(),
        GATEWAY_VNC,
        receiver,
        events,
    ));
    let changed=tokio::time::timeout(Duration::from_secs(5),async {
        loop { tokio::select! {
            result=&mut connect=>panic!("changed key bypassed prompt: {:?}",result.err()),
            _=tokio::time::sleep(Duration::from_millis(5))=>{
                if let Some(prompt)=received.try_iter().find_map(|e|if let LiveShellEvent::HostKeyPrompt(p)=e {Some(p)}else{None}) {
                    assert!(prompt.previous_fingerprint.is_some(),"changed key must differ from first use");
                    commands.send(LiveShellCommand::HostKeyDecision(HostKeyDecision::Reject)).unwrap(); break prompt;
                }
            }
        } }
    }).await.unwrap();
    assert_ne!(
        changed.previous_fingerprint.as_deref(),
        Some(changed.fingerprint.as_str())
    );
    assert!(tokio::time::timeout(Duration::from_secs(2), &mut connect)
        .await
        .unwrap()
        .is_err());
    drop(connect);
    assert_eq!(
        std::fs::read_to_string(&scratch.0).unwrap(),
        previous,
        "reject must preserve old recorded trust"
    );
}
fn ssh_privileged_workers() -> std::collections::BTreeSet<String> {
    let script = r#"from pathlib import Path
for p in Path('/proc').iterdir():
 if p.name.isdigit():
  try:
   if (p/'cmdline').read_bytes().startswith(b'sshd: adit-test [priv]'): print(p.name)
  except OSError: pass"#;
    docker(&["exec", FIXTURE, "python3", "-c", script])
        .lines()
        .map(str::to_string)
        .collect()
}
async fn established_tunnel_interruption_reaches_session_error() {
    use adit_domain::{ConnectionProfile, Protocol, SessionStatus};
    use adit_session::{SessionManager, VncGatewayCredentials};
    let gateway = ConnectionProfile::new("isolated SSH", "127.0.0.1", 12222, "adit-test");
    let mut target =
        ConnectionProfile::new("isolated interrupted desktop", "127.0.0.1", GATEWAY_VNC, "");
    target.protocol = Protocol::Vnc;
    target.vnc.gateway_profile = Some(gateway.id);
    target.vnc.auto_reconnect = false;
    let profile_id = target.id;
    let before = ssh_connections();
    let workers_before = ssh_privileged_workers();
    let mut manager = SessionManager::with_profiles(vec![gateway, target]);
    manager
        .open_live_vnc_session_via(
            profile_id,
            "adit-ci".into(),
            Some(VncGatewayCredentials {
                password: "adit-test-isolated".into(),
                passphrase: String::new(),
            }),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            manager.poll_events();
            if let Some((session, _)) = manager.pending_host_key() {
                manager
                    .respond_host_key(session, HostKeyDecision::TrustOnce)
                    .unwrap();
            }
            assert!(
                manager.active_desktop_error().is_none(),
                "fixture must connect before interruption"
            );
            if manager.active_rdp_frame_if_newer(0).is_some_and(|frame| {
                frame.width == 800 && frame.height == 600 && frame.rgba.iter().any(|b| *b != 0)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("application must paint through SSH before interruption");
    let owned: std::collections::BTreeSet<_> =
        ssh_connections().difference(&before).cloned().collect();
    assert_eq!(owned.len(), 1);
    // Privilege-separated sshd workers hide their FD symlinks in ordinary
    // containers. Correlate the single newly established socket with the single
    // new fixture worker instead; never signal the listener or an older worker.
    let workers = ssh_privileged_workers();
    let created: Vec<_> = workers.difference(&workers_before).collect();
    assert_eq!(
        created.len(),
        1,
        "one new fixture SSH worker must own this transport"
    );
    let script = r#"from pathlib import Path
import os,signal,sys
pid=int(sys.argv[1])
assert (Path('/proc')/str(pid)/'cmdline').read_bytes().startswith(b'sshd: adit-test [priv]')
children=[]
for p in Path('/proc').iterdir():
 if p.name.isdigit():
  try:
   status=(p/'status').read_text().splitlines()
   parent=int(next(row.split()[1] for row in status if row.startswith('PPid:')))
   if parent==pid and (p/'cmdline').read_bytes().startswith(b'sshd: adit-test'): children.append(int(p.name))
  except OSError: pass
assert len(children)==1, 'the new privileged worker must have exactly one transport child'
for target in children+[pid]:
 try: os.kill(target,signal.SIGKILL)
 except ProcessLookupError: pass"#;
    docker(&["exec", FIXTURE, "python3", "-c", script, created[0]]);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            manager.poll_events();
            if manager
                .active_desktop_error()
                .is_some_and(|error| !error.trim().is_empty())
                && manager.active_session_summary().is_some_and(|s| {
                    matches!(s.status, SessionStatus::Disconnected | SessionStatus::Error)
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("interruption must become a visible error and disconnected session");
    wait_connections_closed(&owned).await;
}
