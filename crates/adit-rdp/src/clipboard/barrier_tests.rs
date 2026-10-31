//! Revocation checks for responses detached by the CLIPRDR pump.
use super::*;
use ironrdp_core::decode;

#[test]
fn framed_barrier_sanitizes_text_and_image_already_taken_by_the_pump() {
    use crate::session::InputRouting;
    use adit_rdp_proto::{write_msg, ClientMsg, InputEvent};
    use ironrdp_cliprdr::pdu::{ClipboardPdu, FormatListResponse};
    use ironrdp_pdu::{mcs::SendDataRequest, x224::X224};
    use ironrdp_session::ActiveStageBuilder;
    use ironrdp_svc::{StaticChannelSet, SvcMessage, SvcProcessor as _};
    use std::any::TypeId;

    for image in [false, true] {
        let state = new_shared();
        let mut clip = CliprdrClient::new(Box::new(AditCliprdrBackend::new(Arc::clone(&state))));
        clip.process(&ironrdp_core::encode_vec(&ClipboardPdu::MonitorReady).unwrap())
            .unwrap();
        state.lock().unwrap().take_actions();
        let _: Vec<SvcMessage> = clip.initiate_copy(&[]).unwrap().into();
        clip.process(
            &ironrdp_core::encode_vec(&ClipboardPdu::FormatListResponse(FormatListResponse::Ok))
                .unwrap(),
        )
        .unwrap();
        if image {
            offer_local_image(&state, vec![0x44, 0x55, 0x66]);
        } else {
            offer_local_text(&state, "synthetic-private-text".into());
        }
        state.lock().unwrap().take_actions();
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
        let (detached, revision) = {
            let mut guard = state.lock().unwrap();
            (guard.take_actions(), guard.local_revision)
        };
        assert_eq!(detached.len(), 1);

        let mut channels = StaticChannelSet::new();
        channels.insert(clip);
        channels.attach_channel_id(TypeId::of::<CliprdrClient>(), 1004);
        let mut stage = ActiveStageBuilder {
            static_channels: channels,
            user_channel_id: 1001,
            io_channel_id: 1003,
            message_channel_id: None,
            share_id: 1,
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: false,
        }
        .build();
        let (reader, mut wire) = std::io::pipe().unwrap();
        write_msg(
            &mut wire,
            &ClientMsg::EpochClipboardFiles {
                epoch: 0,
                files: Vec::new(),
            },
        )
        .unwrap();
        // Re-offering the same bytes must not revive the old response.
        let fresh = if image {
            ClientMsg::EpochClipboardImage {
                epoch: 0,
                image: vec![0x44, 0x55, 0x66],
            }
        } else {
            ClientMsg::EpochInput {
                epoch: 0,
                event: InputEvent::ClipboardText("synthetic-private-text".into()),
            }
        };
        write_msg(&mut wire, &fresh).unwrap();
        write_msg(&mut wire, &ClientMsg::Close).unwrap();
        drop(wire);
        let (request_tx, _request_rx) = std_mpsc::channel();
        let (input_tx, _input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (display_tx, _display_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::host::read_app_messages(
            reader,
            request_tx,
            input_tx,
            display_tx,
            Arc::new(InputRouting::default()),
            Arc::clone(&state),
        );
        let (host_tx, _host_rx) = std_mpsc::channel();
        let frames = pump_actions(&state, &mut stage, &host_tx, 0, detached, revision).unwrap();
        assert_eq!(frames.len(), 1);
        let message: X224<SendDataRequest<'_>> = decode(&frames[0]).unwrap();
        let response: ClipboardPdu<'_> = decode(&message.0.user_data[8..]).unwrap();
        assert!(
            matches!(response, ClipboardPdu::FormatDataResponse(response) if response.is_error()),
            "a barrier must sanitize a response already detached from the shared queue"
        );
    }
}

#[test]
fn revoked_detached_format_list_preserves_initial_handshake_without_stealing_ownership() {
    use crate::session::InputRouting;
    use adit_rdp_proto::{write_msg, ClientMsg};
    use ironrdp_cliprdr::pdu::{ClipboardPdu, FormatListResponse};
    use ironrdp_pdu::{mcs::SendDataRequest, x224::X224};
    use ironrdp_session::ActiveStageBuilder;
    use ironrdp_svc::{StaticChannelSet, SvcMessage, SvcProcessor as _};
    use std::any::TypeId;

    for ready in [false, true] {
        let state = new_shared();
        let mut clip = CliprdrClient::new(Box::new(AditCliprdrBackend::new(Arc::clone(&state))));
        if ready {
            clip.process(&ironrdp_core::encode_vec(&ClipboardPdu::MonitorReady).unwrap())
                .unwrap();
            state.lock().unwrap().take_actions();
            let _: Vec<SvcMessage> = clip.initiate_copy(&[]).unwrap().into();
            clip.process(
                &ironrdp_core::encode_vec(&ClipboardPdu::FormatListResponse(
                    FormatListResponse::Ok,
                ))
                .unwrap(),
            )
            .unwrap();
        }
        offer_local_text(&state, "stale-advertisement".into());
        if !ready {
            clip.process(&ironrdp_core::encode_vec(&ClipboardPdu::MonitorReady).unwrap())
                .unwrap();
        }
        let (detached, revision) = {
            let mut guard = state.lock().unwrap();
            (guard.take_actions(), guard.local_revision)
        };
        assert!(detached.iter().any(|action| matches!(action,
            ClipboardAction::Advertise(ids) if ids.contains(&ClipboardFormatId::CF_UNICODETEXT))));
        let mut channels = StaticChannelSet::new();
        channels.insert(clip);
        channels.attach_channel_id(TypeId::of::<CliprdrClient>(), 1004);
        let mut stage = ActiveStageBuilder {
            static_channels: channels,
            user_channel_id: 1001,
            io_channel_id: 1003,
            message_channel_id: None,
            share_id: 1,
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: false,
        }
        .build();
        let (reader, mut wire) = std::io::pipe().unwrap();
        write_msg(
            &mut wire,
            &ClientMsg::EpochClipboardFiles {
                epoch: 0,
                files: Vec::new(),
            },
        )
        .unwrap();
        write_msg(&mut wire, &ClientMsg::Close).unwrap();
        drop(wire);
        let (request_tx, _request_rx) = std_mpsc::channel();
        let (input_tx, _input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (display_tx, _display_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::host::read_app_messages(
            reader,
            request_tx,
            input_tx,
            display_tx,
            Arc::new(InputRouting::default()),
            Arc::clone(&state),
        );
        let (host_tx, _host_rx) = std_mpsc::channel();
        let frames = pump_actions(&state, &mut stage, &host_tx, 0, detached, revision).unwrap();
        if ready {
            assert!(
                frames.is_empty(),
                "a stale list must not reclaim the clipboard"
            );
        } else {
            assert_eq!(frames.len(), 1);
            let mut offset = 0;
            let mut saw_empty_list = false;
            while offset < frames[0].len() {
                let len =
                    u16::from_be_bytes([frames[0][offset + 2], frames[0][offset + 3]]) as usize;
                let message: X224<SendDataRequest<'_>> =
                    decode(&frames[0][offset..offset + len]).unwrap();
                if let ClipboardPdu::FormatList(list) = decode(&message.0.user_data[8..]).unwrap() {
                    assert!(list.get_formats(true).unwrap().is_empty());
                    saw_empty_list = true;
                }
                offset += len;
            }
            assert!(saw_empty_list);
            stage
                .get_svc_processor_mut::<CliprdrClient>()
                .unwrap()
                .process(
                    &ironrdp_core::encode_vec(&ClipboardPdu::FormatListResponse(
                        FormatListResponse::Ok,
                    ))
                    .unwrap(),
                )
                .unwrap();
            assert!(
                state.lock().unwrap().ready,
                "the empty initial list must complete the handshake"
            );
        }
    }
}
