use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

const FIXTURE_REASON: &str =
    "build proxy_stdio_fixture and set ADIT_PROXY_FIXTURE; synthetic processes only";

struct Fixture {
    dir: PathBuf,
    request: LiveShellRequest,
}
impl Fixture {
    fn new(mode: &str) -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let dir = env::temp_dir().join(format!(
            "adit proxy fixture {} {} {}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let program = dir.join(if cfg!(windows) {
            "test proxy.exe"
        } else {
            "test proxy"
        });
        fs::copy(
            env::var_os("ADIT_PROXY_FIXTURE").expect(FIXTURE_REASON),
            &program,
        )
        .unwrap();
        let mut request = LiveShellRequest::new(
            "127.0.0.1",
            2222,
            "synthetic-user",
            "synthetic-never-in-argv",
        );
        request.auth.try_agent = false;
        request.auth.try_default_keys = false;
        request.proxy_command = adit_domain::ProxyCommand {
            enabled: true,
            program: program.to_str().unwrap().into(),
            args: vec![mode.into(), dir.join("pid").to_str().unwrap().into()],
        };
        request.known_hosts_path = dir.join("known_hosts");
        Self { dir, request }
    }
    async fn pid(&self, suffix: &str) -> u32 {
        let path = self.dir.join(format!("pid{suffix}"));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(s) = fs::read_to_string(&path) {
                    if let Ok(pid) = s.parse() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child started")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

#[cfg(windows)]
fn assert_dead(pid: u32) {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, WaitForSingleObject},
    };
    // SYNCHRONIZE: wait for this exact process, rather than trusting an event.
    let handle = unsafe { OpenProcess(0x0010_0000, 0, pid) };
    if !handle.is_null() {
        let status = unsafe { WaitForSingleObject(handle, 5000) };
        unsafe {
            CloseHandle(handle);
        }
        assert_eq!(status, WAIT_OBJECT_0, "fixture process {pid} still running");
    }
}
#[cfg(unix)]
fn assert_dead(pid: u32) {
    assert_eq!(
        unsafe { libc::kill(pid as i32, 0) },
        -1,
        "fixture still running"
    );
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[test]
#[ignore = "requires ADIT_PROXY_FIXTURE"]
fn process_stream_preserves_binary_data_and_windows_argv() {
    runtime().block_on(async {
        let fixture = Fixture::new("echo");
        let mut process = Process::spawn(&fixture.request).await.unwrap();
        let pid = fixture.pid("").await;
        let mut stream = process.io.take().unwrap();
        let data = b"\0SSH-2.0-synthetic\r\n\xff\x80\n";
        stream.write_all(data).await.unwrap();
        stream.flush().await.unwrap();
        let mut out = vec![0; data.len()];
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut out))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out, data);
        stream.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while process.owner.child.lock().unwrap().is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("stdin shutdown must let echo peer exit");
        drop(process);
        assert_dead(pid);
        drop(stream);

        let mut fixture = Fixture::new("inspect");
        let expected = [
            "",
            "a b",
            "quote\"injection & calc",
            "C:\\a path\\trailing\\",
            "%h",
            "user=synthetic-user",
            "127.0.0.1:2222",
        ];
        fixture.request.proxy_command.args.extend(
            [
                "",
                "a b",
                "quote\"injection & calc",
                "C:\\a path\\trailing\\",
                "%%h",
                "user=%r",
                "%h:%p",
            ]
            .map(String::from),
        );
        // A synthetic canary, not a real credential. No other environment value is inspected.
        env::set_var("ADIT_PROXY_PARENT_SECRET", "synthetic-canary");
        let mut process = Process::spawn(&fixture.request).await.unwrap();
        env::remove_var("ADIT_PROXY_PARENT_SECRET");
        let mut stream = process.io.take().unwrap();
        for expected in expected {
            let len = stream.read_u32_le().await.unwrap();
            let mut value = vec![0; len as usize];
            stream.read_exact(&mut value).await.unwrap();
            assert_eq!(value, expected.as_bytes());
        }
        drop(stream);
        drop(process);
    });
}

#[test]
#[ignore = "requires ADIT_PROXY_FIXTURE"]
fn cancellation_timeout_and_failed_handshake_reap_processes() {
    runtime().block_on(async {
        let fixture = Fixture::new("exit");
        let process = Process::spawn(&fixture.request).await.unwrap();
        let pid = fixture.pid("").await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while process.owner.child.lock().unwrap().is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("exit must be reaped even without another IO operation");
        assert_dead(pid);
        drop(process);
        for mode in ["stall", "exit", "echo"] {
            let fixture = Fixture::new(mode);
            let mut process = None;
            let handler = KnownHostsClient::new(
                fixture.request.host.clone(),
                fixture.request.port,
                fixture.request.known_hosts_path.clone(),
                None,
                None,
            );
            let result = tokio::time::timeout(
                Duration::from_millis(600),
                connect_endpoint(
                    &fixture.request,
                    Arc::new(client::Config::default()),
                    handler,
                    &mut process,
                ),
            )
            .await;
            let pid = fixture.pid("").await;
            assert!(
                result.is_err() || result.unwrap().is_err(),
                "fixture must not complete SSH"
            );
            drop(process);
            assert_dead(pid);
        }
        let fixture = Fixture::new("stall");
        let (commands, rx) = tokio_mpsc::unbounded_channel();
        let (events, _rx) = mpsc::channel();
        let task = tokio::spawn(run_live_password_shell(fixture.request.clone(), rx, events));
        let pid = fixture.pid("").await;
        commands.send(LiveShellCommand::Disconnect).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_dead(pid);
    });
}

#[cfg(windows)]
#[test]
#[ignore = "requires ADIT_PROXY_FIXTURE"]
fn cancellation_kills_proxy_descendants() {
    runtime().block_on(async {
        let fixture = Fixture::new("descendant");
        let process = Process::spawn(&fixture.request).await.unwrap();
        let parent = fixture.pid("").await;
        let child = fixture.pid(".child").await;
        drop(process);
        assert_dead(parent);
        assert_dead(child);
    });
}

#[test]
fn disabled_and_conflicting_routes_do_not_spawn() {
    runtime().block_on(async {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let mut request = LiveShellRequest::new(
            "127.0.0.1",
            listener.local_addr().unwrap().port(),
            "test",
            "",
        );
        // A disabled, invalid draft must neither execute nor prevent direct TCP.
        request.proxy_command.program = "not-an-executable".into();
        request.proxy_command.args = vec!["%unsupported".into()];
        let server = tokio::spawn(async move {
            listener.accept().await.unwrap();
        });
        let handler = KnownHostsClient::new(
            request.host.clone(),
            request.port,
            PathBuf::new(),
            None,
            None,
        );
        let mut process = None;
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            connect_endpoint(
                &request,
                Arc::new(client::Config::default()),
                handler,
                &mut process,
            ),
        )
        .await;
        assert!(
            result.unwrap().is_err(),
            "synthetic TCP peer supplies no SSH banner"
        );
        server.await.unwrap();
        assert!(process.is_none(), "disabled routes must not spawn a child");
    });
    let mut request = LiveShellRequest::new("host", 22, "user", "");
    request.proxy_command.program = "does-not-exist.exe".into();
    assert!(validate_live_request(&request).is_ok());
    request.proxy_command.enabled = true;
    assert!(validate_live_request(&request).is_err());
    request.proxy_command.program = if cfg!(windows) {
        "C:\\proxy.exe"
    } else {
        "/proxy"
    }
    .into();
    request.jumps.push(JumpHop::parse("bastion").unwrap());
    assert!(matches!(
        validate_live_request(&request),
        Err(SshError::ProxyCommand(_))
    ));
    assert!(matches!(
        spawn_password_shell(request.clone()),
        Err(SshError::ProxyCommand(_))
    ));
    assert!(
        auxiliary::sftp_request(&{
            let mut r = SftpRequest::new("host", 22, "user", "");
            r.proxy_command = request.proxy_command.clone();
            r
        })
        .proxy_command
        .enabled
    );
}

#[test]
#[ignore = "requires ADIT_PROXY_FIXTURE"]
fn auxiliary_and_gateway_cancellation_close_stdio() {
    runtime().block_on(async {
        let fixture = Fixture::new("stall");
        let mut request = SftpRequest::new("127.0.0.1", 2222, "synthetic-user", "");
        request.proxy_command = fixture.request.proxy_command.clone();
        let (tx, rx) = tokio_mpsc::unbounded_channel();
        let (events, _events) = mpsc::channel();
        let task = tokio::spawn(run_sftp_session(
            request,
            rx,
            events,
            Arc::new(Mutex::new(HashSet::new())),
        ));
        let pid = fixture.pid("").await;
        tx.send(SftpCommand::Disconnect).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_dead(pid);

        let fixture = Fixture::new("stall");
        let mut request = TunnelRequest::new(
            "127.0.0.1",
            2222,
            "synthetic-user",
            "",
            TunnelKind::Dynamic,
            "127.0.0.1",
            1080,
            "",
            0,
        );
        request.proxy_command = fixture.request.proxy_command.clone();
        let (tx, rx) = tokio_mpsc::unbounded_channel();
        let (events, _events) = mpsc::channel();
        let task = tokio::spawn(run_tunnel_session(request, rx, events));
        let pid = fixture.pid("").await;
        tx.send(TunnelCommand::Disconnect).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_dead(pid);

        let fixture = Fixture::new("stall");
        let (tx, rx) = tokio_mpsc::unbounded_channel();
        let (events, _events) = mpsc::channel();
        let task = tokio::spawn(desktop_gateway::connect(
            fixture.request.clone(),
            "127.0.0.1".into(),
            5900,
            rx,
            events,
        ));
        let pid = fixture.pid("").await;
        tx.send(LiveShellCommand::Disconnect).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(SshError::AuthenticationCancelled)));
        assert_dead(pid);
    });
}

struct EchoServer;
impl russh::server::Handler for EchoServer {
    type Error = russh::Error;
    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<russh::server::Auth, Self::Error> {
        assert_eq!(user, "synthetic-user");
        assert_eq!(password, "synthetic-never-in-argv");
        Ok(russh::server::Auth::Accept)
    }
    async fn channel_open_session(
        &mut self,
        _: russh::Channel<russh::server::Msg>,
        _: &mut russh::server::Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
    async fn pty_request(
        &mut self,
        channel: russh::ChannelId,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }
    async fn shell_request(
        &mut self,
        channel: russh::ChannelId,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }
    async fn data(
        &mut self,
        channel: russh::ChannelId,
        data: &[u8],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        session.data(channel, data.to_vec())
    }
}

#[test]
#[ignore = "requires ADIT_PROXY_FIXTURE"]
fn real_ssh_handshake_and_shell_use_stdio_and_keep_shared_claim_alive() {
    runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let port = listener.local_addr().unwrap().port();
            // A deterministic synthetic host key, not a user's identity file.
            let key = russh::keys::PrivateKey::from(
                russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[42; 32]),
            );
            let config = Arc::new(russh::server::Config {
                keys: vec![key],
                auth_rejection_time: Duration::ZERO,
                ..Default::default()
            });
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                russh::server::run_stream(config, socket, EchoServer)
                    .await
                    .unwrap()
                    .await
            });
            let mut fixture = Fixture::new("bridge");
            fixture.request.port = port;
            fixture
                .request
                .proxy_command
                .args
                .extend(["%h".into(), "%p".into()]);
            fixture.request.auto_accept_host_keys = true;
            let (commands, rx) = tokio_mpsc::unbounded_channel();
            let (events, event_rx) = mpsc::channel();
            let task = tokio::spawn(run_live_password_shell(fixture.request.clone(), rx, events));
            let pid = fixture.pid("").await;
            let mut shared = None;
            let mut sent = false;
            let mut received = false;
            loop {
                if let Ok(event) = event_rx.try_recv() {
                    match event {
                        LiveShellEvent::SessionReady(session) => shared = Some(session),
                        LiveShellEvent::Status(status) if status == "connected" => {
                            commands
                                .send(LiveShellCommand::Input(b"proxy-stdio-echo".to_vec()))
                                .unwrap();
                            sent = true;
                        }
                        LiveShellEvent::Output(data) if data == b"proxy-stdio-echo" => {
                            received = true;
                            commands.send(LiveShellCommand::Disconnect).unwrap();
                        }
                        LiveShellEvent::Error(error) | LiveShellEvent::AuthRejected(error) => {
                            panic!("{error}")
                        }
                        LiveShellEvent::Closed => break,
                        _ => {}
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            assert!(sent && received);
            assert!(
                !task.is_finished(),
                "shared SFTP/tunnel claim must keep the proxy alive"
            );
            assert!(!shared.as_ref().unwrap().is_closed());
            drop(shared);
            task.await.unwrap().unwrap();
            assert_dead(pid);
            let _ = server.await.unwrap();
        })
        .await
        .expect("synthetic SSH over child stdio must finish");
    });
}
