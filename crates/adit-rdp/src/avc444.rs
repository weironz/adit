//! AVC444 (MS-RDPEGFX YUV444 mode) decoding.
//!
//! ironrdp-egfx 0.3 decodes only AVC420 and forwards AVC444 PDUs to
//! `on_unhandled_pdu`, so this module exists — without it, advertising V10+
//! capabilities turns the desktop into undecoded rectangles, which is also
//! why Windows' `AVC444ModePreferred` policy refused our AVC420-only
//! advertisement outright.
//!
//! An AVC444 frame is one or two ordinary H.264 (YUV 4:2:0) streams: the
//! **main** view carries luma plus the even chroma samples, the **auxiliary**
//! view smuggles the remaining chroma samples inside its own Y/U/V planes.
//! The two views are independent H.264 sequences with their own reference
//! chains, hence two decoder instances. Recombination happens in a persistent
//! per-surface YUV 4:4:4 buffer (an LC=LUMA frame updates only what its view
//! carries — the rest must survive), and only then is the touched region
//! converted to RGBA.
//!
//! The recombination kernels and the YUV→RGB coefficients are ported from
//! FreeRDP `libfreerdp/primitives/prim_YUV.c` (`general_LumaToYUV444`,
//! `general_ChromaV1ToYUV444`, `general_ChromaV2ToYUV444`, `YUV2R/G/B`) —
//! the reference implementation Windows is known to interoperate with, and
//! the packing (MS-RDPEGFX 3.3.8.3) is far easier to get wrong than to port.

use ironrdp_egfx::pdu::{Avc420BitmapStream, Avc444BitmapStream, Codec1Type};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

/// One recombined-and-converted output region, in surface coordinates.
pub(crate) struct Avc444Region {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
}

/// Persistent YUV 4:4:4 planes for one surface.
struct Yuv444 {
    w: usize,
    h: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl Yuv444 {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            y: vec![0u8; w * h],
            u: vec![0u8; w * h],
            v: vec![0u8; w * h],
        }
    }
}

pub(crate) struct Avc444State {
    /// Decoder for the main (luma) view.
    main: Decoder,
    /// Decoder for the auxiliary (chroma) view.
    aux: Decoder,
    /// Scratch for the AVC-format → Annex-B rewrite.
    annex_b: Vec<u8>,
    /// Per-surface recombination state, keyed by surface id.
    surfaces: std::collections::HashMap<u16, Yuv444>,
}

impl Avc444State {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            main: Decoder::new().map_err(|e| e.to_string())?,
            aux: Decoder::new().map_err(|e| e.to_string())?,
            annex_b: Vec::new(),
            surfaces: std::collections::HashMap::new(),
        })
    }

    /// Drop a surface's recombination state (surface deleted).
    pub fn delete_surface(&mut self, surface_id: u16) {
        self.surfaces.remove(&surface_id);
    }

    /// Reset everything (graphics reset): fresh decoders, no surface state.
    pub fn reset(&mut self) {
        if let Ok(main) = Decoder::new() {
            self.main = main;
        }
        if let Ok(aux) = Decoder::new() {
            self.aux = aux;
        }
        self.surfaces.clear();
    }

    /// Decode one AVC444 PDU into RGBA regions ready to blit.
    ///
    /// `codec` distinguishes the two auxiliary packings: `Avc444` is v1,
    /// `Avc444v2` is v2 (sent by servers once V10.4+ capabilities are
    /// confirmed).
    pub fn decode(
        &mut self,
        surface_id: u16,
        surface_w: u16,
        surface_h: u16,
        codec: Codec1Type,
        stream: &Avc444BitmapStream<'_>,
    ) -> Result<Vec<Avc444Region>, String> {
        let (sw, sh) = (usize::from(surface_w), usize::from(surface_h));
        let yuv = self
            .surfaces
            .entry(surface_id)
            .or_insert_with(|| Yuv444::new(sw, sh));
        if yuv.w != sw || yuv.h != sh {
            *yuv = Yuv444::new(sw, sh);
        }

        // LC semantics (MS-RDPEGFX 2.2.4.5): 0 = stream1 is the luma view and
        // stream2 the chroma view; 1 = stream1 is luma only; 2 = stream1 is
        // the chroma view only.
        let lc = stream.encoding.bits();
        let mut touched: Vec<(usize, usize, usize, usize)> = Vec::new();

        match lc {
            0 => {
                Self::apply(&mut self.main, &mut self.annex_b, yuv, &stream.stream1, Pass::Luma, &mut touched)?;
                let aux = stream
                    .stream2
                    .as_ref()
                    .ok_or_else(|| "LC=0 without an auxiliary stream".to_owned())?;
                let pass = if codec == Codec1Type::Avc444v2 { Pass::ChromaV2 } else { Pass::ChromaV1 };
                Self::apply(&mut self.aux, &mut self.annex_b, yuv, aux, pass, &mut touched)?;
            }
            1 => {
                Self::apply(&mut self.main, &mut self.annex_b, yuv, &stream.stream1, Pass::Luma, &mut touched)?;
            }
            2 => {
                let pass = if codec == Codec1Type::Avc444v2 { Pass::ChromaV2 } else { Pass::ChromaV1 };
                Self::apply(&mut self.aux, &mut self.annex_b, yuv, &stream.stream1, pass, &mut touched)?;
            }
            other => return Err(format!("reserved LC encoding {other}")),
        }

        // Convert every touched rect from the recombined 4:4:4 planes.
        let mut out = Vec::with_capacity(touched.len());
        for (x, y, w, h) in touched {
            let mut rgba = vec![0u8; w * h * 4];
            for row in 0..h {
                let src = (y + row) * yuv.w + x;
                let dst_row = row * w;
                for col in 0..w {
                    let (yy, uu, vv) = (
                        i32::from(yuv.y[src + col]),
                        i32::from(yuv.u[src + col]) - 128,
                        i32::from(yuv.v[src + col]) - 128,
                    );
                    // FreeRDP YUV2R/G/B: BT.709 full-range, fixed point /256.
                    let r = (256 * yy + 403 * vv) >> 8;
                    let g = (256 * yy - 48 * uu - 120 * vv) >> 8;
                    let b = (256 * yy + 475 * uu) >> 8;
                    let px = (dst_row + col) * 4;
                    rgba[px] = r.clamp(0, 255) as u8;
                    rgba[px + 1] = g.clamp(0, 255) as u8;
                    rgba[px + 2] = b.clamp(0, 255) as u8;
                    rgba[px + 3] = 0xFF;
                }
            }
            out.push(Avc444Region { x, y, w, h, rgba });
        }
        Ok(out)
    }

    /// Decode one view and fold it into the 4:4:4 planes over its rects.
    fn apply(
        decoder: &mut Decoder,
        annex_b: &mut Vec<u8>,
        yuv: &mut Yuv444,
        view: &Avc420BitmapStream<'_>,
        pass: Pass,
        touched: &mut Vec<(usize, usize, usize, usize)>,
    ) -> Result<(), String> {
        // ironrdp-egfx's docs claim the wire carries AVC-format NALs (4-byte
        // big-endian length prefixes); a live Windows capture refutes that —
        // the data opens with an Annex-B start code, and reading it as a
        // length made every frame die with "NAL length exceeds stream"
        // (FreeRDP likewise hands the buffer to its decoders untouched).
        // Accept both: pass Annex-B through, convert prefixes if they ever
        // really appear.
        annex_b.clear();
        let mut data = view.data;
        if data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]) {
            annex_b.extend_from_slice(data);
        } else {
            while data.len() >= 4 {
                let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
                data = &data[4..];
                if len > data.len() {
                    return Err("NAL length exceeds stream".to_owned());
                }
                annex_b.extend_from_slice(&[0, 0, 0, 1]);
                annex_b.extend_from_slice(&data[..len]);
                data = &data[len..];
            }
        }

        let Some(frame) = decoder.decode(annex_b).map_err(|e| e.to_string())? else {
            // The decoder buffered the input (no output frame yet). Nothing to
            // paint for this view.
            return Ok(());
        };
        let (fw, fh) = frame.dimensions();
        let (sy, su, sv) = frame.strides();
        let planes = [frame.y(), frame.u(), frame.v()];
        let steps = [sy, su, sv];

        for rect in &view.rectangles {
            // MS-RDPEGFX RDPGFX_RECT16: right/bottom are EXCLUSIVE, whatever
            // the upstream type's name suggests.
            let left = usize::from(rect.left).min(yuv.w);
            let top = usize::from(rect.top).min(yuv.h);
            let right = usize::from(rect.right).min(yuv.w);
            let bottom = usize::from(rect.bottom).min(yuv.h);
            if left >= right || top >= bottom {
                continue;
            }
            match pass {
                Pass::Luma => luma_to_yuv444(&planes, &steps, yuv, left, top, right, bottom),
                Pass::ChromaV1 => chroma_v1_to_yuv444(&planes, &steps, fh, yuv, left, top, right, bottom),
                Pass::ChromaV2 => chroma_v2_to_yuv444(&planes, &steps, fw, yuv, left, top, right, bottom),
            }
            touched.push((left, top, right - left, bottom - top));
        }
        Ok(())
    }
}

enum Pass {
    Luma,
    ChromaV1,
    ChromaV2,
}

/// FreeRDP `general_LumaToYUV444`: copy Y, upsample the view's U/V (the even
/// chroma samples) into every position of the 4:4:4 planes; the odd samples
/// get overwritten by the chroma pass when one arrives.
fn luma_to_yuv444(
    src: &[&[u8]; 3],
    step: &[usize; 3],
    yuv: &mut Yuv444,
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
) {
    let (w, h) = (right - left, bottom - top);
    let half_w = w.div_ceil(2);
    let half_h = h.div_ceil(2);

    for row in 0..h {
        let s = (top + row) * step[0] + left;
        let d = (top + row) * yuv.w + left;
        yuv.y[d..d + w].copy_from_slice(&src[0][s..s + w]);
    }
    for row in 0..half_h {
        let su = (top / 2 + row) * step[1] + left / 2;
        let sv = (top / 2 + row) * step[2] + left / 2;
        let y2 = top + 2 * row;
        let y21 = y2 + 1;
        for col in 0..half_w {
            let u = src[1][su + col];
            let v = src[2][sv + col];
            let x2 = left + 2 * col;
            for (yy, xx) in [(y2, x2), (y2, x2 + 1), (y21, x2), (y21, x2 + 1)] {
                if yy < yuv.h && xx < yuv.w {
                    yuv.u[yy * yuv.w + xx] = u;
                    yuv.v[yy * yuv.w + xx] = v;
                }
            }
        }
    }
}

/// FreeRDP `general_ChromaV1ToYUV444`: the aux Y plane carries the odd U/V
/// rows in 16-row interleave (first 8 U, next 8 V), the aux U/V planes carry
/// the odd-column samples of the even rows.
#[allow(clippy::too_many_arguments)] // mirrors the FreeRDP kernel it ports
fn chroma_v1_to_yuv444(
    src: &[&[u8]; 3],
    step: &[usize; 3],
    frame_h: usize,
    yuv: &mut Yuv444,
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
) {
    const MOD: usize = 16;
    let (w, h) = (right - left, bottom - top);
    let half_w = w / 2;
    let half_h = h / 2;
    let pad_h = h + 16 - h % 16;

    let (mut u_y, mut v_y) = (0usize, 0usize);
    for row in 0..pad_h {
        if top + row >= frame_h {
            break;
        }
        let s = (top + row) * step[0] + left;
        let is_u = row % MOD < MOD.div_ceil(2);
        let pos = if is_u {
            let pos = 2 * u_y + 1;
            u_y += 1;
            pos
        } else {
            let pos = 2 * v_y + 1;
            v_y += 1;
            pos
        };
        if pos >= h {
            continue;
        }
        let d = (top + pos) * yuv.w + left;
        let plane = if is_u { &mut yuv.u } else { &mut yuv.v };
        plane[d..d + w].copy_from_slice(&src[0][s..s + w]);
    }

    for row in 0..half_h {
        let su = (top / 2 + row) * step[1] + left / 2;
        let sv = (top / 2 + row) * step[2] + left / 2;
        let d = (top + 2 * row) * yuv.w + left;
        for col in 0..half_w {
            let x1 = 2 * col + 1;
            if left + x1 < yuv.w {
                yuv.u[d + x1] = src[1][su + col];
                yuv.v[d + x1] = src[2][sv + col];
            }
        }
    }
}

/// FreeRDP `general_ChromaV2ToYUV444`: the aux Y plane is split left/right
/// into odd-column U and V at half width; the aux U/V planes each split
/// left/right again, carrying the odd-row samples four columns apart.
#[allow(clippy::too_many_arguments)] // mirrors the FreeRDP kernel it ports
fn chroma_v2_to_yuv444(
    src: &[&[u8]; 3],
    step: &[usize; 3],
    frame_w: usize,
    yuv: &mut Yuv444,
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
) {
    let (w, h) = (right - left, bottom - top);
    let half_w = w.div_ceil(2);
    let half_h = h.div_ceil(2);
    let quarter_w = w.div_ceil(4);

    for row in 0..h {
        let su = (top + row) * step[0] + left / 2;
        let sv = su + frame_w / 2;
        let d = (top + row) * yuv.w + left;
        for col in 0..half_w {
            let odd = 2 * col + 1;
            if left + odd < yuv.w {
                yuv.u[d + odd] = src[0][su + col];
                yuv.v[d + odd] = src[0][sv + col];
            }
        }
    }

    for row in 0..half_h {
        let su_u = (top / 2 + row) * step[1] + left / 4;
        let su_v = su_u + frame_w / 4;
        let sv_u = (top / 2 + row) * step[2] + left / 4;
        let sv_v = sv_u + frame_w / 4;
        let dy = top + 2 * row + 1;
        if dy >= yuv.h {
            break;
        }
        let d = dy * yuv.w + left;
        for col in 0..quarter_w {
            let x0 = 4 * col;
            let x2 = 4 * col + 2;
            if left + x0 < yuv.w {
                yuv.u[d + x0] = src[1][su_u + col];
                yuv.v[d + x0] = src[1][su_v + col];
            }
            if left + x2 < yuv.w {
                yuv.u[d + x2] = src[2][sv_u + col];
                yuv.v[d + x2] = src[2][sv_v + col];
            }
        }
    }
}
