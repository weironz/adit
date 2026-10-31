//! Framed app IPC through the helper and a synthetic CLIPRDR peer.
use super::*;
use adit_rdp_proto::{ClipFile, InputEvent};
use ironrdp_cliprdr::pdu::{
    Capabilities, ClipboardFormat, ClipboardFormatId, ClipboardFormatName,
    ClipboardGeneralCapabilityFlags, ClipboardPdu, ClipboardProtocolVersion, FileContentsFlags,
    FileContentsRequest, FileContentsResponse, FileDescriptor, FormatDataRequest, FormatList,
    FormatListResponse, OwnedFormatDataResponse, PackedFileList,
};
use ironrdp_cliprdr::CliprdrClient;
use ironrdp_svc::{SvcMessage, SvcProcessor as _};

fn framed_messages(
    state: &SharedClipboard,
    routing: &Arc<InputRouting>,
    messages: impl IntoIterator<Item = ClientMsg>,
) -> Vec<SessionCmd> {
    let (reader, mut writer) = io::pipe().unwrap();
    for message in messages {
        write_msg(&mut writer, &message).unwrap();
    }
    write_msg(&mut writer, &ClientMsg::Close).unwrap();
    drop(writer);
    let (request_tx, _request_rx) = std_mpsc::channel();
    let (input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();
    let (display_tx, _display_rx) = tokio_mpsc::unbounded_channel();
    read_app_messages(
        reader,
        request_tx,
        input_tx,
        display_tx,
        Arc::clone(routing),
        Arc::clone(state),
    );
    let mut commands = Vec::new();
    while let Ok(command) = input_rx.try_recv() {
        commands.push(command);
    }
    commands
}

fn synthetic_cliprdr(state: &SharedClipboard) -> CliprdrClient {
    let backend = clipboard::AditCliprdrBackend::new(Arc::clone(state));
    let mut clip = CliprdrClient::new(Box::new(backend));
    let flags = ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
        | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
        | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA;
    for pdu in [
        ClipboardPdu::Capabilities(Capabilities::new(ClipboardProtocolVersion::V2, flags)),
        ClipboardPdu::MonitorReady,
    ] {
        clip.process(&ironrdp_core::encode_vec(&pdu).unwrap())
            .unwrap();
    }
    state.lock().unwrap().take_actions();
    let _: Vec<SvcMessage> = clip.initiate_copy(&[]).unwrap().into();
    clip.process(
        &ironrdp_core::encode_vec(&ClipboardPdu::FormatListResponse(FormatListResponse::Ok))
            .unwrap(),
    )
    .unwrap();
    state.lock().unwrap().take_actions();
    clip
}

#[test]
fn framed_empty_file_offer_revokes_text_before_a_cliprdr_paste() {
    use crate::clipboard::ClipboardAction;

    let state = clipboard::new_shared();
    let mut clip = synthetic_cliprdr(&state);
    let routing = Arc::new(InputRouting::default());
    framed_messages(
        &state,
        &routing,
        [ClientMsg::EpochInput {
            epoch: 0,
            event: InputEvent::ClipboardText("synthetic-private-text".into()),
        }],
    );
    state.lock().unwrap().take_actions();
    let _: Vec<SvcMessage> = clip
        .initiate_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)])
        .unwrap()
        .into();

    framed_messages(
        &state,
        &routing,
        [ClientMsg::EpochClipboardFiles {
            epoch: 0,
            files: Vec::new(),
        }],
    );

    clip.process(
        &ironrdp_core::encode_vec(&ClipboardPdu::FormatDataRequest(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        }))
        .unwrap(),
    )
    .unwrap();
    let text = state
        .lock()
        .unwrap()
        .take_actions()
        .into_iter()
        .find_map(|action| {
            if let ClipboardAction::SubmitText { text, .. } = action {
                Some(text)
            } else {
                None
            }
        })
        .expect("the remote paste must get a response");
    let response = match text {
        Some(text) => OwnedFormatDataResponse::new_unicode_string(&text),
        None => OwnedFormatDataResponse::new_error(),
    };
    let messages: Vec<SvcMessage> = clip.submit_format_data(response).unwrap().into();
    let wire = messages[0].encode_unframed_pdu().unwrap();
    assert!(
        matches!(ironrdp_core::decode::<ClipboardPdu<'_>>(&wire).unwrap(),
        ClipboardPdu::FormatDataResponse(response) if response.is_error()),
        "an empty framed offer must revoke the previously offered text"
    );
}

#[test]
fn framed_barrier_clears_queued_text_and_image_and_allows_a_fresh_offer() {
    use crate::clipboard::ClipboardAction;
    for image in [false, true] {
        let state = clipboard::new_shared();
        let mut clip = synthetic_cliprdr(&state);
        let routing = Arc::new(InputRouting::default());
        let message = if image {
            ClientMsg::EpochClipboardImage {
                epoch: 0,
                image: vec![11, 22, 33],
            }
        } else {
            ClientMsg::EpochInput {
                epoch: 0,
                event: InputEvent::ClipboardText("old".into()),
            }
        };
        let commands = framed_messages(&state, &routing, [message]);
        assert!(commands
            .iter()
            .all(|c| matches!(c, SessionCmd::ClipboardWake)));
        let format = if image {
            ClipboardFormatId::CF_DIB
        } else {
            ClipboardFormatId::CF_UNICODETEXT
        };
        clip.process(
            &ironrdp_core::encode_vec(&ClipboardPdu::FormatDataRequest(FormatDataRequest {
                format,
            }))
            .unwrap(),
        )
        .unwrap();
        // The clipboard response is queued but the session has not sent it.
        framed_messages(
            &state,
            &routing,
            [ClientMsg::EpochClipboardFiles {
                epoch: 0,
                files: Vec::new(),
            }],
        );
        let queued = state.lock().unwrap().take_actions();
        assert!(queued.iter().any(|action| match action {
            ClipboardAction::SubmitText { text: None, .. } if !image => true,
            ClipboardAction::SubmitImage { image: None, .. } if image => true,
            _ => false,
        }));
        assert!(queued.iter().all(|action| !matches!(
            action,
            ClipboardAction::SubmitText { text: Some(_), .. }
                | ClipboardAction::SubmitImage { image: Some(_), .. }
        )));
        framed_messages(
            &state,
            &routing,
            [ClientMsg::EpochInput {
                epoch: 0,
                event: InputEvent::ClipboardText("fresh".into()),
            }],
        );
        clip.process(
            &ironrdp_core::encode_vec(&ClipboardPdu::FormatDataRequest(FormatDataRequest {
                format: ClipboardFormatId::CF_UNICODETEXT,
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(state
            .lock()
            .unwrap()
            .take_actions()
            .iter()
            .any(|action| matches!(action,
                ClipboardAction::SubmitText { text: Some(text), .. } if text == "fresh"
            )));
    }
}

#[test]
fn framed_legacy_and_nonempty_file_offers_only_revoke_local_data() {
    use crate::clipboard::ClipboardAction;
    for legacy in [false, true] {
        let state = clipboard::new_shared();
        let mut clip = synthetic_cliprdr(&state);
        let routing = Arc::new(InputRouting::default());
        framed_messages(
            &state,
            &routing,
            [ClientMsg::EpochInput {
                epoch: 0,
                event: InputEvent::ClipboardText("old".into()),
            }],
        );
        state.lock().unwrap().take_actions();
        let files = vec![ClipFile {
            name: "local-secret.txt".into(),
            size: 3,
            is_dir: false,
        }];
        let message = if legacy {
            ClientMsg::ClipboardFiles(files)
        } else {
            ClientMsg::EpochClipboardFiles { epoch: 0, files }
        };
        framed_messages(&state, &routing, [message]);
        clip.process(
            &ironrdp_core::encode_vec(&ClipboardPdu::FormatDataRequest(FormatDataRequest {
                format: ClipboardFormatId::CF_UNICODETEXT,
            }))
            .unwrap(),
        )
        .unwrap();
        let actions = state.lock().unwrap().take_actions();
        assert!(actions
            .iter()
            .any(|a| matches!(a, ClipboardAction::SubmitText { text: None, .. })));
        assert!(!actions
            .iter()
            .any(|a| matches!(a, ClipboardAction::Advertise(ids)
            if ids.iter().any(|id| id.value() == 0xc001))));
    }
}

#[test]
fn framed_image_arriving_during_handshake_is_advertised_after_ready() {
    use crate::clipboard::ClipboardAction;
    let state = clipboard::new_shared();
    let mut clip = CliprdrClient::new(Box::new(clipboard::AditCliprdrBackend::new(Arc::clone(
        &state,
    ))));
    clip.process(&ironrdp_core::encode_vec(&ClipboardPdu::MonitorReady).unwrap())
        .unwrap();
    state.lock().unwrap().take_actions();
    let _: Vec<SvcMessage> = clip.initiate_copy(&[]).unwrap().into();
    framed_messages(
        &state,
        &Arc::new(InputRouting::default()),
        [ClientMsg::EpochClipboardImage {
            epoch: 0,
            image: vec![0x44, 0x55, 0x66],
        }],
    );
    clip.process(
        &ironrdp_core::encode_vec(&ClipboardPdu::FormatListResponse(FormatListResponse::Ok))
            .unwrap(),
    )
    .unwrap();
    assert!(
        state
            .lock()
            .unwrap()
            .take_actions()
            .iter()
            .any(|action| matches!(action,
                ClipboardAction::Advertise(ids) if ids.contains(&ClipboardFormatId::CF_DIB)
            )),
        "an image copied while waiting for the initial acknowledgement must be advertised"
    );
}

#[test]
fn framed_queued_offers_and_old_epoch_barriers_never_restore_stale_clipboard() {
    use crate::clipboard::ClipboardAction;
    let state = clipboard::new_shared();
    let clip = synthetic_cliprdr(&state);
    let routing = Arc::new(InputRouting::default());
    let commands = framed_messages(
        &state,
        &routing,
        [
            ClientMsg::EpochInput {
                epoch: 0,
                event: InputEvent::ClipboardText("old".into()),
            },
            ClientMsg::EpochClipboardImage {
                epoch: 0,
                image: vec![1, 2, 3],
            },
            ClientMsg::EpochClipboardFiles {
                epoch: 0,
                files: Vec::new(),
            },
        ],
    );
    assert_eq!(commands.len(), 3);
    assert!(commands
        .iter()
        .all(|c| matches!(c, SessionCmd::ClipboardWake)));
    framed_messages(
        &state,
        &routing,
        [ClientMsg::EpochClipboardImage {
            epoch: 0,
            image: vec![4, 5, 6],
        }],
    );
    assert_eq!(routing.advance(), Some(1));
    // Rebuilding the CLIPRDR channel intentionally preserves the local
    // clipboard; a late global barrier must still revoke that preserved data.
    drop(clip);
    let mut clip = synthetic_cliprdr(&state);
    // A real clipboard change can be tagged with the old epoch while the
    // app has not yet received the redirect notification.
    framed_messages(
        &state,
        &routing,
        [ClientMsg::EpochClipboardFiles {
            epoch: 0,
            files: Vec::new(),
        }],
    );
    framed_messages(
        &state,
        &routing,
        [
            ClientMsg::EpochInput {
                epoch: 0,
                event: InputEvent::ClipboardText("stale".into()),
            },
            ClientMsg::EpochClipboardImage {
                epoch: 0,
                image: vec![7, 8],
            },
        ],
    );
    for format in [ClipboardFormatId::CF_UNICODETEXT, ClipboardFormatId::CF_DIB] {
        clip.process(
            &ironrdp_core::encode_vec(&ClipboardPdu::FormatDataRequest(FormatDataRequest {
                format,
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let queued = state.lock().unwrap().take_actions();
    assert!(queued
        .iter()
        .any(|a| matches!(a, ClipboardAction::SubmitText { text: None, .. })));
    assert!(queued
        .iter()
        .any(|a| matches!(a, ClipboardAction::SubmitImage { image: None, .. })));
    framed_messages(
        &state,
        &routing,
        [ClientMsg::EpochInput {
            epoch: 1,
            event: InputEvent::ClipboardText("current".into()),
        }],
    );
    clip.process(
        &ironrdp_core::encode_vec(&ClipboardPdu::FormatDataRequest(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(state
        .lock()
        .unwrap()
        .take_actions()
        .iter()
        .any(|a| matches!(a,
            ClipboardAction::SubmitText { text: Some(text), .. } if text == "current"
        )));
}

#[test]
fn framed_file_barrier_preserves_remote_file_snapshot_and_never_offers_local_files() {
    use crate::clipboard::ClipboardAction;
    let state = clipboard::new_shared();
    let mut clip = synthetic_cliprdr(&state);
    let routing = Arc::new(InputRouting::default());
    let format =
        ClipboardFormat::new(ClipboardFormatId(0xc001)).with_name(ClipboardFormatName::FILE_LIST);
    let list = ClipboardPdu::FormatList(FormatList::new_unicode(&[format], true).unwrap());
    let messages = clip
        .process(&ironrdp_core::encode_vec(&list).unwrap())
        .unwrap();
    let lock = messages
        .iter()
        .find_map(|message| {
            match ironrdp_core::decode::<ClipboardPdu<'_>>(&message.encode_unframed_pdu().unwrap())
                .unwrap()
            {
                ClipboardPdu::LockData(id) => Some(id.0),
                _ => None,
            }
        })
        .expect("remote file lock");
    let _: Vec<SvcMessage> = clip
        .initiate_paste(ClipboardFormatId(0xc001))
        .unwrap()
        .into();
    let mut descriptor = FileDescriptor::new("remote.txt");
    descriptor.file_size = Some(3);
    let response = OwnedFormatDataResponse::new_file_list(&PackedFileList {
        files: vec![descriptor],
    })
    .unwrap();
    clip.process(&ironrdp_core::encode_vec(&ClipboardPdu::FormatDataResponse(response)).unwrap())
        .unwrap();
    let generation = state
        .lock()
        .unwrap()
        .take_actions()
        .into_iter()
        .find_map(|action| match action {
            ClipboardAction::InboundFiles { generation, files } if files.len() == 1 => {
                Some(generation)
            }
            _ => None,
        })
        .expect("remote file offer");
    clipboard::request_file_contents(&state, generation, 41, 0, 0, 3);
    let in_flight = state
        .lock()
        .unwrap()
        .take_actions()
        .into_iter()
        .find_map(|action| match action {
            ClipboardAction::RequestFileContents(read) => Some(read),
            _ => None,
        })
        .unwrap();
    let _: Vec<SvcMessage> = clip
        .request_file_contents(FileContentsRequest {
            stream_id: in_flight.wire_id,
            index: i32::try_from(in_flight.index).unwrap(),
            flags: FileContentsFlags::RANGE,
            position: in_flight.offset,
            requested_size: in_flight.length,
            data_id: Some(in_flight.data_id),
        })
        .unwrap()
        .into();
    // Another read remains queued when the local-only barriers arrive.
    clipboard::request_file_contents(&state, generation, 42, 0, 0, 3);
    framed_messages(
        &state,
        &routing,
        [
            ClientMsg::EpochClipboardFiles {
                epoch: 0,
                files: Vec::new(),
            },
            ClientMsg::EpochClipboardFiles {
                epoch: 0,
                files: vec![ClipFile {
                    name: "local-secret.txt".into(),
                    size: 3,
                    is_dir: false,
                }],
            },
        ],
    );
    let queued = state.lock().unwrap().take_actions();
    assert!(!queued
        .iter()
        .any(|a| matches!(a, ClipboardAction::InboundFiles { files, .. } if files.is_empty())));
    assert!(!queued.iter().any(|a| matches!(a, ClipboardAction::Advertise(ids) if ids.iter().any(|id| id.value() == 0xc001))));
    let read = queued
        .into_iter()
        .find_map(|action| match action {
            ClipboardAction::RequestFileContents(read) => Some(read),
            _ => None,
        })
        .expect("queued remote read survives the barrier");
    assert_eq!(read.data_id, lock);
    clip.process(
        &ironrdp_core::encode_vec(&ClipboardPdu::FileContentsResponse(
            FileContentsResponse::new_data_response(in_flight.wire_id, vec![1, 2, 3]),
        ))
        .unwrap(),
    )
    .unwrap();
    assert!(state.lock().unwrap().take_actions().iter().any(|action| matches!(action,
        ClipboardAction::InboundFileContents(reply) if reply.generation == generation && reply.stream_id == 41 && reply.data.as_deref() == Some(&[1, 2, 3])
    )));
    let commands = framed_messages(
        &state,
        &routing,
        [ClientMsg::FileContents {
            epoch: 0,
            stream_id: 97,
            data: Some(vec![0xaa, 0xbb]),
        }],
    );
    for command in commands {
        if let SessionCmd::FileContents {
            stream_id, data, ..
        } = command
        {
            clipboard::submit_file_contents(&state, stream_id, data);
        }
    }
    assert!(state
        .lock()
        .unwrap()
        .take_actions()
        .iter()
        .any(|a| matches!(a, ClipboardAction::RefuseFileContents { stream_id: 97 })));
    clip.process(
        &ironrdp_core::encode_vec(&ClipboardPdu::FormatDataRequest(FormatDataRequest {
            format: ClipboardFormatId(0xc001),
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(state
        .lock()
        .unwrap()
        .take_actions()
        .iter()
        .any(|a| matches!(a, ClipboardAction::SubmitText { text: None, .. })));
    let request = FileContentsRequest {
        stream_id: read.wire_id,
        index: i32::try_from(read.index).unwrap(),
        flags: FileContentsFlags::RANGE,
        position: read.offset,
        requested_size: read.length,
        data_id: Some(read.data_id),
    };
    let _: Vec<SvcMessage> = clip.request_file_contents(request).unwrap().into();
    clip.process(
        &ironrdp_core::encode_vec(&ClipboardPdu::FileContentsResponse(
            FileContentsResponse::new_data_response(read.wire_id, vec![4, 5, 6]),
        ))
        .unwrap(),
    )
    .unwrap();
    assert!(state.lock().unwrap().take_actions().iter().any(|a| matches!(a,
        ClipboardAction::InboundFileContents(reply) if reply.generation == generation && reply.stream_id == 42 && reply.data.as_deref() == Some(&[4, 5, 6])
    )));
    let local_request = ClipboardPdu::FileContentsRequest(FileContentsRequest {
        stream_id: 99,
        index: 0,
        flags: FileContentsFlags::RANGE,
        position: 0,
        requested_size: 3,
        data_id: None,
    });
    let response = clip
        .process(&ironrdp_core::encode_vec(&local_request).unwrap())
        .unwrap();
    assert!(
        matches!(ironrdp_core::decode::<ClipboardPdu<'_>>(&response[0].encode_unframed_pdu().unwrap()).unwrap(),
        ClipboardPdu::FileContentsResponse(response) if response.is_error())
    );
}
