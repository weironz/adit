use super::*;

fn browser_fixture() -> (std::net::TcpListener, SessionManager) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let profile = ConnectionProfile::new(
        "native drop fixture",
        "127.0.0.1",
        listener.local_addr().unwrap().port(),
        "fixture",
    );
    let id = profile.id;
    let mut manager = SessionManager::with_profiles(vec![profile]);
    manager
        .open_live_ssh_session(id, String::new(), String::new())
        .unwrap();
    manager.open_sftp_for_active().unwrap();
    assert!(manager.sftp_browser().unwrap().drop_identity().is_none());
    manager.native_file_drop_fixture_ready();
    (listener, manager)
}

#[test]
fn native_drop_panel_waits_for_both_navigation_replies_in_either_order() {
    for reverse in [false, true] {
        let (_listener, mut manager) = browser_fixture();
        let browser = manager.sftp.as_mut().unwrap();
        let original = browser.drop_identity().unwrap().0;
        browser.begin_navigation("/a".into());
        browser.begin_navigation("/b".into());
        assert!(browser.drop_identity().is_none());
        let paths = if reverse { ["/b", "/a"] } else { ["/a", "/b"] };
        browser.finish_navigation(paths[0].into());
        assert!(
            browser.drop_identity().is_none(),
            "the first reply enabled an uncertain target"
        );
        browser.finish_navigation(paths[1].into());
        assert_eq!(browser.drop_identity().unwrap().1, paths[1]);
        assert_ne!(browser.drop_identity().unwrap().0, original);
    }
}

#[test]
fn native_drop_panel_failure_retires_only_its_own_request_and_recovers() {
    let (_listener, mut manager) = browser_fixture();
    let browser = manager.sftp.as_mut().unwrap();
    let original = browser.drop_identity().unwrap().0;
    browser.begin_navigation("/a".into());
    browser.begin_navigation("/b".into());
    browser.fail_navigation("mkdir /a: denied");
    assert_eq!(browser.pending_listings.len(), 2);
    browser.fail_navigation("list /a: denied");
    assert!(browser.drop_identity().is_none());
    browser.finish_navigation("/b".into());
    assert_eq!(browser.drop_identity().unwrap().1, "/b");
    assert_ne!(browser.drop_identity().unwrap().0, original);
    browser.begin_navigation("/a".into());
    browser.fail_navigation("list /a: denied");
    assert!(
        browser.drop_identity().is_some(),
        "failed navigation disabled drops permanently"
    );
}

#[test]
fn native_drop_panel_duplicate_paths_and_error_prefixes_do_not_clear_other_requests() {
    let (_listener, mut manager) = browser_fixture();
    let browser = manager.sftp.as_mut().unwrap();
    for path in ["/a", "/a", "/a: b"] {
        browser.begin_navigation(path.into());
    }
    browser.fail_navigation("list /a: b: denied");
    assert_eq!(
        browser.pending_listings.iter().collect::<Vec<_>>(),
        ["/a", "/a"]
    );
    browser.finish_navigation("/a".into());
    assert!(browser.drop_identity().is_none());
    browser.fail_navigation("list /a: denied");
    assert!(browser.drop_identity().is_some());
}

#[test]
fn native_drop_panel_directory_round_trip_never_reuses_a_revision() {
    let (_listener, mut manager) = browser_fixture();
    let browser = manager.sftp.as_mut().unwrap();
    let old = browser.drop_identity().unwrap().0;
    for path in ["/b", "/fixture"] {
        browser.begin_navigation(path.into());
        browser.finish_navigation(path.into());
    }
    assert_eq!(browser.drop_identity().unwrap().1, "/fixture");
    assert_ne!(browser.drop_identity().unwrap().0, old);
}

#[test]
fn native_drop_panel_refuses_before_ready_and_after_connection_closed() {
    let (_listener, mut manager) = browser_fixture();
    assert!(manager.sftp_browser().unwrap().drop_identity().is_some());
    let browser = manager.sftp.as_mut().unwrap();
    browser.connected = false;
    assert!(browser.drop_identity().is_none());
    manager.close_sftp();
    assert!(manager.sftp_browser().is_none());
}
