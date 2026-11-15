//! xrdp answers a client Monitor Data block with a Monitor Layout PDU during
//! connection finalization, before the Font Map. The vendored connector used to
//! reject it as an unexpected server message, which failed every multi-monitor
//! connect to xrdp (#244).

use std::borrow::Cow;

use ironrdp_connector::{
    ConnectionFinalizationSequence, ConnectionFinalizationState, Sequence as _,
};
use ironrdp_core::{encode_vec, WriteBuf};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_pdu::mcs::SendDataIndication;
use ironrdp_pdu::rdp::client_info::CompressionType;
use ironrdp_pdu::rdp::finalization_messages::{FontPdu, MonitorLayoutPdu};
use ironrdp_pdu::rdp::headers::{
    CompressionFlags, ShareControlHeader, ShareControlPdu, ShareDataHeader, ShareDataPdu,
    StreamPriority,
};
use ironrdp_pdu::x224::X224;

const IO_CHANNEL: u16 = 1003;
const USER_CHANNEL: u16 = 1007;
const SHARE_ID: u32 = 0x1_03ea;

fn server_share_data(pdu: ShareDataPdu) -> Vec<u8> {
    let share_control = ShareControlHeader {
        share_control_pdu: ShareControlPdu::Data(ShareDataHeader {
            share_data_pdu: pdu,
            stream_priority: StreamPriority::Medium,
            compression_flags: CompressionFlags::empty(),
            compression_type: CompressionType::K8,
        }),
        pdu_source: 1002,
        share_id: SHARE_ID,
    };
    let indication = SendDataIndication {
        initiator_id: 1002,
        channel_id: IO_CHANNEL,
        user_data: Cow::Owned(encode_vec(&share_control).expect("share control encodes")),
    };
    encode_vec(&X224(indication)).expect("send data indication encodes")
}

fn waiting_for_response() -> ConnectionFinalizationSequence {
    let mut sequence = ConnectionFinalizationSequence::new(IO_CHANNEL, USER_CHANNEL, SHARE_ID);
    sequence.state = ConnectionFinalizationState::WaitForResponse;
    sequence
}

#[test]
fn a_monitor_layout_before_the_font_map_does_not_fail_the_connection() {
    let mut sequence = waiting_for_response();
    let mut output = WriteBuf::new();

    let layout = ShareDataPdu::MonitorLayout(MonitorLayoutPdu {
        monitors: vec![
            Monitor {
                left: 0,
                top: 0,
                right: 1279,
                bottom: 799,
                flags: MonitorFlags::PRIMARY,
            },
            Monitor {
                left: 1280,
                top: 0,
                right: 2559,
                bottom: 799,
                flags: MonitorFlags::empty(),
            },
        ],
    });
    sequence
        .step(&server_share_data(layout), &mut output)
        .expect("a Monitor Layout PDU is accepted during finalization");
    assert!(matches!(
        sequence.state,
        ConnectionFinalizationState::WaitForResponse
    ));

    sequence
        .step(
            &server_share_data(ShareDataPdu::FontMap(FontPdu::default())),
            &mut output,
        )
        .expect("the Font Map that follows still finishes finalization");
    assert!(matches!(
        sequence.state,
        ConnectionFinalizationState::Finished
    ));
}
