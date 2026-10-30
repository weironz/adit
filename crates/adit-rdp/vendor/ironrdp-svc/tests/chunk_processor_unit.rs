//! Internal invariants which cannot be observed through the channel API.

use super::{ChunkProcessor, MAX_STATIC_CHANNEL_PDU_BYTES};

const FIRST: u32 = 0x01;
const LAST: u32 = 0x02;
const COMPRESSED: u32 = 0x0020_0000;

fn frame(length: u32, flags: u32, data: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    result.extend_from_slice(&length.to_le_bytes());
    result.extend_from_slice(&flags.to_le_bytes());
    result.extend_from_slice(data);
    result
}

#[test]
fn a_large_claim_does_not_preallocate_the_declared_size() {
    let mut processor = ChunkProcessor::new();
    let max = u32::try_from(MAX_STATIC_CHANNEL_PDU_BYTES).unwrap();
    assert!(processor.dechunkify(&frame(max, FIRST, b"a")).unwrap().is_none());
    assert_eq!(processor.expected_len, Some(MAX_STATIC_CHANNEL_PDU_BYTES));
    assert_eq!(processor.chunked_pdu, b"a");
    assert!(processor.chunked_pdu.capacity() < 1024);
}

#[test]
fn every_error_releases_the_partial_allocation_and_expected_length() {
    let max = u32::try_from(MAX_STATIC_CHANNEL_PDU_BYTES).unwrap();
    let failures = [
        vec![0, 1, 2],                           // truncated header
        frame(max + 1, 0, b"a"),                 // oversized claim
        frame(8, FIRST, b"a"),                   // duplicate FIRST
        frame(9, 0, b"a"),                       // inconsistent claim
        frame(8, LAST, b"a"),                    // premature LAST
        frame(8, 0, b"abcdefgh"),                // declared length exceeded
        frame(8, LAST | COMPRESSED, b"bcdefgh"), // unsupported compression
    ];
    for failure in failures {
        let mut processor = ChunkProcessor::new();
        assert!(processor.dechunkify(&frame(8, FIRST, b"a")).unwrap().is_none());
        assert_ne!(processor.chunked_pdu.capacity(), 0);
        assert!(processor.dechunkify(&failure).is_err());
        assert_eq!(processor.expected_len, None);
        assert_eq!(processor.chunked_pdu.len(), 0);
        assert_eq!(processor.chunked_pdu.capacity(), 0);
        assert_eq!(
            processor.dechunkify(&frame(2, FIRST | LAST, b"ok")).unwrap(),
            Some(b"ok".to_vec())
        );
    }
}

#[test]
fn reassembly_tracks_received_bytes_until_the_message_completes() {
    let mut processor = ChunkProcessor::new();
    assert!(processor.dechunkify(&frame(7, FIRST, b"a")).unwrap().is_none());
    assert!(processor.dechunkify(&frame(7, 0, b"bc")).unwrap().is_none());
    assert!(processor.dechunkify(&frame(7, 0, b"de")).unwrap().is_none());
    assert_eq!(processor.chunked_pdu, b"abcde");
    assert_eq!(processor.expected_len, Some(7));
    assert_eq!(
        processor.dechunkify(&frame(7, LAST, b"fg")).unwrap(),
        Some(b"abcdefg".to_vec())
    );
    assert_eq!(processor.expected_len, None);
    assert_eq!(processor.chunked_pdu.capacity(), 0);
}
