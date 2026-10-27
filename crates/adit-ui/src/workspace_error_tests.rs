//! Actual helper IPC -> SessionManager -> production UI update/view regression.
//! No native Windows server or codec failure is claimed by this synthetic peer.
use super::*;
use iced::advanced::widget::{tree::Tag, Tree};
use std::process::{Command, Stdio};
use std::time::Duration;

const ERROR: &str =
    "ClearCodec decode failed: synthetic IPC acceptance fault (no native RDP decode)";
const WORKER_ENV: &str = "ADIT_RDP_GUI_ERROR_WORKER";
const MODE_ENV: &str = "ADIT_RDP_FAILURE_MODE";
const REPORT_ENV: &str = "ADIT_RDP_FAILURE_REPORT";

struct ReleaseHold(std::path::PathBuf);
impl Drop for ReleaseHold {
    fn drop(&mut self) {
        // A failing assertion must not leave the synthetic peer waiting.
        let _ = std::fs::write(&self.0, b"release");
    }
}

fn view_signature(app: &AditApp) -> Vec<(Tag, usize)> {
    fn visit(tree: &Tree, result: &mut Vec<(Tag, usize)>) {
        result.push((tree.tag, tree.children.len()));
        for child in &tree.children {
            visit(child, result);
        }
    }
    let view = rdp_surface_view(app);
    let tree = Tree::new(view.as_widget());
    let mut result = Vec::new();
    visit(&tree, &mut result);
    result
}

fn run_worker() {
    let mode = std::env::var(MODE_ENV).unwrap();
    let report = std::path::PathBuf::from(std::env::var_os(REPORT_ENV).unwrap());
    let scratch = report.parent().unwrap().to_owned();
    let mut profile = ConnectionProfile::new("synthetic RDP error", "adit.invalid", 3389, "");
    profile.protocol = Protocol::Rdp;
    let id = profile.id;
    let mut app = AditApp {
        manager: SessionManager::with_profiles(vec![profile]),
        profile_store: ProfileStore::new(scratch.join("profiles.json")),
        settings_store: SettingsStore::new(scratch.join("settings.json")),
        credential_store: CredentialStore::with_path(
            "rdp-error-ui",
            scratch.join("credentials.json"),
        ),
        ..AditApp::default()
    };
    let session = app
        .manager
        .open_live_rdp_session(id, String::new(), 2, 2)
        .unwrap();
    app.main_view = MainView::Terminal;
    let deadline = Instant::now() + Duration::from_secs(8);
    while app.rdp_tiles.is_empty() {
        app.manager.poll_events();
        let _ = update(&mut app, Message::RdpTick);
        assert!(
            Instant::now() < deadline,
            "public synthetic IPC frame must reach the UI"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        app.manager.active_session_summary().unwrap().status,
        SessionStatus::Connected
    );
    assert_eq!(app.rdp_surface_size, Some((2, 2)));
    assert!(app.manager.active_desktop_error().is_none());
    let healthy_view = view_signature(&app);
    let stale_count = app.rdp_tiles.len();
    let _release = ReleaseHold(report.with_extension("release"));
    app.manager.send_rdp_input_to_active(RdpInput::Key {
        scancode: 0x7f,
        extended: false,
        pressed: true,
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut handled = 0;
    while app.manager.active_desktop_error().is_none() {
        handled += app.manager.poll_events();
        let _ = update(&mut app, Message::RdpTick);
        assert!(
            Instant::now() < deadline,
            "specific Error must cross the real helper pipe"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        handled, 1,
        "peer must hold the pipe open after Error, before Closed/EOF"
    );
    assert_eq!(
        app.manager.active_session_summary().unwrap().status,
        SessionStatus::Error
    );
    assert_eq!(app.manager.active_desktop_error(), Some(ERROR));
    assert_eq!(
        app.rdp_tiles.len(),
        stale_count,
        "test must exercise retained post-frame pixels"
    );
    let failed_view = view_signature(&app);
    assert_ne!(
        healthy_view, failed_view,
        "the interactive image tree must leave the view on error"
    );
    // These are production dispatcher messages that could already have been
    // queued when the frame was visible. KeyboardInput is global (the RDP
    // branch intentionally precedes terminal_focused), not a widget click.
    app.terminal_focused = false;
    for pressed in [true, false] {
        let event = if pressed {
            keyboard::Event::KeyPressed {
                key: Key::Character("a".into()),
                modified_key: Key::Character("a".into()),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyA),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: Some("a".into()),
                repeat: false,
            }
        } else {
            keyboard::Event::KeyReleased {
                key: Key::Character("a".into()),
                modified_key: Key::Character("a".into()),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyA),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
            }
        };
        let _ = update(&mut app, Message::KeyboardInput(event));
    }
    let _ = update(&mut app, Message::RdpPointerMoved(Point::new(1.0, 1.0)));
    let _ = update(&mut app, Message::RdpPressed(mouse::Button::Left));
    let _ = update(&mut app, Message::RdpReleased(mouse::Button::Left));
    let _ = update(
        &mut app,
        Message::RdpScrolled(mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 }),
    );
    for pressed in [true, false] {
        app.manager
            .send_rdp_input_to_active(RdpInput::Unicode { ch: 'a', pressed });
    }
    app.manager.send_rdp_input_to_active(RdpInput::Resize {
        width: 4,
        height: 4,
    });
    // This empty metadata-only control message uses the same stdio writer
    // FIFO, so the peer has read every earlier Input when it reports counts.
    // No local clipboard or real file content is accessed.
    app.manager.offer_rdp_files_to_active(Vec::new());
    let deadline = Instant::now() + Duration::from_secs(8);
    while !report.is_file() {
        assert!(Instant::now() < deadline, "post-error peer FIFO barrier");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        std::fs::read_to_string(&report).unwrap(),
        format!("mode={mode}\ninputs_after_error=0\nbarrier=true\npipe_held_open=true\n"),
        "Error must block global keys, queued pointer input, Unicode and resize before the peer closes"
    );
    assert_eq!(app.manager.active_desktop_error(), Some(ERROR));
    assert_eq!(
        handled, 1,
        "input isolation was checked while the peer was still open"
    );
    std::fs::write(report.with_extension("release"), b"release").unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while handled < 2 || !report.with_extension("finished").is_file() {
        handled += app.manager.poll_events();
        let _ = update(&mut app, Message::RdpTick);
        assert!(
            Instant::now() < deadline,
            "Closed or actual EOF must finish the held peer"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        std::fs::read_to_string(report.with_extension("finished")).unwrap(),
        format!("mode={mode}\nclosed_message_sent={}\n", mode == "closed")
    );
    // Compare the actual production tree against its existing error-only
    // placeholder. This catches the old cached-image/mouse_area branch even
    // without a GPU or a platform window; status-only assertions missed it.
    app.rdp_tiles.clear();
    app.rdp_tiles_prev.clear();
    app.rdp_surface_size = None;
    app.rdp_surface_size_prev = None;
    assert_eq!(
        failed_view,
        view_signature(&app),
        "cached pixels must not mask the error-only view"
    );
    for _ in 0..3 {
        app.manager.poll_events();
        let _ = update(&mut app, Message::RdpTick);
    }
    assert_eq!(
        app.manager.active_desktop_error(),
        Some(ERROR),
        "Closed/EOF cannot replace the specific error"
    );
    assert_eq!(
        app.manager.active_session_summary().unwrap().status,
        SessionStatus::Error
    );
    app.manager.close(session);
    println!(
        "synthetic_ipc mode={mode} had_frame=true held_after_error=true input_count=0 specific_error_preserved=true shutdown_event_consumed=true"
    );
}

/// Build the public IPC peer, then set ADIT_RDP_FAILURE_HELPER to its absolute
/// executable path. The helper override exists only in this worker process;
/// parallel tests cannot accidentally inherit a synthetic RDP helper.
#[test]
#[ignore = "build -p adit-rdp-proto --example rdp_failure_ipc_peer and set ADIT_RDP_FAILURE_HELPER"]
fn connected_rdp_error_replaces_cached_interactive_surface() {
    if std::env::var_os(WORKER_ENV).is_some() {
        run_worker();
        return;
    }
    let helper = std::env::var_os("ADIT_RDP_FAILURE_HELPER").expect("set the public IPC peer path");
    assert!(
        std::path::Path::new(&helper).is_file(),
        "IPC peer must be built first"
    );
    for mode in ["closed", "eof"] {
        let scratch = std::env::temp_dir().join(format!("adit-rdp-error-ui-{}", SessionId::new()));
        std::fs::create_dir_all(&scratch).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace::error_tests::connected_rdp_error_replaces_cached_interactive_surface",
                "--ignored",
                "--nocapture",
            ])
            .env(WORKER_ENV, "1")
            .env("ADIT_RDP_HOST", &helper)
            .env(MODE_ENV, mode)
            .env(REPORT_ENV, scratch.join("peer.txt"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("isolated IPC/UI worker exceeded its deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "IPC/UI worker failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
}
