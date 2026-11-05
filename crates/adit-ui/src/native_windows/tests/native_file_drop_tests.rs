use super::*;
use native_file_drop::{test_support as native, Destination, Policy, Region};

fn ready_fixture(panel: bool) -> (DesktopApplication, SessionId) {
    let (mut desktop, shell) = sftp_drop_fixture(panel);
    desktop.app.manager.native_file_drop_fixture_ready();
    (desktop, shell)
}

fn publish_target(desktop: &DesktopApplication, window: window::Id) -> Destination {
    native::install(window);
    desktop.reconcile_native_drops();
    let policy = desktop.native_drop_policy(window);
    let destination = match &policy {
        Policy::Panel {
            instance,
            revision,
            cwd,
        } => Destination::Panel {
            instance: *instance,
            revision: *revision,
            cwd: cwd.clone(),
        },
        Policy::Shells(shells) => {
            let (session, (revision, cwd)) = shells.iter().next().unwrap();
            Destination::Shell {
                session: *session,
                revision: *revision,
                cwd: cwd.clone(),
            }
        }
        _ => panic!("missing legal native destination"),
    };
    native::publish(
        window,
        policy,
        vec![Region {
            destination: destination.clone(),
            bounds: iced::Rectangle::new(Point::new(300.0, 100.0), iced::Size::new(500.0, 400.0)),
        }],
    );
    destination
}

fn queue(window: window::Id, paths: Vec<std::path::PathBuf>) {
    assert!(native::queue(window, Point::new(400.0, 200.0), paths));
}

#[test]
fn native_queue_enters_the_real_update_loop_as_one_complete_panel_batch() {
    let _guard = native::SERIAL.lock().unwrap();
    let (mut desktop, _) = ready_fixture(true);
    let window = desktop.primary;
    publish_target(&desktop, window);
    let paths: Vec<std::path::PathBuf> =
        (0..150).map(|i| format!("native-{i}.txt").into()).collect();
    queue(window, paths.clone());
    let _ = desktop.update(NativeMessage::Blink);
    let browser = desktop.app.manager.sftp_browser().unwrap();
    assert_eq!(browser.transfers.len(), paths.len());
    for (transfer, path) in browser.transfers.iter().zip(paths) {
        assert!(transfer.dest.ends_with(path.to_str().unwrap()));
    }
    let _ = desktop.update(NativeMessage::Blink);
    assert_eq!(
        desktop.app.manager.sftp_browser().unwrap().transfers.len(),
        150
    );
    native_file_drop::forget(window);
}

#[test]
fn native_queue_routes_to_a_non_active_split_shell_and_restores_active() {
    let _guard = native::SERIAL.lock().unwrap();
    let (mut desktop, shell) = ready_fixture(false);
    let profile = ssh_profile(&desktop);
    let other = desktop.app.manager.open_mock_session(profile).unwrap();
    desktop.app.panes = vec![shell, other];
    desktop.app.manager.activate(other).unwrap();
    let window = desktop.primary;
    let before_other = desktop.app.manager.snapshot_for(other, Viewport::tail(100));
    assert!(
        matches!(publish_target(&desktop, window), Destination::Shell { session, .. } if session == shell)
    );
    queue(window, vec!["native-single.txt".into()]);
    let _ = desktop.update(NativeMessage::Blink);
    assert_eq!(desktop.app.manager.active_session(), Some(other));
    assert_eq!(
        desktop.app.manager.snapshot_for(other, Viewport::tail(100)),
        before_other
    );
    let text = terminal_text::snapshot_to_text(
        &desktop.app.manager.snapshot_for(shell, Viewport::tail(100)),
    );
    assert!(text.contains("native-single.txt"));
    native_file_drop::forget(window);
}

#[test]
fn native_queue_pins_detached_window_panel_and_directory() {
    let _guard = native::SERIAL.lock().unwrap();
    let (mut desktop, shell) = ready_fixture(true);
    let _ = desktop.detach(shell);
    let child = desktop.owner(shell);
    desktop.app.native_sftp_window = Some(child);
    assert!(matches!(
        desktop.native_drop_policy(desktop.primary),
        Policy::Disabled
    ));
    publish_target(&desktop, child);
    queue(child, vec!["native-detached.txt".into()]);
    let _ = desktop.update(NativeMessage::Blink);
    assert_eq!(
        desktop.app.manager.sftp_browser().unwrap().transfers.len(),
        1
    );
    native_file_drop::forget(child);
}

#[test]
fn native_queue_rejects_a_changed_directory_before_any_path_inspection() {
    let _guard = native::SERIAL.lock().unwrap();
    let (mut desktop, _) = ready_fixture(true);
    let window = desktop.primary;
    publish_target(&desktop, window);
    queue(window, vec!["never-stat-this-path.txt".into()]);
    let request = native_file_drop::drain().pop().unwrap();
    desktop.app.manager.sftp_goto("/another-directory");
    desktop.reconcile_native_drops();
    let _ = desktop.consume_native_drop(request, |_| panic!("stale directory inspected a path"));
    assert!(desktop
        .app
        .manager
        .sftp_browser()
        .unwrap()
        .transfers
        .is_empty());
    native_file_drop::forget(window);
}

#[test]
fn native_batch_preserves_overwrite_confirmation_and_never_starts_a_prefix() {
    let _guard = native::SERIAL.lock().unwrap();
    let (mut desktop, _) = ready_fixture(true);
    let window = desktop.primary;
    publish_target(&desktop, window);
    queue(window, vec!["native-queued.txt".into()]);
    desktop.app.sftp_overwrite = Some((Vec::new(), vec!["/existing.txt".into()]));
    let _ = desktop.update(NativeMessage::Blink);
    assert_eq!(
        desktop.app.sftp_overwrite.as_ref().unwrap().1,
        ["/existing.txt"]
    );
    assert!(desktop
        .app
        .manager
        .sftp_browser()
        .unwrap()
        .transfers
        .is_empty());
    desktop.app.sftp_overwrite = None;
    publish_target(&desktop, window);
    queue(window, vec!["valid.txt".into(), std::env::temp_dir()]);
    let _ = desktop.update(NativeMessage::Blink);
    assert!(desktop
        .app
        .manager
        .sftp_browser()
        .unwrap()
        .transfers
        .is_empty());
    assert!(desktop.app.last_error.is_some());
    native_file_drop::forget(window);
}

#[test]
fn native_shell_refuses_a_multiple_file_batch_before_inspecting_paths() {
    let _guard = native::SERIAL.lock().unwrap();
    let (mut desktop, _) = ready_fixture(false);
    let window = desktop.primary;
    let before = shell_text(&desktop);
    publish_target(&desktop, window);
    queue(window, vec!["a.txt".into(), "b.txt".into()]);
    let request = native_file_drop::drain().pop().unwrap();
    let _ = desktop.consume_native_drop(request, |_| panic!("multiple shell files inspected"));
    assert_eq!(shell_text(&desktop), before);
    assert!(desktop.app.last_error.as_deref().unwrap().contains("SFTP"));
    native_file_drop::forget(window);
}

#[test]
fn native_registration_failure_is_explicit_and_revokes_queued_requests() {
    let _guard = native::SERIAL.lock().unwrap();
    let (mut desktop, _) = ready_fixture(true);
    let window = desktop.primary;
    publish_target(&desktop, window);
    queue(window, vec!["must-not-upload.txt".into()]);
    let _ = desktop.update(NativeMessage::DropRegistered(window, false));
    assert_eq!(
        desktop.app.last_error.as_deref(),
        Some(t("原生文件拖放不可用，请使用 SFTP 面板选择文件"))
    );
    assert!(native_file_drop::drain().is_empty());
    assert!(desktop
        .app
        .manager
        .sftp_browser()
        .unwrap()
        .transfers
        .is_empty());
}
