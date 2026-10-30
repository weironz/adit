//! Regression coverage through the public static-channel processing path.

use ironrdp_svc::pdu::gcc::ChannelName;
use ironrdp_svc::pdu::{ironrdp_core::impl_as_any, PduResult};
use ironrdp_svc::{StaticVirtualChannel, SvcMessage, SvcProcessor, MAX_STATIC_CHANNEL_PDU_BYTES};

const FIRST: u32 = 0x01;
const LAST: u32 = 0x02;
const SHOW_PROTOCOL: u32 = 0x10;
const COMPRESSED: u32 = 0x0020_0000;

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
    channel.process(&frame(2, FIRST | LAST, b"ok")).unwrap();
    assert_eq!(messages(channel).len(), count + 1);
    assert_eq!(messages(channel).last().unwrap(), b"ok");
}

#[test]
fn reassembles_ordered_chunks_and_multiple_complete_messages() {
    let mut channel = channel(*b"cliprdr\0");
    channel
        .process(&frame(6, FIRST | SHOW_PROTOCOL, b"ab"))
        .unwrap();
    channel.process(&frame(6, SHOW_PROTOCOL, b"cd")).unwrap();
    assert!(messages(&channel).is_empty());
    channel
        .process(&frame(6, LAST | SHOW_PROTOCOL, b"ef"))
        .unwrap();
    channel.process(&frame(2, FIRST | LAST, b"gh")).unwrap();
    channel.process(&frame(0, FIRST | LAST, b"")).unwrap();
    // MS-RDPBCGR also allows a receiver to dispatch an unfragmented PDU with
    // neither boundary flag when no fragmented message is in progress.
    channel.process(&frame(2, 0, b"ij")).unwrap();
    assert_eq!(
        messages(&channel),
        &[b"abcdef".to_vec(), b"gh".to_vec(), vec![], b"ij".to_vec()]
    );
}

#[test]
fn accepts_chunks_produced_by_the_actual_encoder() {
    let bytes: Vec<u8> = (0..10_000)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let chunks = StaticVirtualChannel::chunkify(vec![SvcMessage::from(bytes.clone())]).unwrap();
    assert!(chunks.len() > 1);
    let mut channel = channel(*b"rdpsnd\0\0");
    for chunk in chunks {
        channel.process(chunk.filled()).unwrap();
    }
    assert_eq!(messages(&channel), &[bytes]);
}

#[test]
fn rejects_a_missing_last_before_unbounded_append_and_recovers() {
    let mut channel = channel(*b"cliprdr\0");
    channel.process(&frame(5, FIRST, b"a")).unwrap();
    for _ in 0..3 {
        channel.process(&frame(5, 0, b"b")).unwrap();
    }
    // Reaching the declared length without LAST is already invalid; no extra
    // bytes should be appended, even if more otherwise well-formed PDUs arrive.
    assert!(channel.process(&frame(5, 0, b"b")).is_err());
    for _ in 0..1_000 {
        assert!(channel.process(&frame(5, 0, b"b")).is_err());
    }
    assert!(messages(&channel).is_empty());
    assert_recovered(&mut channel);
}

#[test]
fn rejects_out_of_order_boundaries_and_discards_the_old_message() {
    let mut channel = channel(*b"cliprdr\0");
    assert!(channel.process(&frame(2, LAST, b"ok")).is_err());
    assert!(channel.process(&frame(4, 0, b"x")).is_err());
    channel.process(&frame(4, FIRST, b"a")).unwrap();
    assert!(channel.process(&frame(4, FIRST, b"b")).is_err());
    assert_recovered(&mut channel);

    channel.process(&frame(4, FIRST, b"a")).unwrap();
    assert!(channel.process(&frame(2, FIRST | LAST, b"ok")).is_err());
    assert_recovered(&mut channel);
}

#[test]
fn rejects_inconsistent_or_incorrect_declared_lengths() {
    let mut channel = channel(*b"cliprdr\0");
    assert!(channel.process(&frame(1, FIRST | LAST, b"ab")).is_err());
    assert!(channel.process(&frame(3, FIRST | LAST, b"ab")).is_err());
    assert!(channel.process(&frame(1, FIRST, b"ab")).is_err());
    assert!(channel.process(&frame(1, FIRST, b"a")).is_err());

    channel.process(&frame(4, FIRST, b"a")).unwrap();
    assert!(channel.process(&frame(5, 0, b"b")).is_err());
    assert_recovered(&mut channel);

    channel.process(&frame(3, FIRST, b"ab")).unwrap();
    assert!(channel.process(&frame(3, LAST, b"cd")).is_err());
    assert_recovered(&mut channel);
}

#[test]
fn rejects_truncated_last_or_header_and_discards_partial_bytes() {
    let mut channel = channel(*b"cliprdr\0");
    channel.process(&frame(4, FIRST, b"a")).unwrap();
    assert!(channel.process(&frame(4, LAST, b"b")).is_err());
    assert_recovered(&mut channel);
    for len in 0..8 {
        channel.process(&frame(4, FIRST, b"a")).unwrap();
        assert!(channel.process(&[0u8; 8][..len]).is_err());
        assert_recovered(&mut channel);
    }
}

#[test]
fn bounds_declared_length_before_allocating_or_appending() {
    let mut channel = channel(*b"cliprdr\0");
    let max_message = u32::try_from(MAX_STATIC_CHANNEL_PDU_BYTES).unwrap();
    assert!(channel
        .process(&frame(max_message + 1, FIRST, b"a"))
        .is_err());
    assert!(channel
        .process(&frame(u32::MAX, FIRST | LAST, b"a"))
        .is_err());
    assert_recovered(&mut channel);
    // A message exactly at the production limit is allowed to begin; the
    // truncated LAST must then be rejected and must not poison the next PDU.
    channel.process(&frame(max_message, FIRST, b"a")).unwrap();
    assert!(channel.process(&frame(max_message, LAST, b"b")).is_err());
    assert_recovered(&mut channel);
}

#[test]
fn rejects_compressed_svc_data_without_treating_compressed_length_as_plaintext() {
    let mut channel = channel(*b"cliprdr\0");
    channel.process(&frame(4, FIRST, b"a")).unwrap();
    assert!(channel
        .process(&frame(4, LAST | COMPRESSED, b"bcd"))
        .is_err());
    assert_recovered(&mut channel);
    assert!(channel
        .process(&frame(2, FIRST | LAST | COMPRESSED, b"ok"))
        .is_err());
    assert_recovered(&mut channel);
}

#[test]
fn error_on_one_static_channel_does_not_contaminate_another() {
    let mut clip = channel(*b"cliprdr\0");
    let mut other = channel(*b"rdpsnd\0\0");
    clip.process(&frame(4, FIRST, b"a")).unwrap();
    other.process(&frame(4, FIRST, b"1")).unwrap();
    assert!(clip.process(&frame(4, LAST, b"b")).is_err());
    other.process(&frame(4, LAST, b"234")).unwrap();
    assert_eq!(messages(&other), &[b"1234".to_vec()]);
    assert_recovered(&mut clip);
}

#[test]
fn accepts_large_fragmented_payload_above_the_existing_image_cap() {
    const TOTAL: usize = 64 * 1024 * 1024 + 1024;
    const CHUNK: usize = 64 * 1024; // A server may negotiate larger SVC chunks.
    let mut channel = channel(*b"cliprdr\0");
    let data = vec![0xA5; CHUNK];
    let mut sent = 0usize;
    while sent < TOTAL {
        let count = (TOTAL - sent).min(CHUNK);
        let mut flags = SHOW_PROTOCOL;
        if sent == 0 {
            flags |= FIRST;
        }
        if sent + count == TOTAL {
            flags |= LAST;
        }
        channel
            .process(&frame(TOTAL.try_into().unwrap(), flags, &data[..count]))
            .unwrap();
        sent += count;
    }
    assert_eq!(messages(&channel).len(), 1);
    assert_eq!(messages(&channel)[0].len(), TOTAL);
    assert!(messages(&channel)[0].iter().all(|&byte| byte == 0xA5));
}
