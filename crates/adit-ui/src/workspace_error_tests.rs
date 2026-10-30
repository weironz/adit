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

fn dispatch_keyboard(app: &mut AditApp) {
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
        let _ = update(app, Message::KeyboardInput(event));
    }
}

fn dispatch_inputs(app: &mut AditApp) {
    // The same focused, unblocked dispatch must work before Error and be
    // rejected afterwards, including queued messages from the old surface.
    app.terminal_focused = true;
    dispatch_keyboard(app);
    let _ = update(app, Message::RdpPointerMoved(Point::new(1.0, 1.0)));
    let _ = update(app, Message::RdpPressed(mouse::Button::Left));
    let _ = update(app, Message::RdpReleased(mouse::Button::Left));
    let _ = update(
        app,
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
}

fn wait_for_file(path: &std::path::Path, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !path.is_file() {
        assert!(Instant::now() < deadline, "{description}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn run_worker() {
    let mode = std::env::var(MODE_ENV).unwrap();
    let live = mode.starts_with("live_");
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
    loop {
        app.manager.poll_events();
        let _ = update(&mut app, Message::RdpTick);
        let connected = app
            .manager
            .active_session_summary()
            .is_some_and(|summary| summary.status == SessionStatus::Connected);
        let tile_cached = app
            .manager
            .active_rdp_frame_if_newer(0)
            .is_some_and(|frame| {
                frame.width == 2
                    && frame.height == 2
                    && frame.rgba == [10, 20, 30, 255].repeat(4)
                    && frame.generation == app.rdp_frame_generation
                    && !app.rdp_tiles.is_empty()
            });
        if connected && tile_cached {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Connected and the synthetic IPC tile must reach the UI cache"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        app.manager.active_session_summary().unwrap().status,
        SessionStatus::Connected
    );
    assert_eq!(app.rdp_surface_size, Some((2, 2)));
    assert!(app.manager.active_desktop_error().is_none());
    let deadline = Instant::now() + Duration::from_secs(8);
    while !app.manager.test_rdp_helper_attached(session) {
        assert!(
            Instant::now() < deadline,
            "synthetic helper must be attached"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let healthy_view = view_signature(&app);
    let stale_count = app.rdp_tiles.len();
    let _release = ReleaseHold(report.with_extension("release"));
    app.manager.send_rdp_input_to_active(RdpInput::Key {
        scancode: 0x7f,
        extended: false,
        pressed: true,
    });
    if live {
        dispatch_inputs(&mut app);
        app.manager.test_rdp_fifo_barrier(session);
        let preflight = report.with_extension("preflight");
        wait_for_file(&preflight, "connected input FIFO barrier");
        assert_eq!(
            std::fs::read_to_string(preflight).unwrap(),
            "key_down,key_up,move,button_down,button_up,wheel,unicode_down,unicode_up,resize",
            "the tested input paths must actually reach the live helper before Error"
        );
        // A real HostMsg::Error now terminates the writer. Inject only its
        // session event here so the original live-writer gate remains testable;
        // the fatal modes below exercise the real reader and its Close.
        app.manager
            .test_inject_rdp_error_with_live_writer(session, ERROR);
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut handled = 0;
    while app.manager.active_desktop_error().is_none() {
        handled += app.manager.poll_events();
        let _ = update(&mut app, Message::RdpTick);
        assert!(
            Instant::now() < deadline,
            "specific Error must reach the session"
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
    dispatch_inputs(&mut app);
    assert!(
        crate::input::remote_workspace_accepts_input(&app),
        "a focus or modal gate must not mask the session input gate"
    );
    // Preserve the original global-key case as well: these events can already
    // be queued when the desktop loses focus after the error.
    app.terminal_focused = false;
    dispatch_keyboard(&mut app);
    assert!(!crate::input::remote_workspace_accepts_input(&app));
    app.terminal_focused = true;
    if live {
        // This synthetic marker shares the real writer FIFO with inputs.
        // Reaching it proves the zero count is not a closed-writer artifact.
        app.manager.test_rdp_fifo_barrier(session);
    }
    wait_for_file(&report, "post-error peer evidence");
    let evidence = if live {
        format!("mode={mode}\ninputs_after_error=0\nbarrier=true\npipe_held_open=true\n")
    } else {
        format!("mode={mode}\ninputs_after_error=0\nclose_received=true\nstdin_eof=true\nstdout_held_open=true\n")
    };
    assert_eq!(
        std::fs::read_to_string(&report).unwrap(),
        evidence,
        "the peer must report the expected live-writer or fatal-close evidence"
    );
    assert_eq!(app.manager.active_desktop_error(), Some(ERROR));
    assert_eq!(
        handled, 1,
        "the peer must still hold stdout open after the error"
    );
    assert_eq!(
        app.manager.poll_events(),
        0,
        "no Closed/EOF may arrive before the peer is released"
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
        format!(
            "mode={mode}\nclosed_message_sent={}\n",
            mode.ends_with("_closed")
        )
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
    let error_source = if live { "injected_event" } else { "helper_ipc" };
    println!(
        "synthetic_ipc mode={mode} error_source={error_source} had_frame=true held_after_error=true input_count=0 specific_error_preserved=true shutdown_event_consumed=true"
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
    for mode in ["live_closed", "live_eof", "fatal_closed", "fatal_eof"] {
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
            .env("ADIT_CONFIG_DIR", scratch.join("config"))
            .env_remove("ADIT_RDP_FRAMES")
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
