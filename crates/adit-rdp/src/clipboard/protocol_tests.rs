//! Exercise the real IronRDP CLIPRDR processor with encoded synthetic PDUs.
use super::*;
use ironrdp_cliprdr::pdu::{
    Capabilities, ClipboardFormatName, ClipboardPdu, ClipboardProtocolVersion, FormatList,
    FormatListResponse, PackedFileList,
};
use ironrdp_core::decode;
use ironrdp_svc::{SvcMessage, SvcProcessor as _};

fn actions(state: &SharedClipboard) -> Vec<ClipboardAction> {
    state.lock().unwrap().take_actions()
}

fn process(clip: &mut CliprdrClient, pdu: &ClipboardPdu<'_>) -> Vec<SvcMessage> {
    clip.process(&ironrdp_core::encode_vec(pdu).unwrap())
        .unwrap()
}

fn channel_with_lock(state: SharedClipboard, locking: bool) -> CliprdrClient {
    let mut clip = CliprdrClient::new(Box::new(AditCliprdrBackend::new(Arc::clone(&state))));
    let mut flags = ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
        | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED;
    if locking {
        flags |= ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA;
    }
    process(
        &mut clip,
        &ClipboardPdu::Capabilities(Capabilities::new(ClipboardProtocolVersion::V2, flags)),
    );
    process(&mut clip, &ClipboardPdu::MonitorReady);
    let _ = actions(&state);
    let _: Vec<SvcMessage> = clip.initiate_copy(&[]).unwrap().into();
    process(
        &mut clip,
        &ClipboardPdu::FormatListResponse(FormatListResponse::Ok),
    );
    let _ = actions(&state);
    clip
}

fn channel() -> (CliprdrClient, SharedClipboard) {
    let state = new_shared();
    let clip = channel_with_lock(Arc::clone(&state), true);
    (clip, state)
}

fn format_list(clip: &mut CliprdrClient, formats: &[ClipboardFormat]) -> Option<u32> {
    let list = ClipboardPdu::FormatList(FormatList::new_unicode(formats, true).unwrap());
    process(clip, &list).iter().find_map(|message| {
        match decode::<ClipboardPdu<'_>>(&message.encode_unframed_pdu().unwrap()).unwrap() {
            ClipboardPdu::LockData(id) => Some(id.0),
            _ => None,
        }
    })
}

fn file_format() -> ClipboardFormat {
    ClipboardFormat::new(ClipboardFormatId(0xc001)).with_name(ClipboardFormatName::FILE_LIST)
}

fn text_format() -> ClipboardFormat {
    ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)
}

fn issue_taken(
    clip: &mut CliprdrClient,
    state: &SharedClipboard,
    queued: &[ClipboardAction],
) -> ClipboardFormatId {
    let (format, generation) = queued
        .iter()
        .find_map(|action| match action {
            ClipboardAction::RequestFormat { format, generation } => Some((*format, *generation)),
            _ => None,
        })
        .expect("queued format request");
    assert!(state
        .lock()
        .unwrap()
        .begin_remote_format(format, generation));
    let messages: Vec<SvcMessage> = clip.initiate_paste(format).unwrap().into();
    assert!(
        matches!(decode::<ClipboardPdu<'_>>(&messages[0].encode_unframed_pdu().unwrap()).unwrap(),
        ClipboardPdu::FormatDataRequest(request) if request.format == format)
    );
    format
}

fn issue(clip: &mut CliprdrClient, state: &SharedClipboard) -> ClipboardFormatId {
    issue_taken(clip, state, &actions(state))
}

fn file_response(name: &str) -> ClipboardPdu<'static> {
    let mut descriptor = FileDescriptor::new(name);
    descriptor.file_size = Some(8);
    ClipboardPdu::FormatDataResponse(
        OwnedFormatDataResponse::new_file_list(&PackedFileList {
            files: vec![descriptor],
        })
        .unwrap(),
    )
}

fn offered(state: &SharedClipboard, name: &str) -> u64 {
    actions(state)
        .into_iter()
        .find_map(|action| match action {
            ClipboardAction::InboundFiles { generation, files }
                if files.len() == 1 && files[0].name == name =>
            {
                Some(generation)
            }
            _ => None,
        })
        .expect("expected remote file offer")
}

fn read_request(state: &SharedClipboard, generation: u64, stream_id: u32) -> BoundRead {
    request_file_contents(state, generation, stream_id, 0, 0, 3);
    actions(state)
        .into_iter()
        .find_map(|action| match action {
            ClipboardAction::RequestFileContents(read) => Some(read),
            _ => None,
        })
        .expect("file read")
}

fn assert_no_request_or_offer(state: &SharedClipboard) {
    assert!(!state.lock().unwrap().remote_files.active());
    assert!(actions(state).iter().all(|action| match action {
        ClipboardAction::RequestFormat { .. } => false,
        ClipboardAction::InboundFiles { files, .. } => files.is_empty(),
        _ => true,
    }));
}

#[test]
fn first_remote_file_offer_uses_the_negotiated_lock() {
    let (mut clip, state) = channel();
    let lock = format_list(&mut clip, &[file_format()]).expect("file lock");
    assert_eq!(issue(&mut clip, &state), ClipboardFormatId(0xc001));
    process(&mut clip, &file_response("first.bin"));
    let generation = offered(&state, "first.bin");
    let read = read_request(&state, generation, 42);
    let request = prepare_remote_read(&state, &read).unwrap();
    assert_eq!(request.data_id, Some(lock));
    let messages: Vec<SvcMessage> = clip.request_file_contents(request).unwrap().into();
    assert!(
        matches!(decode::<ClipboardPdu<'_>>(&messages[0].encode_unframed_pdu().unwrap()).unwrap(),
        ClipboardPdu::FileContentsRequest(request) if request.data_id == Some(lock))
    );
    process(
        &mut clip,
        &ClipboardPdu::FileContentsResponse(FileContentsResponse::new_data_response(
            read.wire_id,
            vec![1, 2, 3],
        )),
    );
    assert!(matches!(actions(&state).as_slice(),
        [ClipboardAction::InboundFileContents(Reply { generation: got, stream_id: 42, data: Some(data) })]
            if *got == generation && data == &[1, 2, 3]));
}

#[test]
fn overlapping_file_offers_drain_a_before_requesting_b() {
    let (mut clip, state) = channel();
    let a_lock = format_list(&mut clip, &[file_format()]).unwrap();
    issue(&mut clip, &state);
    let b_lock = format_list(&mut clip, &[file_format()]).unwrap();
    assert_ne!(a_lock, b_lock);
    assert_no_request_or_offer(&state);
    process(&mut clip, &file_response("A.bin"));
    assert!(!state.lock().unwrap().remote_files.active());
    assert_eq!(issue(&mut clip, &state), ClipboardFormatId(0xc001));
    process(&mut clip, &file_response("B.bin"));
    let generation = offered(&state, "B.bin");
    let read = read_request(&state, generation, 43);
    assert_eq!(
        prepare_remote_read(&state, &read).unwrap().data_id,
        Some(b_lock)
    );
}

#[test]
fn only_latest_deferred_offer_is_requested_after_old_error() {
    let (mut clip, state) = channel();
    format_list(&mut clip, &[file_format()]).unwrap();
    issue(&mut clip, &state);
    format_list(&mut clip, &[file_format()]).unwrap();
    let c_lock = format_list(&mut clip, &[file_format()]).unwrap();
    assert_no_request_or_offer(&state);
    process(
        &mut clip,
        &ClipboardPdu::FormatDataResponse(OwnedFormatDataResponse::new_error()),
    );
    issue(&mut clip, &state);
    process(&mut clip, &file_response("C.bin"));
    let generation = offered(&state, "C.bin");
    let read = read_request(&state, generation, 44);
    assert_eq!(
        prepare_remote_read(&state, &read).unwrap().data_id,
        Some(c_lock)
    );
}

#[test]
fn queued_request_replaced_before_send_does_not_poison_the_new_offer() {
    let (mut clip, state) = channel();
    format_list(&mut clip, &[file_format()]).unwrap();
    let old = actions(&state);
    let (old_format, old_generation) = match old.as_slice() {
        [ClipboardAction::RequestFormat { format, generation }] => (*format, *generation),
        _ => panic!("expected A request"),
    };
    let b_lock = format_list(&mut clip, &[file_format()]).unwrap();
    assert!(!state
        .lock()
        .unwrap()
        .begin_remote_format(old_format, old_generation));
    issue(&mut clip, &state);
    process(&mut clip, &file_response("B.bin"));
    let generation = offered(&state, "B.bin");
    let read = read_request(&state, generation, 45);
    assert_eq!(
        prepare_remote_read(&state, &read).unwrap().data_id,
        Some(b_lock)
    );
}

#[test]
fn unsolicited_response_does_not_consume_a_queued_request() {
    let (mut clip, state) = channel();
    let lock = format_list(&mut clip, &[file_format()]).unwrap();
    process(&mut clip, &file_response("unsolicited.bin"));
    assert!(!state.lock().unwrap().remote_files.active());
    issue(&mut clip, &state);
    process(&mut clip, &file_response("expected.bin"));
    let generation = offered(&state, "expected.bin");
    let read = read_request(&state, generation, 50);
    assert_eq!(
        prepare_remote_read(&state, &read).unwrap().data_id,
        Some(lock)
    );
}

#[test]
fn local_copy_invalidates_an_issued_file_request_without_breaking_later_offers() {
    let (mut clip, state) = channel();
    format_list(&mut clip, &[file_format()]).unwrap();
    issue(&mut clip, &state);
    offer_local_text(&state, "local text".into());
    let _ = actions(&state);
    // With no intervening remote FormatList, IronRDP dispatches the response
    // through on_remote_file_list even though local ownership has changed.
    process(&mut clip, &file_response("old.bin"));
    assert_no_request_or_offer(&state);
    process(
        &mut clip,
        &ClipboardPdu::FormatDataRequest(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        }),
    );
    assert!(matches!(actions(&state).as_slice(),
        [ClipboardAction::SubmitText { text: Some(text), .. }] if text == "local text"));
    let lock = format_list(&mut clip, &[file_format()]).unwrap();
    issue(&mut clip, &state);
    process(&mut clip, &file_response("new.bin"));
    let generation = offered(&state, "new.bin");
    let read = read_request(&state, generation, 51);
    assert_eq!(
        prepare_remote_read(&state, &read).unwrap().data_id,
        Some(lock)
    );
}

#[test]
fn old_file_response_is_drained_before_new_text() {
    let (mut clip, state) = channel();
    format_list(&mut clip, &[file_format()]).unwrap();
    issue(&mut clip, &state);
    format_list(&mut clip, &[text_format()]);
    assert_no_request_or_offer(&state);
    process(&mut clip, &file_response("old.bin"));
    assert_eq!(issue(&mut clip, &state), ClipboardFormatId::CF_UNICODETEXT);
    process(
        &mut clip,
        &ClipboardPdu::FormatDataResponse(OwnedFormatDataResponse::new_unicode_string("new text")),
    );
    assert_eq!(
        actions(&state),
        vec![ClipboardAction::Inbound("new text".into())]
    );
}

#[test]
fn old_text_response_is_drained_before_new_file() {
    let (mut clip, state) = channel();
    format_list(&mut clip, &[text_format()]);
    issue(&mut clip, &state);
    let lock = format_list(&mut clip, &[file_format()]).unwrap();
    assert_no_request_or_offer(&state);
    process(
        &mut clip,
        &ClipboardPdu::FormatDataResponse(OwnedFormatDataResponse::new_unicode_string("old text")),
    );
    issue(&mut clip, &state);
    process(&mut clip, &file_response("new.bin"));
    let generation = offered(&state, "new.bin");
    let read = read_request(&state, generation, 46);
    assert_eq!(
        prepare_remote_read(&state, &read).unwrap().data_id,
        Some(lock)
    );
}

#[test]
fn unlocked_file_offer_is_refused() {
    let state = new_shared();
    let mut clip = channel_with_lock(Arc::clone(&state), false);
    assert_eq!(format_list(&mut clip, &[file_format()]), None);
    issue(&mut clip, &state);
    process(&mut clip, &file_response("unlocked.bin"));
    assert!(!state.lock().unwrap().remote_files.active());
    assert!(actions(&state).iter().any(|action| matches!(action,
        ClipboardAction::InboundFiles { files, .. } if files.is_empty())));
}

#[test]
fn missing_old_response_waits_for_channel_reset_without_disconnecting() {
    let (mut clip, state) = channel();
    format_list(&mut clip, &[file_format()]).unwrap();
    issue(&mut clip, &state);
    format_list(&mut clip, &[file_format()]).unwrap();
    assert_no_request_or_offer(&state);
    // The channel still processes requests from the server while waiting.
    offer_local_text(&state, "local text".into());
    let _ = actions(&state);
    process(
        &mut clip,
        &ClipboardPdu::FormatDataRequest(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        }),
    );
    assert!(matches!(actions(&state).as_slice(),
        [ClipboardAction::SubmitText { text: Some(text), .. }] if text == "local text"));
    drop(clip);
    let mut fresh = channel_with_lock(Arc::clone(&state), true);
    let lock = format_list(&mut fresh, &[file_format()]).unwrap();
    issue(&mut fresh, &state);
    process(&mut fresh, &file_response("fresh.bin"));
    let generation = offered(&state, "fresh.bin");
    let read = read_request(&state, generation, 47);
    assert_eq!(
        prepare_remote_read(&state, &read).unwrap().data_id,
        Some(lock)
    );
}

#[test]
fn local_file_contents_remain_refused_while_text_works() {
    let (mut clip, state) = channel();
    offer_local_text(&state, "local text".into());
    let _ = actions(&state);
    process(
        &mut clip,
        &ClipboardPdu::FormatDataRequest(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        }),
    );
    assert!(matches!(actions(&state).as_slice(),
        [ClipboardAction::SubmitText { text: Some(text), .. }] if text == "local text"));
    for (stream_id, flags, requested_size) in [
        (10, FileContentsFlags::SIZE, 8),
        (11, FileContentsFlags::RANGE, 1),
    ] {
        let messages = process(
            &mut clip,
            &ClipboardPdu::FileContentsRequest(FileContentsRequest {
                stream_id,
                index: 0,
                flags,
                position: 0,
                requested_size,
                data_id: None,
            }),
        );
        assert!(
            matches!(decode::<ClipboardPdu<'_>>(&messages[0].encode_unframed_pdu().unwrap()).unwrap(),
            ClipboardPdu::FileContentsResponse(response) if response.stream_id() == stream_id && response.is_error())
        );
    }
    format_list(&mut clip, &[text_format()]);
    assert_eq!(issue(&mut clip, &state), ClipboardFormatId::CF_UNICODETEXT);
    process(
        &mut clip,
        &ClipboardPdu::FormatDataResponse(OwnedFormatDataResponse::new_unicode_string(
            "remote text",
        )),
    );
    assert_eq!(
        actions(&state),
        vec![ClipboardAction::Inbound("remote text".into())]
    );
}

#[test]
fn same_wire_descriptors_do_not_prove_which_lock_generated_them() {
    let response = file_response("same.bin");
    let wire = ironrdp_core::encode_vec(&response).unwrap();
    let (mut first, state_a) = channel();
    let lock_a = format_list(&mut first, &[file_format()]).unwrap();
    issue(&mut first, &state_a);
    first.process(&wire).unwrap();
    let generation = offered(&state_a, "same.bin");
    let read = read_request(&state_a, generation, 48);
    assert_eq!(
        prepare_remote_read(&state_a, &read).unwrap().data_id,
        Some(lock_a)
    );

    let (mut second, state_b) = channel();
    format_list(&mut second, &[file_format()]).unwrap();
    let lock_b = format_list(&mut second, &[file_format()]).unwrap();
    assert_ne!(lock_a, lock_b);
    issue(&mut second, &state_b);
    // The identical PDU is accepted with another lock: no wire field proves
    // whether the remote rendered it from either locked snapshot.
    second.process(&wire).unwrap();
    let generation = offered(&state_b, "same.bin");
    let read = read_request(&state_b, generation, 49);
    assert_eq!(
        prepare_remote_read(&state_b, &read).unwrap().data_id,
        Some(lock_b)
    );
}
