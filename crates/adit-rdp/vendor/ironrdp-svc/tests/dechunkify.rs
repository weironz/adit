//! Receive regressions registered as the helper's `svc_dechunkify` test target.
//! Wire fixtures come from the Microsoft examples linked below; SVC framing and
//! fragmentation around them are synthetic. No host or OS clipboard is used.

use ironrdp_cliprdr::pdu::ClipboardPdu;
use ironrdp_core::{decode, encode_vec, impl_as_any};
use ironrdp_svc::pdu::gcc::ChannelName;
use ironrdp_svc::pdu::PduResult;
use ironrdp_svc::{
    ChannelFlags, StaticVirtualChannel, SvcMessage, SvcProcessor, MAX_STATIC_CHANNEL_PDU_BYTES,
};

const FIRST: u32 = 0x01;
const LAST: u32 = 0x02;
const SHOW_PROTOCOL: u32 = 0x10;
const SUSPEND: u32 = 0x20;
const RESUME: u32 = 0x40;
const SHADOW_PERSISTENT: u32 = 0x80;
const COMPRESSED: u32 = 0x0020_0000;
const AT_FRONT: u32 = 0x0040_0000;
const FLUSHED: u32 = 0x0080_0000;

// MS-RDPECLIP 4.1.1, server capabilities (the annotated Microsoft wire dump):
// https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeclip/017fbfda-2f6d-449a-b9cf-396b2cc58591
const SERVER_CAPABILITIES: &[u8] = &[
    0x07, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x0c, 0x00,
    0x02, 0x00, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00,
];

// MS-RDPECLIP 4.4.2, successful UTF-16 "hello world" format-data response:
// https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeclip/539f2ca7-ee67-46ba-8995-b2274fd8bcea
const TEXT_RESPONSE: &[u8] = &[
    0x05, 0x00, 0x01, 0x00, 0x18, 0x00, 0x00, 0x00, 0x68, 0x00, 0x65, 0x00, 0x6c, 0x00, 0x6c, 0x00,
    0x6f, 0x00, 0x20, 0x00, 0x77, 0x00, 0x6f, 0x00, 0x72, 0x00, 0x6c, 0x00, 0x64, 0x00, 0x00, 0x00,
];

#[derive(Debug)]
struct Receiver {
    name: ChannelName,
    messages: Vec<Vec<u8>>,
}

impl_as_any!(Receiver);

impl SvcProcessor for Receiver {
    fn channel_name(&self) -> ChannelName {
        self.name.clone()
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        self.messages.push(payload.to_vec());
        Ok(Vec::new())
    }
}

fn channel(name: [u8; 8]) -> StaticVirtualChannel {
    StaticVirtualChannel::new(Receiver {
        name: ChannelName::new(name),
        messages: Vec::new(),
    })
}

fn frame(length: u32, flags: u32, data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(8 + data.len());
    result.extend_from_slice(&length.to_le_bytes());
    result.extend_from_slice(&flags.to_le_bytes());
    result.extend_from_slice(data);
    result
}

fn messages(channel: &StaticVirtualChannel) -> &[Vec<u8>] {
    &channel
        .channel_processor_downcast_ref::<Receiver>()
        .unwrap()
        .messages
}

fn assert_recovered(channel: &mut StaticVirtualChannel) {
    let count = messages(channel).len();
    // Recovery must also reset sequence state: an unflagged complete message
    // must work immediately, without requiring an extra FIRST to clear it.
    channel.process(&frame(2, 0, b"ok")).unwrap();
    assert_eq!(messages(channel).len(), count + 1);
    assert_eq!(messages(channel).last().unwrap(), b"ok");
}

#[test]
fn accepts_chunks_produced_by_the_actual_encoder() {
    let mut channel = channel(*b"cliprdr\0");
    for length in [0_usize, 1, 1599, 1600, 1601, 10_000] {
        let bytes: Vec<u8> = (0..length)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let chunks = StaticVirtualChannel::chunkify(vec![
            SvcMessage::from(bytes.clone()).with_flags(ChannelFlags::SHOW_PROTOCOL)
        ])
        .unwrap();
        assert_eq!(chunks.len(), length.max(1).div_ceil(1600));
        for chunk in chunks {
            channel.process(chunk.filled()).unwrap();
        }
        assert_eq!(messages(&channel).last().unwrap(), &bytes);
    }
}

#[test]
fn reassembles_the_microsoft_2062_byte_three_chunk_example() {
    // MS-RDPBCGR 3.1.5.2.2.1: FIRST(1000), neither flag(1000), LAST(62).
    // https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/a542bf19-1c86-4c80-ab3e-61449653abf6
    let bytes: Vec<u8> = (0..2062).map(|i| u8::try_from(i % 251).unwrap()).collect();
    let mut channel = channel(*b"rdpsnd\0\0");
    channel
        .process(&frame(2062, FIRST, &bytes[..1000]))
        .unwrap();
    channel
        .process(&frame(2062, 0, &bytes[1000..2000]))
        .unwrap();
    assert!(messages(&channel).is_empty());
    channel.process(&frame(2062, LAST, &bytes[2000..])).unwrap();
    assert_eq!(messages(&channel), &[bytes]);
}

#[test]
fn accepts_complete_unflagged_and_last_only_messages_when_idle() {
    let mut channel = channel(*b"cliprdr\0");
    for flags in [
        0,
        LAST,
        SHOW_PROTOCOL,
        LAST | SHOW_PROTOCOL,
        SHADOW_PERSISTENT,
        0x8000_0000,
    ] {
        channel.process(&frame(2, flags, b"ok")).unwrap();
        assert_eq!(messages(&channel).last().unwrap(), b"ok");
        channel.process(&frame(0, flags, b"")).unwrap();
        assert!(messages(&channel).last().unwrap().is_empty());
    }
    assert_eq!(messages(&channel).len(), 12);
}

#[test]
fn accepts_empty_first_last_pdus_and_empty_fragment_sequences() {
    let mut channel = channel(*b"drdynvc\0");
    channel.process(&frame(0, FIRST | LAST, b"")).unwrap();
    channel.process(&frame(0, FIRST, b"")).unwrap();
    channel.process(&frame(0, 0, b"")).unwrap();
    assert_eq!(messages(&channel), &[Vec::<u8>::new()]);
    channel.process(&frame(0, LAST, b"")).unwrap();
    assert_eq!(messages(&channel), &[Vec::<u8>::new(), Vec::new()]);
}

#[test]
fn accepts_empty_start_middle_and_last_chunks_for_nonempty_data() {
    let mut channel = channel(*b"cliprdr\0");
    channel.process(&frame(4, FIRST, b"")).unwrap();
    channel.process(&frame(4, 0, b"ab")).unwrap();
    channel.process(&frame(4, 0, b"")).unwrap();
    channel.process(&frame(4, 0, b"cd")).unwrap();
    assert!(messages(&channel).is_empty());
    channel.process(&frame(4, LAST, b"")).unwrap();
    assert_eq!(messages(&channel), &[b"abcd".to_vec()]);

    channel.process(&frame(2, FIRST, b"ok")).unwrap();
    assert_eq!(messages(&channel).len(), 1);
    channel.process(&frame(2, LAST, b"")).unwrap();
    assert_eq!(messages(&channel).last().unwrap(), b"ok");
}

#[test]
fn a_new_first_resynchronizes_instead_of_concatenating_old_bytes() {
    let mut channel = channel(*b"cliprdr\0");
    channel.process(&frame(8, FIRST, b"stale")).unwrap();
    channel.process(&frame(2, FIRST | LAST, b"ok")).unwrap();
    assert_eq!(messages(&channel), &[b"ok".to_vec()]);

    channel.process(&frame(8, FIRST, b"stale")).unwrap();
    channel.process(&frame(4, FIRST, b"ab")).unwrap();
    channel.process(&frame(4, LAST, b"cd")).unwrap();
    assert_eq!(messages(&channel).last().unwrap(), b"abcd");
}

#[test]
fn rejects_incomplete_or_oversized_complete_messages_and_recovers() {
    let mut channel = channel(*b"cliprdr\0");
    for flags in [0, LAST, FIRST | LAST] {
        for length in [0, 1, 3] {
            let count = messages(&channel).len();
            assert!(channel.process(&frame(length, flags, b"ab")).is_err());
            assert_eq!(messages(&channel).len(), count);
            assert_recovered(&mut channel);
        }
    }
    assert!(channel.process(&frame(1, FIRST, b"ab")).is_err());
    assert_recovered(&mut channel);
}

#[test]
fn rejects_changed_total_lengths_on_middle_and_last_chunks() {
    let mut channel = channel(*b"cliprdr\0");
    for flags in [0, LAST] {
        for length in [0, 3, 5] {
            channel.process(&frame(4, FIRST, b"a")).unwrap();
            let count = messages(&channel).len();
            assert!(channel.process(&frame(length, flags, b"bcd")).is_err());
            assert_eq!(messages(&channel).len(), count);
            assert_recovered(&mut channel);
        }
    }
}

#[test]
fn rejects_short_last_and_append_past_declared_length() {
    let mut channel = channel(*b"cliprdr\0");
    for (flags, data) in [(LAST, &b"b"[..]), (LAST, &b"bcde"[..]), (0, &b"bcde"[..])] {
        channel.process(&frame(4, FIRST, b"a")).unwrap();
        let count = messages(&channel).len();
        assert!(channel.process(&frame(4, flags, data)).is_err());
        assert_eq!(messages(&channel).len(), count);
        assert_recovered(&mut channel);
    }
}

#[test]
fn a_missing_last_cannot_cause_unbounded_append() {
    let mut channel = channel(*b"cliprdr\0");
    channel.process(&frame(5, FIRST, b"a")).unwrap();
    for _ in 0..4 {
        channel.process(&frame(5, 0, b"b")).unwrap();
    }
    assert!(messages(&channel).is_empty());
    for _ in 0..1000 {
        assert!(channel.process(&frame(5, 0, b"b")).is_err());
    }
    assert!(messages(&channel).is_empty());
    assert_recovered(&mut channel);
}

#[test]
fn every_truncated_header_discards_partial_data() {
    let mut channel = channel(*b"cliprdr\0");
    for length in 0..8 {
        channel.process(&frame(1024, FIRST, &[0xa5; 512])).unwrap();
        let count = messages(&channel).len();
        assert!(channel.process(&[0u8; 8][..length]).is_err());
        assert_eq!(messages(&channel).len(), count);
        assert_recovered(&mut channel);
    }
}

#[test]
fn rejects_oversized_claims_with_or_without_data_and_recovers() {
    let mut channel = channel(*b"cliprdr\0");
    let max = u32::try_from(MAX_STATIC_CHANNEL_PDU_BYTES).unwrap();
    for length in [max + 1, u32::MAX] {
        for flags in [0, FIRST, LAST, FIRST | LAST] {
            for data in [&b""[..], &b"x"[..]] {
                channel.process(&frame(1024, FIRST, &[0xa5; 512])).unwrap();
                let count = messages(&channel).len();
                assert!(channel.process(&frame(length, flags, data)).is_err());
                assert_eq!(messages(&channel).len(), count);
                assert_recovered(&mut channel);
            }
        }
    }
}

#[test]
fn a_maximum_claim_can_begin_with_only_one_received_byte() {
    let mut channel = channel(*b"cliprdr\0");
    let max = u32::try_from(MAX_STATIC_CHANNEL_PDU_BYTES).unwrap();
    channel.process(&frame(max, FIRST, b"a")).unwrap();
    assert!(messages(&channel).is_empty());
    assert!(channel.process(&frame(max, LAST, b"b")).is_err());
    assert_recovered(&mut channel);
}

#[test]
fn compressed_chunks_are_explicitly_rejected_before_length_validation() {
    let mut channel = channel(*b"cliprdr\0");
    for package in [0, 1, 2, 3, 15] {
        for history in [0, AT_FRONT, FLUSHED, AT_FRONT | FLUSHED] {
            for flags in [0, FIRST, LAST, FIRST | LAST] {
                channel.process(&frame(4, FIRST, b"a")).unwrap();
                let count = messages(&channel).len();
                let error = channel
                    .process(&frame(
                        4000,
                        flags | COMPRESSED | history | (package << 16),
                        b"encoded",
                    ))
                    .unwrap_err();
                assert!(format!("{error:?}").contains("compressed SVC data is unsupported"));
                assert_eq!(messages(&channel).len(), count);
                assert_recovered(&mut channel);
            }
        }
    }
    // Equal encoded/plain lengths, including an empty body, do not establish
    // that the input has already been decompressed.
    for data in [&b""[..], &b"ok"[..]] {
        let error = channel
            .process(&frame(
                data.len().try_into().unwrap(),
                FIRST | LAST | COMPRESSED,
                data,
            ))
            .unwrap_err();
        assert!(format!("{error:?}").contains("compressed SVC data is unsupported"));
        assert_recovered(&mut channel);
    }
}

#[test]
fn uncompressed_flushed_data_and_package_metadata_still_work() {
    // MS-RDPBCGR 3.1.8.2.1 allows FLUSHED without COMPRESSED. A package
    // identifier alone does not turn literal bytes into encoded data.
    let mut channel = channel(*b"cliprdr\0");
    for package in 0..=3 {
        let metadata = (package << 16) | FLUSHED;
        channel.process(&frame(4, FIRST | metadata, b"ab")).unwrap();
        channel.process(&frame(4, LAST | metadata, b"cd")).unwrap();
        assert_eq!(messages(&channel).last().unwrap(), b"abcd");
        channel.process(&frame(2, metadata, b"ok")).unwrap();
        assert_eq!(messages(&channel).last().unwrap(), b"ok");
    }
}

#[test]
fn suspend_resume_preserve_an_incomplete_message_without_dispatching_data() {
    let mut channel = channel(*b"drdynvc\0");
    for boundary in [0, FIRST, LAST, FIRST | LAST] {
        // Include exactly FIRST(4, "a") -> SUSPEND -> RESUME -> LAST(4, "bcd").
        // Header-only controls have no application bytes; their length and
        // boundary bits must neither terminate nor reset an active sequence.
        for length in [0, 4, u32::MAX] {
            channel.process(&frame(4, FIRST, b"a")).unwrap();
            let count = messages(&channel).len();
            for control in [SUSPEND, RESUME] {
                let responses = channel
                    .process(&frame(length, control | boundary, b""))
                    .unwrap();
                assert!(responses.is_empty());
                assert_eq!(messages(&channel).len(), count);
            }
            channel.process(&frame(4, LAST, b"bcd")).unwrap();
            assert_eq!(messages(&channel).len(), count + 1);
            assert_eq!(messages(&channel).last().unwrap(), b"abcd");
        }
    }
}

#[test]
fn idle_suspend_resume_controls_do_not_dispatch_or_start_a_message() {
    let mut channel = channel(*b"drdynvc\0");
    for control in [SUSPEND, RESUME, SUSPEND | RESUME] {
        for boundary in [0, FIRST, LAST, FIRST | LAST] {
            for length in [0, 4, u32::MAX] {
                let count = messages(&channel).len();
                let responses = channel
                    .process(&frame(length, control | boundary | SHOW_PROTOCOL, b""))
                    .unwrap();
                assert!(responses.is_empty());
                assert_eq!(messages(&channel).len(), count);
                // An unflagged PDU would be mistaken for a continuation if a
                // control's FIRST/length had established any sequence state.
                assert_recovered(&mut channel);
            }
        }
    }
}

#[test]
fn control_bits_on_data_bearing_chunks_do_not_discard_the_payload() {
    let mut channel = channel(*b"cliprdr\0");
    channel.process(&frame(4, FIRST | SUSPEND, b"a")).unwrap();
    channel.process(&frame(4, RESUME, b"b")).unwrap();
    assert!(messages(&channel).is_empty());
    channel
        .process(&frame(4, LAST | SUSPEND | RESUME, b"cd"))
        .unwrap();
    assert_eq!(messages(&channel), &[b"abcd".to_vec()]);
}

#[test]
fn microsoft_clipboard_wire_samples_roundtrip_in_complete_and_fragmented_forms() {
    for (sample, name) in [
        (SERVER_CAPABILITIES, "CLIPRDR_CAPABILITIES"),
        (TEXT_RESPONSE, "CLIPRDR_FORMAT_DATA_RESPONSE"),
    ] {
        let mut channel = channel(*b"cliprdr\0");
        let length = u32::try_from(sample.len()).unwrap();
        for flags in [0, LAST, FIRST | LAST, FIRST | LAST | SHOW_PROTOCOL] {
            channel.process(&frame(length, flags, sample)).unwrap();
        }
        // Split even the CLIPRDR header; a channel fragment is not an inner PDU.
        channel
            .process(&frame(length, FIRST | SHOW_PROTOCOL, &sample[..3]))
            .unwrap();
        channel
            .process(&frame(length, SHOW_PROTOCOL, &sample[3..8]))
            .unwrap();
        channel
            .process(&frame(length, LAST | SHOW_PROTOCOL, &sample[8..]))
            .unwrap();
        assert_eq!(messages(&channel).len(), 5);
        for received in messages(&channel) {
            assert_eq!(received, sample);
            let decoded = decode::<ClipboardPdu<'_>>(received).unwrap();
            assert_eq!(decoded.message_name(), name);
            assert_eq!(encode_vec(&decoded).unwrap(), sample);
        }
    }
}

#[test]
fn an_error_on_one_channel_does_not_corrupt_another_channels_sequence() {
    let mut clip = channel(*b"cliprdr\0");
    let mut audio = channel(*b"rdpsnd\0\0");
    clip.process(&frame(4, FIRST, b"a")).unwrap();
    audio.process(&frame(4, FIRST, b"1")).unwrap();
    assert!(clip.process(&frame(4, LAST, b"b")).is_err());
    audio.process(&frame(4, LAST, b"234")).unwrap();
    assert_eq!(messages(&audio), &[b"1234".to_vec()]);
    assert_recovered(&mut clip);
}

#[derive(Debug, Default)]
struct SizeReceiver {
    length: Option<usize>,
}

impl_as_any!(SizeReceiver);

impl SvcProcessor for SizeReceiver {
    fn channel_name(&self) -> ChannelName {
        ChannelName::new(*b"cliprdr\0")
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        // Avoid retaining a second 128 MiB copy in this boundary test.
        assert!(payload.iter().all(|&byte| byte == 0xa5));
        self.length = Some(payload.len());
        Ok(Vec::new())
    }
}

#[test]
fn accepts_a_message_exactly_at_the_limit_including_payloads_above_64_mib() {
    let mut channel = StaticVirtualChannel::new(SizeReceiver::default());
    let length = u32::try_from(MAX_STATIC_CHANNEL_PDU_BYTES).unwrap();
    // Larger chunks can be negotiated; do not hard-code 1600 as a receive cap.
    let chunk = [0xa5; 64 * 1024];
    for offset in (0..MAX_STATIC_CHANNEL_PDU_BYTES).step_by(chunk.len()) {
        let mut flags = SHOW_PROTOCOL;
        if offset == 0 {
            flags |= FIRST;
        }
        if offset + chunk.len() == MAX_STATIC_CHANNEL_PDU_BYTES {
            flags |= LAST;
        }
        channel.process(&frame(length, flags, &chunk)).unwrap();
    }
    assert_eq!(
        channel
            .channel_processor_downcast_ref::<SizeReceiver>()
            .unwrap()
            .length,
        Some(MAX_STATIC_CHANNEL_PDU_BYTES),
    );
}
