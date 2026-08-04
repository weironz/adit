//! SRL (Simplified Run-Length) entropy codec for progressive upgrade passes.
//!
//! Used during progressive TILE_UPGRADE decoding where the tri-state sign
//! array (DAS) indicates zero-valued coefficients. SRL encodes/decodes
//! magnitudes for coefficients that were previously zero.
//!
//! The algorithm is similar to RLGR's zero-run mode with a simpler structure:
//! adaptive K parameter controlling zero-run lengths, followed by unary-coded
//! magnitudes with sign bits.

/// Streaming SRL decoder whose adaptation state survives across reads.
///
/// ADIT PATCH: this struct replaces a per-call `decode_srl` as the decode-side
/// entry point. The SRL stream inside a TILE_UPGRADE is ONE continuous
/// bitstream per component, spanning all ten sub-bands — the adaptive `kp`,
/// the pending zero-run, and the bit cursor all carry across band boundaries
/// (FreeRDP holds them in `RFX_PROGRESSIVE_UPGRADE_STATE` for the lifetime of
/// the component). Restarting the stream per band — what the old call shape
/// did — reads the first band correctly and then re-reads the stream head as
/// garbage for every band after it.
pub struct SrlDecoder<'a> {
    reader: BitReader<'a>,
    kp: u32,
    /// Zeros still owed from the current zero-run.
    nz: u32,
}

impl<'a> SrlDecoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            reader: BitReader::new(data),
            kp: 0,
            nz: 0,
        }
    }

    /// Decode the next value. `num_bits` is this band's magnitude width and
    /// may differ from the previous read's — the adaptation state persists
    /// regardless.
    pub fn read(&mut self, num_bits: u8) -> i16 {
        let k = self.kp >> 3;

        if self.nz > 0 {
            // Still emitting zeros from a previous run
            self.nz -= 1;
            return 0;
        }

        // Zero-run mode: chunk_size = 1 << k (1 when k=0).
        let bit = self.reader.read_bit();
        if !bit {
            self.nz = 1u32.checked_shl(k).unwrap_or(0);
            self.kp = self.kp.saturating_add(4).min(80);
            self.nz -= 1;
            return 0;
        }
        let zeros = self.reader.read_bits(k);
        if zeros > 0 {
            self.nz = zeros;
            self.nz -= 1;
            return 0;
        }

        // Unary mode: decode a non-zero magnitude
        self.kp = self.kp.saturating_sub(6);

        if num_bits == 0 {
            // No bits to decode, just emit +/-1 from sign bit
            let sign = self.reader.read_bit();
            return if sign { -1 } else { 1 };
        }

        // Read sign bit
        let sign = self.reader.read_bit();

        if num_bits == 1 {
            return if sign { -1 } else { 1 };
        }

        // Decode unary quotient: count 0-bits before the terminating 1-bit.
        // magnitude = (quotient << extra_bits) | remainder.
        let mut quotient: u32 = 0;
        loop {
            let bit = self.reader.read_bit();
            if bit || quotient >= 0x8000 {
                break;
            }
            quotient += 1;
        }

        let extra_bits = u32::from(num_bits).saturating_sub(1);
        let magnitude = if extra_bits > 0 && extra_bits < 16 {
            let remainder = self.reader.read_bits(extra_bits);
            (quotient << extra_bits) | remainder
        } else {
            quotient
        };

        let value = i16::try_from(magnitude.min(0x7FFF)).unwrap_or(i16::MAX);
        if sign { -value } else { value }
    }
}

/// Decode SRL data for a set of zero-valued (DAS=0) coefficient positions.
///
/// One-shot wrapper over [`SrlDecoder`], kept for the encoder round-trip
/// tests. The decode path proper must NOT use this per band — see the
/// [`SrlDecoder`] docs for why.
pub fn decode_srl(data: &[u8], num_values: usize, num_bits: u8) -> Vec<i16> {
    let mut decoder = SrlDecoder::new(data);
    (0..num_values).map(|_| decoder.read(num_bits)).collect()
}

/// Encode coefficient magnitudes using the SRL algorithm.
///
/// `values` contains signed coefficient values (non-zero = needs encoding,
/// zero = contributes to zero runs).
/// `num_bits` is the bit width for magnitude encoding.
///
/// Returns the encoded SRL byte stream (with trailing 0x00 sentinel).
pub fn encode_srl(values: &[i16], num_bits: u8) -> Vec<u8> {
    if values.is_empty() {
        return vec![0x00];
    }

    let mut writer = BitWriter::new();
    let mut kp: u32 = 0;
    let mut idx = 0;

    while idx < values.len() {
        // Count leading zeros (may be 0)
        let mut zero_count: u32 = 0;
        while idx + usize::try_from(zero_count).unwrap_or(usize::MAX) < values.len()
            && values[idx + usize::try_from(zero_count).unwrap_or(usize::MAX)] == 0
        {
            zero_count += 1;
        }

        // Encode zero run one chunk at a time, recomputing k after
        // each kp update to stay in sync with the decoder.
        while zero_count > 0 {
            let cur_k = kp >> 3;
            let chunk_size = 1u32.checked_shl(cur_k).unwrap_or(u32::MAX);
            if zero_count >= chunk_size {
                writer.write_bit(false);
                kp = kp.saturating_add(4).min(80);
                zero_count -= chunk_size;
                idx += usize::try_from(chunk_size).unwrap_or(usize::MAX);
            } else {
                // Remaining zeros < chunk: escape bit + count
                writer.write_bit(true);
                writer.write_bits(zero_count, cur_k);
                idx += usize::try_from(zero_count).unwrap_or(usize::MAX);
                zero_count = 0;
                continue;
            }
        }
        // No remaining zeros: write escape with zero count
        let cur_k = kp >> 3;
        writer.write_bit(true);
        writer.write_bits(0, cur_k);

        if idx >= values.len() {
            break;
        }

        // Encode non-zero value
        kp = kp.saturating_sub(6);
        let value = values[idx];
        let sign = value < 0;
        let magnitude = u32::from(value.unsigned_abs());

        writer.write_bit(sign);

        if num_bits <= 1 {
            idx += 1;
            continue;
        }

        // Unary encode: quotient zeros + terminator + remainder bits.
        // magnitude = (quotient << extra_bits) | remainder.
        let extra_bits = u32::from(num_bits).saturating_sub(1);
        if extra_bits > 0 && extra_bits < 16 {
            let quotient = magnitude >> extra_bits;
            let remainder = magnitude & ((1u32 << extra_bits) - 1);

            for _ in 0..quotient {
                writer.write_bit(false);
            }
            writer.write_bit(true);
            writer.write_bits(remainder, extra_bits);
        }

        idx += 1;
    }

    // Trailing sentinel
    let mut result = writer.finish();
    result.push(0x00);
    result
}

// ---------------------------------------------------------------------------
// Bit-level I/O helpers
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    byte_idx: usize,
    bit_idx: u8, // 0..7, MSB first
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_idx: 0,
            bit_idx: 0,
        }
    }

    fn read_bit(&mut self) -> bool {
        if self.byte_idx >= self.data.len() {
            return false;
        }
        let bit = (self.data[self.byte_idx] >> (7 - self.bit_idx)) & 1 != 0;
        self.bit_idx += 1;
        if self.bit_idx >= 8 {
            self.bit_idx = 0;
            self.byte_idx += 1;
        }
        bit
    }

    fn read_bits(&mut self, count: u32) -> u32 {
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.read_bit());
        }
        value
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    bit_count: u8, // bits written in current byte (0..7)
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            current: 0,
            bit_count: 0,
        }
    }

    fn write_bit(&mut self, bit: bool) {
        self.current = (self.current << 1) | u8::from(bit);
        self.bit_count += 1;
        if self.bit_count >= 8 {
            self.bytes.push(self.current);
            self.current = 0;
            self.bit_count = 0;
        }
    }

    fn write_bits(&mut self, value: u32, count: u32) {
        for i in (0..count).rev() {
            self.write_bit((value >> i) & 1 != 0);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bit_count > 0 {
            // Pad remaining bits with zeros (MSB aligned)
            self.current <<= 8 - self.bit_count;
            self.bytes.push(self.current);
        }
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_empty() {
        let result = decode_srl(&[], 0, 1);
        assert!(result.is_empty());
    }

    #[test]
    fn decode_empty_data() {
        // With no data (empty slice), all positions default to zero
        let result = decode_srl(&[], 5, 1);
        assert_eq!(result, vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn encode_empty() {
        let encoded = encode_srl(&[], 1);
        assert_eq!(encoded, vec![0x00]); // just sentinel
    }

    #[test]
    fn encode_all_zeros() {
        let encoded = encode_srl(&[0, 0, 0], 1);
        // Sentinel must be present
        assert_eq!(*encoded.last().unwrap(), 0x00);
        // Round-trip: all zeros must survive
        let decoded = decode_srl(&encoded, 3, 1);
        assert_eq!(decoded, vec![0, 0, 0]);
    }

    #[test]
    fn round_trip_single_positive() {
        let original = vec![1];
        let encoded = encode_srl(&original, 1);
        let decoded = decode_srl(&encoded, 1, 1);
        assert_eq!(decoded, original);
    }

    #[test]
    fn round_trip_single_negative() {
        let original = vec![-1];
        let encoded = encode_srl(&original, 1);
        let decoded = decode_srl(&encoded, 1, 1);
        assert_eq!(decoded, original);
    }

    #[test]
    fn round_trip_mixed_zeros() {
        // Zeros at the start (where k=0) must survive the round-trip
        let original = vec![0, 0, 1, -1, 0, 3];
        let encoded = encode_srl(&original, 4);
        let decoded = decode_srl(&encoded, original.len(), 4);
        assert_eq!(decoded, original);
    }

    #[test]
    fn round_trip_nonzero_only() {
        let original = vec![1, -1, 2, -3, 1];
        let encoded = encode_srl(&original, 4);
        let decoded = decode_srl(&encoded, original.len(), 4);
        assert_eq!(decoded, original);
    }

    #[test]
    fn bit_reader_basic() {
        let data = [0b10110000];
        let mut reader = BitReader::new(&data);
        assert!(reader.read_bit()); // 1
        assert!(!reader.read_bit()); // 0
        assert!(reader.read_bit()); // 1
        assert!(reader.read_bit()); // 1
    }

    #[test]
    fn bit_writer_basic() {
        let mut writer = BitWriter::new();
        writer.write_bit(true);
        writer.write_bit(false);
        writer.write_bit(true);
        writer.write_bit(true);
        writer.write_bit(false);
        writer.write_bit(false);
        writer.write_bit(false);
        writer.write_bit(false);
        let result = writer.finish();
        assert_eq!(result, vec![0b10110000]);
    }

    #[test]
    fn bit_writer_multi_byte() {
        let mut writer = BitWriter::new();
        writer.write_bits(0xFF, 8);
        writer.write_bits(0x00, 8);
        let result = writer.finish();
        assert_eq!(result, vec![0xFF, 0x00]);
    }
}
