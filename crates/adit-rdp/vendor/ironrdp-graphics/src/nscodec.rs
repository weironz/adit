//! NSCodec (MS-RDPNSC) decoder.
//!
//! ADIT PATCH: this module does not exist upstream at all.
//!
//! IronRDP has no NSCodec anywhere — only capability-set constants naming it.
//! That was invisible until ClearCodec was wired up, because ClearCodec can
//! carry a whole region as an NSCodec subcodec and the handler for that
//! subcodec id was a silent no-op:
//!
//! ```text
//! SubcodecId::NsCodec => {
//!     // Not yet implemented; encoder avoids generating NSCodec tiles.
//! }
//! ```
//!
//! IronRDP's own ClearCodec encoder never emits one, so nothing upstream
//! noticed. Windows emits them constantly, for photographic and gradient
//! regions. In a captured session 101 of 512 ClearCodec streams carried
//! nothing but an NSCodec subcodec: they "decoded" with no error and painted
//! zero pixels, which is what left the desktop a mosaic of stale rectangles.
//!
//! Ported from FreeRDP `libfreerdp/codec/nsc.c` (`nsc_stream_initialize`,
//! `nsc_rle_decode`, `nsc_rle_decompress_data`, `nsc_decode`), which is the
//! reference implementation Windows is known to interoperate with.

use ironrdp_core::{DecodeResult, ReadCursor, ensure_size, invalid_field_err};

/// Round `value` up to the next multiple of `to`.
fn round_up_to(value: usize, to: usize) -> usize {
    value.div_ceil(to) * to
}

/// Decode an NSCodec bitmap stream into BGRA pixels, `width * height * 4` bytes.
///
/// The alpha byte carries the stream's own alpha plane, which is `0xFF`
/// everywhere for the opaque content Windows sends.
///
/// # Errors
/// Returns a decode error if the header is malformed, a plane is truncated, or
/// a run length would overflow its plane.
pub fn decode(data: &[u8], width: u16, height: u16) -> DecodeResult<Vec<u8>> {
    let (w, h) = (usize::from(width), usize::from(height));
    if w == 0 || h == 0 {
        return Ok(Vec::new());
    }

    let mut src = ReadCursor::new(data);
    ensure_size!(ctx: "NSCodecHeader", in: src, size: 20);

    // NSCODEC_BITMAP_STREAM: four plane byte counts, then the two levels.
    let mut plane_byte_count = [0usize; 4];
    let mut total = 0usize;
    for count in &mut plane_byte_count {
        *count = src.read_u32() as usize;
        total += *count;
    }
    let color_loss_level = src.read_u8();
    if !(1..=7).contains(&color_loss_level) {
        return Err(invalid_field_err!("ColorLossLevel", "must be 1..=7"));
    }
    let chroma_subsampling = src.read_u8() != 0;
    let _reserved = src.read_u16();

    ensure_size!(ctx: "NSCodecPlanes", in: src, size: total);
    let planes_data = src.read_slice(total);

    // Plane geometry. Luma is padded to a multiple of 8 columns; with chroma
    // subsampling the chroma planes are a quarter of that, rows rounded to
    // even. `plane_len` is the allocation every plane shares, as in FreeRDP.
    let temp_width = round_up_to(w, 8);
    let temp_height = round_up_to(h, 2);
    let plane_len = temp_width * temp_height;

    let mut original = [w * h; 4];
    if chroma_subsampling {
        original[0] = temp_width * h;
        original[1] = (temp_width >> 1) * (temp_height >> 1);
        original[2] = original[1];
    }

    // Decompress each plane. A zero byte count means "solid 0xFF", a count
    // below the original size means RLE, anything else is stored raw.
    let mut planes: [Vec<u8>; 4] = [
        vec![0u8; plane_len],
        vec![0u8; plane_len],
        vec![0u8; plane_len],
        vec![0u8; plane_len],
    ];
    let mut offset = 0usize;
    for (index, plane) in planes.iter_mut().enumerate() {
        let plane_size = plane_byte_count[index];
        let original_size = original[index];
        if original_size > plane_len {
            return Err(invalid_field_err!("NSCodec", "plane larger than its buffer"));
        }
        let Some(rle) = planes_data.get(offset..offset + plane_size) else {
            return Err(invalid_field_err!("NSCodec", "plane data truncated"));
        };

        if plane_size == 0 {
            plane[..original_size].fill(0xFF);
        } else if plane_size < original_size {
            rle_decode(rle, plane.as_mut_slice(), original_size)?;
        } else {
            let Some(raw) = rle.get(..original_size) else {
                return Err(invalid_field_err!("NSCodec", "raw plane truncated"));
            };
            plane[..original_size].copy_from_slice(raw);
        }
        offset += plane_size;
    }

    // YCoCg -> RGB. The shift recovers the bits the encoder dropped as
    // "colour loss" and doubles as the YCoCg scale.
    let shift = color_loss_level - 1;
    let mut out = vec![0u8; w * h * 4];
    let mut pos = 0usize;
    for y in 0..h {
        let (luma_row, chroma_row) = if chroma_subsampling {
            (y * temp_width, (y >> 1) * (temp_width >> 1))
        } else {
            (y * w, y * w)
        };
        let alpha_row = y * w;

        // With subsampling the chroma index advances every other column; the
        // `x % 2` in FreeRDP's loop is that, expressed as a running index.
        let mut chroma = chroma_row;
        for x in 0..w {
            let luma = i16::from(planes[0][luma_row + x]);
            // The chroma planes are signed once shifted; the cast back through
            // i8 is what preserves the sign — a plain shift would not.
            let co = i16::from(((i16::from(planes[1][chroma]) << shift) & 0xFF) as u8 as i8);
            let cg = i16::from(((i16::from(planes[2][chroma]) << shift) & 0xFF) as u8 as i8);

            let r = luma + co - cg;
            let g = luma + cg;
            let b = luma - co - cg;

            out[pos] = b.clamp(0, 0xFF) as u8;
            out[pos + 1] = g.clamp(0, 0xFF) as u8;
            out[pos + 2] = r.clamp(0, 0xFF) as u8;
            out[pos + 3] = planes[3][alpha_row + x];
            pos += 4;

            chroma += if chroma_subsampling { x % 2 } else { 1 };
        }
    }

    Ok(out)
}

/// NSCodec's plane RLE: a byte repeated twice introduces a run length, and the
/// final four bytes of every plane are stored verbatim.
fn rle_decode(mut input: &[u8], out: &mut [u8], original_size: usize) -> DecodeResult<()> {
    let mut left = original_size;
    let mut written = 0usize;

    while left > 4 {
        let Some((&value, rest)) = input.split_first() else {
            return Err(invalid_field_err!("NSCodec", "RLE input exhausted"));
        };
        input = rest;

        if left == 5 {
            // The last byte before the verbatim tail is never a run marker.
            *out.get_mut(written)
                .ok_or_else(|| invalid_field_err!("NSCodec", "RLE output overflow"))? = value;
            written += 1;
            left -= 1;
            continue;
        }

        let Some(&next) = input.first() else {
            return Err(invalid_field_err!("NSCodec", "RLE input exhausted"));
        };
        if value != next {
            *out.get_mut(written)
                .ok_or_else(|| invalid_field_err!("NSCodec", "RLE output overflow"))? = value;
            written += 1;
            left -= 1;
            continue;
        }

        // A repeated byte: the length follows, as one byte, or as 0xFF plus a
        // little-endian u32.
        input = &input[1..];
        let Some(&marker) = input.first() else {
            return Err(invalid_field_err!("NSCodec", "RLE input exhausted"));
        };
        let len = if marker < 0xFF {
            input = &input[1..];
            usize::from(marker) + 2
        } else {
            if input.len() < 5 {
                return Err(invalid_field_err!("NSCodec", "RLE long length truncated"));
            }
            let len = u32::from_le_bytes([input[1], input[2], input[3], input[4]]) as usize;
            input = &input[5..];
            len
        };

        if len > left || written + len > out.len() {
            return Err(invalid_field_err!("NSCodec", "RLE run exceeds plane"));
        }
        out[written..written + len].fill(value);
        written += len;
        left -= len;
    }

    // The trailing four bytes are always literal.
    if input.len() < 4 || written + 4 > out.len() {
        return Err(invalid_field_err!("NSCodec", "RLE tail truncated"));
    }
    out[written..written + 4].copy_from_slice(&input[..4]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of a repeated byte expands, and the last four bytes stay literal.
    #[test]
    fn rle_expands_runs_and_keeps_the_tail() {
        // 0x41 repeated, then length byte 3 -> 3 + 2 = 5, then the tail.
        let input = [0x41, 0x41, 0x03, 0x01, 0x02, 0x03, 0x04];
        let mut out = vec![0u8; 16];
        rle_decode(&input, &mut out, 9).expect("decode");
        assert_eq!(&out[..5], &[0x41; 5]);
        assert_eq!(&out[5..9], &[0x01, 0x02, 0x03, 0x04]);
    }

    /// Non-repeating bytes are copied one at a time.
    #[test]
    fn rle_copies_literals() {
        let input = [0x10, 0x20, 0x30, 0x01, 0x02, 0x03, 0x04];
        let mut out = vec![0u8; 16];
        rle_decode(&input, &mut out, 7).expect("decode");
        assert_eq!(&out[..3], &[0x10, 0x20, 0x30]);
        assert_eq!(&out[3..7], &[0x01, 0x02, 0x03, 0x04]);
    }

    /// A header whose planes are absent is rejected rather than silently
    /// producing an empty tile — the failure mode this module exists to remove.
    #[test]
    fn truncated_planes_are_an_error() {
        let mut header = Vec::new();
        header.extend_from_slice(&64u32.to_le_bytes()); // luma
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.push(1); // ColorLossLevel
        header.push(0); // ChromaSubsamplingLevel
        header.extend_from_slice(&0u16.to_le_bytes());
        assert!(decode(&header, 8, 8).is_err());
    }

    /// All-zero plane counts mean every plane is solid 0xFF, and the tile
    /// decodes fully opaque rather than empty.
    #[test]
    fn zero_length_planes_fill_solid() {
        let mut header = Vec::new();
        for _ in 0..4 {
            header.extend_from_slice(&0u32.to_le_bytes());
        }
        header.push(1);
        header.push(0);
        header.extend_from_slice(&0u16.to_le_bytes());
        let out = decode(&header, 4, 4).expect("decode");
        assert_eq!(out.len(), 4 * 4 * 4);
        assert!(out.chunks_exact(4).all(|px| px[3] == 0xFF));
    }
}
