use super::*;

#[test]
fn clipboard_stream_ids_and_payloads_cannot_cross_native_window_sessions() {
    let a = ConnectionProfile::new("a", "a.invalid", 22, "fixture");
    let b = ConnectionProfile::new("b", "b.invalid", 22, "fixture");
    let (a_id, b_id) = (a.id, b.id);
    let mut manager = SessionManager::with_profiles(vec![a, b]);
    let a = manager.open_mock_session(a_id).unwrap();
    let b = manager.open_mock_session(b_id).unwrap();
    // Different servers routinely reuse the same stream IDs.
    manager.pending_rdp_file_chunks = vec![(a, 1, Some(vec![10])), (b, 1, Some(vec![20]))];
    let read = RdpFileRead {
        stream_id: 1,
        index: 0,
        offset: 0,
        length: 1,
    };
    manager.pending_rdp_file_reads = vec![(a, read), (b, read)];
    manager.pending_rdp_clipboard_image = Some((a, vec![30]));
    manager.pending_rdp_clipboard_files = Some((a, Vec::new()));
    manager.activate(b).unwrap();
    assert_eq!(manager.take_rdp_file_chunks(), vec![(1, Some(vec![20]))]);
    assert_eq!(manager.take_rdp_file_reads(), vec![read]);
    assert!(manager.take_rdp_clipboard_image().is_none());
    assert!(manager.take_rdp_clipboard_files().is_none());
    assert!(manager.take_rdp_file_reads().is_empty());
    manager.activate(a).unwrap();
    assert_eq!(manager.take_rdp_file_chunks(), vec![(1, Some(vec![10]))]);
    assert_eq!(manager.take_rdp_file_reads(), vec![read]);
    assert_eq!(manager.take_rdp_clipboard_image(), Some(vec![30]));
    assert!(manager.take_rdp_clipboard_files().is_some());
    assert!(manager.pending_rdp_file_chunks.is_empty());
    assert!(manager.pending_rdp_file_reads.is_empty());
}

#[test]
fn temporary_window_projection_preserves_pending_offers_and_focus_retirement_keeps_file_streams() {
    let a = ConnectionProfile::new("a", "a.invalid", 22, "fixture");
    let b = ConnectionProfile::new("b", "b.invalid", 22, "fixture");
    let (a_id, b_id) = (a.id, b.id);
    let mut manager = SessionManager::with_profiles(vec![a, b]);
    let a = manager.open_mock_session(a_id).unwrap();
    let b = manager.open_mock_session(b_id).unwrap();
    manager.pending_rdp_clipboard = Some((a, PendingDesktopClipboard::Rdp("fixture-a".into())));
    manager.select_session_context(b).unwrap();
    assert!(manager.take_rdp_clipboard().is_none());
    manager.select_session_context(a).unwrap();
    assert_eq!(manager.take_rdp_clipboard().as_deref(), Some("fixture-a"));
    manager.pending_rdp_clipboard = Some((a, PendingDesktopClipboard::Rdp("retired".into())));
    manager.pending_rdp_file_chunks.push((a, 1, Some(vec![7])));
    manager.discard_clipboard_offers(a);
    assert!(manager.take_rdp_clipboard().is_none());
    assert_eq!(manager.take_rdp_file_chunks(), vec![(1, Some(vec![7]))]);
}
