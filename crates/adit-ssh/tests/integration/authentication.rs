//! These are real OpenSSH/PAM tests, gated with the parent Docker suite.
//! All passwords, challenge answers and keys here are disposable fixture data.
use super::*;
use adit_domain::ProfileId;
use adit_ssh::{JumpCredential, TunnelCommand};
use std::process::Stdio;

fn password_only() -> AuthOptions {
    AuthOptions {
        try_password: true,
        try_agent: false,
        try_default_keys: false,
        identity_file: None,
        passphrase: None,
        agent_socket: None,
    }
}

fn docker_stdin(args: &[&str], input: &str) {
    let mut child = Command::new("docker")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fixture docker stdin");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "fixture setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

impl TestServer {
    fn deterministic_mfa(&self, password: &str, code: &str) {
        // Only this owned container's fixture account/config are changed.
        docker_stdin(
            &["exec", "-i", &self.id, "chpasswd"],
            &format!("{USER}:{password}\n"),
        );
        docker_stdin(
            &["exec", "-i", &self.id, "tee", "/etc/adit-fixture-code"],
            &format!("{code}\n"),
        );
        docker(&["exec", &self.id, "sh", "-c",
            "sed -i 's/^KbdInteractiveAuthentication no/KbdInteractiveAuthentication yes/; s/^UsePAM no/UsePAM yes/; s/^PasswordAuthentication yes/PasswordAuthentication no/' /etc/ssh/sshd_config && kill -HUP 1"
        ]).expect("enable deterministic PAM on owned fixture");
        self.wait_ready_internal(Duration::from_secs(15));
    }
}

fn sftp_request(server: &TestServer, password: &str) -> SftpRequest {
    let mut request = SftpRequest::new("127.0.0.1", server.port, USER, password);
    request.auth = password_only();
    request.known_hosts_path = temp_known_hosts();
    request
}

#[test]
fn standalone_sftp_and_tunnel_each_request_a_fresh_pam_challenge() {
    let server = TestServer::start();
    server.deterministic_mfa(PASS, "ADIT-DETERMINISTIC-SECOND-FACTOR");
    let sftp = spawn_sftp_session(sftp_request(&server, PASS)).unwrap();
    let mut first_prompt = None;
    let ready = pump_until(
        Duration::from_secs(30),
        || sftp.try_recv(),
        |event| match event {
            SftpEvent::AuthPrompt(p) => {
                assert!(p.instructions.contains("127.0.0.1"));
                assert_eq!(p.prompts.len(), 1);
                assert!(p.respond(vec!["ADIT-DETERMINISTIC-SECOND-FACTOR".to_owned()]));
                first_prompt = Some(p);
                ControlFlow::Continue(())
            }
            SftpEvent::Ready { .. } => ControlFlow::Break(true),
            SftpEvent::Error(e) => panic!("SFTP MFA failed: {e}"),
            SftpEvent::Closed => ControlFlow::Break(false),
            _ => ControlFlow::Continue(()),
        },
    );
    assert_eq!(ready, Some(true));
    let first_prompt = first_prompt.expect("SFTP must ask the GUI for its factor");
    sftp.send(SftpCommand::Disconnect).unwrap();
    assert!(pump_until(
        Duration::from_secs(10),
        || sftp.try_recv(),
        |e| {
            if matches!(e, SftpEvent::Closed) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        }
    )
    .is_some());

    let bind_port = free_local_port();
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
    request.auth = password_only();
    request.known_hosts_path = temp_known_hosts();
    let tunnel = spawn_tunnel_session(request).unwrap();
    let mut asked = false;
    let listening = pump_until(
        Duration::from_secs(30),
        || tunnel.try_recv(),
        |event| match event {
            TunnelEvent::AuthPrompt(p) => {
                assert_ne!(p.id, first_prompt.id);
                assert!(!first_prompt.respond(vec!["STALE-ANSWER".to_owned()]));
                assert!(p.respond(vec!["ADIT-DETERMINISTIC-SECOND-FACTOR".to_owned()]));
                asked = true;
                ControlFlow::Continue(())
            }
            TunnelEvent::Listening { .. } => ControlFlow::Break(true),
            TunnelEvent::Error(e) => panic!("tunnel MFA failed: {e}"),
            TunnelEvent::Stopped => ControlFlow::Break(false),
            _ => ControlFlow::Continue(()),
        },
    );
    assert_eq!(listening, Some(true));
    assert!(
        asked,
        "a new SSH connection must ask again, never reuse an OTP"
    );
    assert!(read_line(bind_port, Duration::from_secs(5))
        .unwrap()
        .starts_with("SSH-"));
    tunnel.send(TunnelCommand::Disconnect).unwrap();
}

#[test]
fn standalone_mfa_cancel_disconnect_and_wrong_answer_do_not_open_sftp() {
    let server = TestServer::start();
    server.deterministic_mfa(PASS, "EXPECTED-FACTOR");
    for mode in ["cancel", "disconnect", "wrong"] {
        let handle = spawn_sftp_session(sftp_request(&server, PASS)).unwrap();
        let mut prompt = None;
        let mut failed = false;
        let closed = pump_until(
            Duration::from_secs(30),
            || handle.try_recv(),
            |event| match event {
                SftpEvent::AuthPrompt(p) => {
                    match mode {
                        "cancel" => {
                            assert!(p.respond(Vec::new()));
                        }
                        "disconnect" => handle.send(SftpCommand::Disconnect).unwrap(),
                        _ => {
                            assert!(p.respond(vec!["WRONG-FACTOR".to_owned()]));
                        }
                    }
                    prompt = Some(p);
                    ControlFlow::Continue(())
                }
                SftpEvent::Ready { .. } => panic!("cancelled/rejected auth must not open SFTP"),
                SftpEvent::Error(_) => {
                    failed = true;
                    ControlFlow::Continue(())
                }
                SftpEvent::Closed => ControlFlow::Break(true),
                _ => ControlFlow::Continue(()),
            },
        );
        assert_eq!(closed, Some(true), "{mode} must end the handshake");
        assert!(!prompt.expect("fixture MFA challenge").is_pending());
        if mode != "disconnect" {
            assert!(failed);
        }
    }
}

#[test]
fn two_independent_jump_passwords_and_three_pam_conversations_reach_the_target() {
    let net = TestNetwork::create();
    let target = TestServer::start_on_network(&net, "target", false);
    target.wait_ready_internal(Duration::from_secs(30));
    let hop2 = TestServer::start_on_network(&net, "hop2", false);
    hop2.wait_ready_internal(Duration::from_secs(30));
    let hop1 = TestServer::start_on_network(&net, "hop1", true);
    hop1.wait_ready(Duration::from_secs(30));
    hop1.deterministic_mfa("HOP-ONE-PASSWORD", "HOP-ONE-FACTOR");
    hop2.deterministic_mfa("HOP-TWO-PASSWORD", "HOP-TWO-FACTOR");
    target.deterministic_mfa("DESTINATION-PASSWORD", "DESTINATION-FACTOR");
    docker(&["exec", &target.id, "touch", "/home/adit/only-on-target"]).unwrap();
    let first = ProfileId::new();
    let second = ProfileId::new();
    let mut request = SftpRequest::new("target", 2222, USER, "DESTINATION-PASSWORD");
    request.auth = password_only();
    request.known_hosts_path = temp_known_hosts();
    request.jumps = vec![
        JumpHop {
            host: "127.0.0.1".to_owned(),
            port: hop1.port,
            username: USER.to_owned(),
            auth_profile: Some(first),
        },
        JumpHop {
            host: "hop2".to_owned(),
            port: 2222,
            username: USER.to_owned(),
            auth_profile: Some(second),
        },
    ];
    for (id, password) in [(first, "HOP-ONE-PASSWORD"), (second, "HOP-TWO-PASSWORD")] {
        request.jump_credentials.insert(
            id,
            JumpCredential {
                username: USER.to_owned(),
                password: password.to_owned(),
                auth: password_only(),
            },
        );
    }
    let debug = format!("{request:?}");
    for secret in [
        "HOP-ONE-PASSWORD",
        "HOP-TWO-PASSWORD",
        "DESTINATION-PASSWORD",
    ] {
        assert!(!debug.contains(secret));
    }
    let handle = spawn_sftp_session(request).unwrap();
    let mut contexts = Vec::new();
    let found = pump_until(
        Duration::from_secs(60),
        || handle.try_recv(),
        |event| match event {
            SftpEvent::AuthPrompt(p) => {
                let (context, code) = if p.instructions.contains("跳板 1") {
                    ("hop1", "HOP-ONE-FACTOR")
                } else if p.instructions.contains("跳板 2") {
                    ("hop2", "HOP-TWO-FACTOR")
                } else {
                    assert!(p.instructions.contains("目标"));
                    ("target", "DESTINATION-FACTOR")
                };
                contexts.push(context);
                assert!(p.respond(vec![code.to_owned()]));
                ControlFlow::Continue(())
            }
            SftpEvent::Listing { entries, .. } => {
                if entries.iter().any(|e| e.name == "only-on-target") {
                    ControlFlow::Break(true)
                } else {
                    ControlFlow::Continue(())
                }
            }
            SftpEvent::Error(e) => panic!("independent jump authentication: {e}"),
            SftpEvent::Closed => ControlFlow::Break(false),
            _ => ControlFlow::Continue(()),
        },
    );
    assert_eq!(found, Some(true));
    assert_eq!(contexts, ["hop1", "hop2", "target"]);
    handle.send(SftpCommand::Disconnect).unwrap();
}

#[test]
fn cancellation_or_bad_credentials_on_second_jump_never_authenticate_the_target() {
    let net = TestNetwork::create();
    let target = TestServer::start_on_network(&net, "target", false);
    target.wait_ready_internal(Duration::from_secs(30));
    let middle = TestServer::start_on_network(&net, "middle", false);
    middle.wait_ready_internal(Duration::from_secs(30));
    let first = TestServer::start_on_network(&net, "first", true);
    first.wait_ready(Duration::from_secs(30));
    first.deterministic_mfa("FIRST-PASSWORD", "FIRST-FACTOR");
    middle.deterministic_mfa("MIDDLE-PASSWORD", "MIDDLE-FACTOR");
    for mode in [
        "cancel-second",
        "wrong-second-password",
        "wrong-second-code",
    ] {
        let first_owner = ProfileId::new();
        let middle_owner = ProfileId::new();
        let mut request = SftpRequest::new("target", 2222, USER, PASS);
        request.auth = password_only();
        request.known_hosts_path = temp_known_hosts();
        request.jumps = vec![
            JumpHop {
                host: "127.0.0.1".into(),
                port: first.port,
                username: USER.into(),
                auth_profile: Some(first_owner),
            },
            JumpHop {
                host: "middle".into(),
                port: 2222,
                username: USER.into(),
                auth_profile: Some(middle_owner),
            },
        ];
        request.jump_credentials.insert(
            first_owner,
            JumpCredential {
                username: USER.into(),
                password: "FIRST-PASSWORD".into(),
                auth: password_only(),
            },
        );
        request.jump_credentials.insert(
            middle_owner,
            JumpCredential {
                username: USER.into(),
                password: if mode == "wrong-second-password" {
                    "INCORRECT-PASSWORD"
                } else {
                    "MIDDLE-PASSWORD"
                }
                .into(),
                auth: password_only(),
            },
        );
        let handle = spawn_sftp_session(request).unwrap();
        let mut second_prompt = false;
        let mut error = String::new();
        let closed = pump_until(
            Duration::from_secs(30),
            || handle.try_recv(),
            |event| match event {
                SftpEvent::AuthPrompt(p) => {
                    let answers = if p.instructions.contains("跳板 1") {
                        vec!["FIRST-FACTOR".into()]
                    } else {
                        assert!(p.instructions.contains("跳板 2"));
                        second_prompt = true;
                        match mode {
                            "cancel-second" => Vec::new(),
                            "wrong-second-code" => vec!["INCORRECT-FACTOR".into()],
                            _ => vec!["MIDDLE-FACTOR".into()],
                        }
                    };
                    assert!(p.respond(answers));
                    ControlFlow::Continue(())
                }
                SftpEvent::Ready { .. } => panic!("failed jump must not open target SFTP"),
                SftpEvent::Error(e) => {
                    error = e;
                    ControlFlow::Continue(())
                }
                SftpEvent::Closed => ControlFlow::Break(()),
                _ => ControlFlow::Continue(()),
            },
        );
        assert!(closed.is_some());
        assert!(second_prompt);
        assert!(!error.is_empty());
        if mode != "cancel-second" {
            assert!(error.contains("jump 2"));
        }
        assert!(
            !target.logs().contains("Accepted "),
            "destination credentials must never be attempted"
        );
    }
}

fn socks_banner(proxy_port: u16, address: &[u8], port: u16) {
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(&[5, 1, 0]).unwrap();
    let mut greeting = [0; 2];
    stream.read_exact(&mut greeting).unwrap();
    assert_eq!(greeting, [5, 0]);
    let mut request = vec![5, 1, 0];
    request.extend_from_slice(address);
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).unwrap();
    let mut reply = [0; 4];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0..2], [5, 0]);
    let len = match reply[3] {
        1 => 4,
        4 => 16,
        other => panic!("unexpected reply address: {other}"),
    };
    let mut rest = vec![0; len + 2];
    stream.read_exact(&mut rest).unwrap();
    let mut banner = [0; 4];
    stream.read_exact(&mut banner).unwrap();
    assert_eq!(&banner, b"SSH-");
}

#[test]
fn dynamic_socks5_ipv4_and_remote_dns_reach_real_sshd() {
    let net = TestNetwork::create();
    let target = TestServer::start_on_network(&net, "target", false);
    target.wait_ready_internal(Duration::from_secs(30));
    let bastion = TestServer::start_on_network(&net, "proxy", true);
    bastion.wait_ready(Duration::from_secs(30));
    let port = free_local_port();
    let mut request = TunnelRequest::new(
        "127.0.0.1",
        bastion.port,
        USER,
        PASS,
        TunnelKind::Dynamic,
        "127.0.0.1",
        port,
        "",
        0,
    );
    request.auth = password_only();
    request.known_hosts_path = temp_known_hosts();
    let handle = spawn_tunnel_session(request).unwrap();
    wait_tunnel_listening(&handle, Duration::from_secs(20));
    socks_banner(port, &[1, 127, 0, 0, 1], 2222);
    socks_banner(port, &[3, 6, b't', b'a', b'r', b'g', b'e', b't'], 2222);
    handle.send(TunnelCommand::Disconnect).unwrap();
}

#[cfg(unix)]
#[test]
fn isolated_ssh_agent_authenticates_to_real_sshd_without_process_global_env() {
    struct Agent {
        child: std::process::Child,
        socket: PathBuf,
    }
    impl Drop for Agent {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = fs::remove_file(&self.socket);
        }
    }
    let server = TestServer::start();
    let keys = server.generate_key();
    let socket = temp_path("agent.sock");
    let child = Command::new("ssh-agent")
        .arg("-D")
        .arg("-a")
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start owned ssh-agent");
    let agent = Agent { child, socket };
    assert!(wait_until(Duration::from_secs(5), || agent.socket.exists()));
    let loaded = Command::new("ssh-add")
        .arg(&keys.private)
        .env("SSH_AUTH_SOCK", &agent.socket)
        .stdin(Stdio::null())
        .output()
        .expect("load fixture key into owned agent");
    assert!(loaded.status.success(), "isolated ssh-add failed");
    let mut request = LiveShellRequest::new("127.0.0.1", server.port, USER, "");
    request.auth = AuthOptions {
        try_password: false,
        try_agent: true,
        try_default_keys: false,
        identity_file: None,
        passphrase: None,
        agent_socket: Some(agent.socket.clone()),
    };
    request.known_hosts_path = temp_known_hosts();
    request.auto_accept_host_keys = true;
    let handle = spawn_password_shell(request).unwrap();
    let shared = wait_for_session(&handle, Duration::from_secs(20));
    assert!(!shared.is_closed());
    assert!(server.logs().contains("Accepted publickey for adit"));
    handle.send(LiveShellCommand::Disconnect).unwrap();
    drop(shared);
    let _ = fs::remove_file(&keys.private);
    let _ = fs::remove_file(&keys.public);
}
