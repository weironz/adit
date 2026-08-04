//! EGFX (RDPGFX graphics pipeline, MS-RDPEGFX) client handler.
//!
//! GNOME Remote Desktop — and modern Windows — serve graphics only over the EGFX
//! pipeline, not the legacy bitmap path; a client that doesn't advertise it is
//! rejected at capabilities exchange. We attach a [`GraphicsPipelineClient`] to
//! the dynamic virtual channels and composite the decoded RGBA surface updates it
//! hands us into a shared framebuffer the session loop emits as tiles.
//!
//! No H.264 decoder is configured, so IronRDP advertises the V8 (no-AVC)
//! capability set and the server falls back to **RemoteFX Progressive**
//! (`WireToSurface2`, which IronRDP delivers via [`on_wire_to_surface2`] but does
//! NOT decode itself). We decode it with [`ironrdp_graphics::progressive`] and
//! composite the resulting 64×64 RGBA tiles.
//!
//! [`GraphicsPipelineClient`]: ironrdp_egfx::client::GraphicsPipelineClient
//! [`on_wire_to_surface2`]: GraphicsPipelineHandler::on_wire_to_surface2

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineHandler, Surface};
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, DeleteEncodingContextPdu, SolidFillPdu, SurfaceToCachePdu,
    SurfaceToSurfacePdu, WireToSurface2Pdu,
};
use ironrdp_graphics::clearcodec::ClearCodecDecoder;
use ironrdp_graphics::image_processing::PixelFormat as GfxPixelFormat;
use ironrdp_graphics::progressive::ProgressiveDecoder;
use ironrdp_session::image::DecodedImage;
use ironrdp_session::rfx::DecodingContext as RfxDecodingContext;

/// Match the framebuffer clamp on the app side.
const MAX_DIMENSION: u32 = 8192;

/// How many progressive streams to write out before giving up, when dumping is
/// switched on with `ADIT_RDP_DUMP=1`.
///
/// Successes are captured too, not just failures. A stream that decodes is
/// usually the one carrying the SYNC + CONTEXT blocks that establish the codec
/// context, and without it a captured failure cannot be replayed at all — every
/// later frame references a context that a fresh decoder has never seen. That is
/// exactly what happened the first time this was used: only failures were kept,
/// and the capture turned out to be unreplayable on its own.
///
/// Off unless asked for: a capture is a picture of the user's desktop, so it is
/// opt-in rather than something a bad connection scatters across their disk.
const MAX_DUMPS: u32 = 8;

/// RemoteFX Progressive tile edge, in pixels (MS-RDPRFX): tiles are 64×64.
const TILE: usize = 64;

/// The EGFX output framebuffer, written by the handler (which runs inside
/// `ActiveStage::process`) and sampled by the session loop.
pub(crate) struct EgfxFrame {
    pub rgba: Vec<u8>,
    pub width: u16,
    pub height: u16,
    /// Updates landed since the loop last emitted a frame.
    pub dirty: bool,
    /// Union of the pixel rects touched since the last take (x, y, w, h).
    /// Lets the session loop ship only the changed region instead of cloning
    /// and piping the whole framebuffer (~9 MB per frame at 2560-class sizes)
    /// on every present.
    dirty_rect: Option<(usize, usize, usize, usize)>,
}

impl EgfxFrame {
    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.rgba = vec![0u8; usize::from(width) * usize::from(height) * 4];
        self.dirty_rect = None;
        // Deliberately NOT dirty: this buffer is all black until content
        // lands. Publishing it at allocation time flashed a full black frame
        // on every graphics reset; the first composited EndFrame after the
        // resize is the correct first publish (and carries the new size).
        self.dirty = false;
    }
}

pub(crate) type SharedEgfx = Arc<Mutex<EgfxFrame>>;

pub(crate) fn new_shared() -> SharedEgfx {
    Arc::new(Mutex::new(EgfxFrame {
        rgba: Vec::new(),
        width: 0,
        height: 0,
        dirty: false,
        dirty_rect: None,
    }))
}

/// If a new EGFX frame is ready, return its size + a full-frame RGBA copy and
/// clear the dirty flag. `None` when nothing changed (or EGFX isn't in use).
/// One published frame: the surface size, the changed region's position and
/// size, and that region's pixels (row-major, tightly packed).
pub(crate) struct FrameUpdate {
    pub surface_width: u16,
    pub surface_height: u16,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub rgba: Vec<u8>,
}

pub(crate) fn take_frame(shared: &SharedEgfx) -> Option<FrameUpdate> {
    let mut frame = shared.lock().ok()?;
    if !frame.dirty || frame.rgba.is_empty() {
        return None;
    }
    frame.dirty = false;
    let (fw, fh) = (usize::from(frame.width), usize::from(frame.height));
    // No recorded rect (defensive) -> whole surface.
    let (x, y, w, h) = frame.dirty_rect.take().unwrap_or((0, 0, fw, fh));
    let (x, y) = (x.min(fw), y.min(fh));
    let (w, h) = (w.min(fw - x), h.min(fh - y));
    if w == 0 || h == 0 {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let start = ((y + row) * fw + x) * 4;
        rgba.extend_from_slice(&frame.rgba[start..start + w * 4]);
    }
    Some(FrameUpdate {
        surface_width: frame.width,
        surface_height: frame.height,
        x: x as u16,
        y: y as u16,
        width: w as u16,
        height: h as u16,
        rgba,
    })
}

/// Grow the frame's dirty rect to cover `w x h` pixels at `(x, y)`.
fn grow_dirty(frame: &mut EgfxFrame, x: usize, y: usize, w: usize, h: usize) {
    if w == 0 || h == 0 {
        return;
    }
    frame.dirty_rect = Some(match frame.dirty_rect {
        None => (x, y, w, h),
        Some((dx, dy, dw, dh)) => {
            let x0 = dx.min(x);
            let y0 = dy.min(y);
            let x1 = (dx + dw).max(x + w);
            let y1 = (dy + dh).max(y + h);
            (x0, y0, x1 - x0, y1 - y0)
        }
    });
}

/// One entry of the EGFX bitmap cache.
struct CachedBitmap {
    width: u16,
    height: u16,
    rgba: Vec<u8>,
}

/// A server surface: where it maps onto the output, and its size.
struct SurfaceInfo {
    origin_x: u32,
    origin_y: u32,
    width: u16,
    height: u16,
}

pub(crate) struct EgfxHandler {
    shared: SharedEgfx,
    /// surface_id → mapping/size. A bitmap update targets a surface; the surface
    /// is mapped to a position on the output.
    surfaces: HashMap<u16, SurfaceInfo>,
    /// RemoteFX Progressive decoder, keyed internally by codec-context id. Kept
    /// across frames (progressive frames refine earlier ones).
    progressive: ProgressiveDecoder,
    /// How many failing streams have been written out so far.
    dumps: u32,
    /// The EGFX bitmap cache: slot -> (width, height, RGBA).
    ///
    /// Windows sends a region's pixels ONCE, tells the client to cache it, and
    /// from then on says "blit slot N here" instead of re-encoding. Ignoring
    /// those PDUs left every repeated element — window chrome, glyph runs, the
    /// contents of a text box — frozen at whatever was on screen before, which
    /// is what made a logged-in desktop render as a mosaic of stale fragments.
    cache: HashMap<u16, CachedBitmap>,
    /// ClearCodec decoder (glyph + vBar caches are session-global state).
    ///
    /// The codec exists, fully implemented, in the graphics crate — the EGFX
    /// client just never wires it: WireToSurface1 with ClearCodec lands in
    /// `on_unhandled_pdu`. Windows encodes sharp UI content (text boxes,
    /// glyph runs, small controls) with it, so before this was connected a
    /// logged-in desktop rendered as fragments and the logon password box
    /// never appeared at all.
    clear: ClearCodecDecoder,
    /// Classic RemoteFX (TS_RFX) decoder + scratch image, per surface.
    ///
    /// Same story: `ironrdp_session::rfx::DecodingContext` is a complete
    /// decoder sitting one crate over, used only by the legacy non-EGFX path.
    /// Thin-client-mode Windows ships whole frames as classic RemoteFX inside
    /// WireToSurface1.
    rfx: HashMap<u16, (RfxDecodingContext, DecodedImage)>,
    /// How many ClearCodec streams have been written out.
    clear_dumps: u32,
    /// Counters for the one-off diagnostics above.
    unhandled_logged: u32,
    frames_seen: u32,
    /// How many "no rect covered this tile" warnings have been logged.
    unclipped_logged: u32,
    /// Whether anything was composited since the last present. `EndFrame`
    /// publishes only when this is set: after a graphics reset zeroes the
    /// framebuffer, an empty frame's unconditional present would flash pure
    /// black until the next real content arrived.
    composited: bool,
}

impl EgfxHandler {
    pub(crate) fn new(shared: SharedEgfx) -> Self {
        Self {
            shared,
            surfaces: HashMap::new(),
            progressive: ProgressiveDecoder::new(),
            dumps: 0,
            composited: false,
            clear: ClearCodecDecoder::new(),
            rfx: HashMap::new(),
            clear_dumps: 0,
            unclipped_logged: 0,
            unhandled_logged: 0,
            frames_seen: 0,
            cache: HashMap::new(),
        }
    }

    /// Write a progressive stream next to the helper's log, in arrival order, so
    /// the sequence can be replayed offline. Best-effort in every respect: a
    /// diagnostic must never take down a session that is otherwise working.
    fn dump_stream(&mut self, pdu: &WireToSurface2Pdu, sw: u16, sh: u16, outcome: &str) {
        if self.dumps >= MAX_DUMPS || std::env::var_os("ADIT_RDP_DUMP").is_none() {
            return;
        }
        let Some(base) = std::env::var_os("APPDATA").map(std::path::PathBuf::from) else {
            return;
        };
        let dir = base.join("Adit");
        let index = self.dumps;
        self.dumps += 1;
        // The sidecar carries what the bytes alone cannot: which context they
        // belong to, the surface they were sized against, and which error they
        // produced — all of which the parse has to be read against.
        let meta = format!(
            "outcome={outcome}
codec_context_id={}
surface={sw}x{sh}
bytes={}
",
            pdu.codec_context_id,
            pdu.bitmap_data.len(),
        );
        let stream = dir.join(format!("progressive-{index}.bin"));
        let sidecar = dir.join(format!("progressive-{index}.txt"));
        if std::fs::write(&stream, &pdu.bitmap_data).is_ok()
            && std::fs::write(&sidecar, meta).is_ok()
        {
            tracing::warn!("wrote progressive stream to {}", stream.display());
        }
    }

    /// Composite a 64×64 RGBA tile at output pixel (`px`, `py`), clamped to the
    /// framebuffer (edge tiles overhang a surface whose size isn't a multiple of 64).
    /// Copy `w x h` RGBA pixels into the frame at `(x, y)`, clipped to it.
    fn blit_rect(frame: &mut EgfxFrame, x: usize, y: usize, w: usize, h: usize, rgba: &[u8]) {
        let (fw, fh) = (usize::from(frame.width), usize::from(frame.height));
        if x >= fw || y >= fh || w == 0 || h == 0 {
            return;
        }
        let cols = w.min(fw - x);
        let rows = h.min(fh - y);
        if rgba.len() < (rows - 1) * w * 4 + cols * 4 {
            return;
        }
        for row in 0..rows {
            let dst = ((y + row) * fw + x) * 4;
            let src = row * w * 4;
            frame.rgba[dst..dst + cols * 4].copy_from_slice(&rgba[src..src + cols * 4]);
        }
        grow_dirty(frame, x, y, cols, rows);
    }

    /// Read `w x h` RGBA pixels out of the frame at `(x, y)`, tightly packed.
    fn read_rect(frame: &EgfxFrame, x: usize, y: usize, w: usize, h: usize) -> Option<Vec<u8>> {
        let (fw, fh) = (usize::from(frame.width), usize::from(frame.height));
        if x + w > fw || y + h > fh || w == 0 || h == 0 {
            return None;
        }
        let mut out = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            let start = ((y + row) * fw + x) * 4;
            out.extend_from_slice(&frame.rgba[start..start + w * 4]);
        }
        Some(out)
    }

    /// Write a ClearCodec stream next to the helper's log, in arrival order.
    fn dump_clear_stream(
        &mut self,
        pdu: &ironrdp_egfx::pdu::WireToSurface1Pdu,
        w: u16,
        h: u16,
        outcome: &str,
    ) {
        if self.clear_dumps >= MAX_DUMPS || std::env::var_os("ADIT_RDP_DUMP").is_none() {
            return;
        }
        let Some(base) = std::env::var_os("APPDATA").map(std::path::PathBuf::from) else {
            return;
        };
        let dir = base.join("Adit");
        let index = self.clear_dumps;
        self.clear_dumps += 1;
        let meta = format!(
            "outcome={outcome}
surface_id={}
rect={}x{} at {},{}
bytes={}
",
            pdu.surface_id,
            w,
            h,
            pdu.destination_rectangle.left,
            pdu.destination_rectangle.top,
            pdu.bitmap_data.len(),
        );
        let stream = dir.join(format!("clear-{index:03}.bin"));
        let sidecar = dir.join(format!("clear-{index:03}.txt"));
        if std::fs::write(&stream, &pdu.bitmap_data).is_ok() {
            let _ = std::fs::write(&sidecar, meta);
        }
    }

    /// WireToSurface1 with ClearCodec: decode and composite at the
    /// destination rectangle. The decoder outputs BGRA with alpha forced to
    /// 0xFF; the framebuffer is RGBA, so channels swap during the blit.
    fn decode_clear_codec(&mut self, pdu: &ironrdp_egfx::pdu::WireToSurface1Pdu) {
        let Some((ox, oy)) = self.surfaces.get(&pdu.surface_id).map(|s| (s.origin_x, s.origin_y))
        else {
            return;
        };
        let rect = &pdu.destination_rectangle;
        let (w, h) = (
            rect.right.saturating_sub(rect.left),
            rect.bottom.saturating_sub(rect.top),
        );
        if w == 0 || h == 0 {
            return;
        }
        let outcome = self.clear.decode(&pdu.bitmap_data, w, h);
        // Capture under ADIT_RDP_DUMP, successes included: the glyph and v-bar
        // caches are stateful, so a failing stream can only be replayed in the
        // order it arrived — the lesson the progressive captures already
        // taught, where a failures-only capture turned out unreplayable.
        self.dump_clear_stream(
            pdu,
            w,
            h,
            &match &outcome {
                Ok(_) => "ok".to_owned(),
                Err(error) => error.to_string(),
            },
        );
        let mut bgra = match outcome {
            Ok(bgra) => bgra,
            Err(error) => {
                tracing::warn!("ClearCodec decode failed: {error}");
                return;
            }
        };
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2); // BGRA -> RGBA
        }
        if let Ok(mut frame) = self.shared.lock() {
            Self::blit_rect(
                &mut frame,
                ox as usize + usize::from(rect.left),
                oy as usize + usize::from(rect.top),
                usize::from(w),
                usize::from(h),
                &bgra,
            );
            self.composited = true;
        }
    }

    /// WireToSurface1 with classic RemoteFX (TS_RFX): decode through the
    /// session crate's decoder into a per-surface scratch image, then copy the
    /// updated region across. Thin-client-mode Windows ships entire frames
    /// this way.
    fn decode_classic_rfx(&mut self, pdu: &ironrdp_egfx::pdu::WireToSurface1Pdu) {
        use ironrdp_pdu::geometry::InclusiveRectangle;

        let Some((sw, sh, ox, oy)) = self
            .surfaces
            .get(&pdu.surface_id)
            .map(|s| (s.width, s.height, s.origin_x, s.origin_y))
        else {
            return;
        };
        let (context, image) = self.rfx.entry(pdu.surface_id).or_insert_with(|| {
            (
                RfxDecodingContext::new(),
                DecodedImage::new(GfxPixelFormat::RgbA32, sw, sh),
            )
        });
        if image.width() != sw || image.height() != sh {
            *context = RfxDecodingContext::new();
            *image = DecodedImage::new(GfxPixelFormat::RgbA32, sw, sh);
        }

        let rect = &pdu.destination_rectangle;
        let destination = InclusiveRectangle {
            left: rect.left,
            top: rect.top,
            right: rect.right.saturating_sub(1),
            bottom: rect.bottom.saturating_sub(1),
        };
        let mut cursor = ironrdp_core::ReadCursor::new(&pdu.bitmap_data);
        let updated = match context.decode(image, &destination, &mut cursor) {
            Ok((_frame_id, updated)) => updated,
            Err(error) => {
                tracing::warn!("classic RemoteFX decode failed: {error}");
                return;
            }
        };

        // Copy the updated region out of the scratch image (RgbA32 == the
        // framebuffer's byte order, so this is a straight row copy).
        let (ux, uy) = (usize::from(updated.left), usize::from(updated.top));
        let uw = usize::from(updated.right.saturating_sub(updated.left)) + 1;
        let uh = usize::from(updated.bottom.saturating_sub(updated.top)) + 1;
        let img_w = usize::from(image.width());
        let data = image.data();
        let mut region = Vec::with_capacity(uw * uh * 4);
        for row in 0..uh {
            let start = ((uy + row) * img_w + ux) * 4;
            let Some(slice) = data.get(start..start + uw * 4) else {
                return;
            };
            region.extend_from_slice(slice);
        }
        if let Ok(mut frame) = self.shared.lock() {
            Self::blit_rect(
                &mut frame,
                ox as usize + ux,
                oy as usize + uy,
                uw,
                uh,
                &region,
            );
            self.composited = true;
        }
    }

    /// Blit a window of one 64x64 tile: `w x h` pixels starting at
    /// `(sub_x, sub_y)` inside the tile, landing at `(px, py)` on the frame.
    #[expect(clippy::too_many_arguments, reason = "a rectangle is six numbers")]
    fn blit_tile_window(
        frame: &mut EgfxFrame,
        px: usize,
        py: usize,
        sub_x: usize,
        sub_y: usize,
        w: usize,
        h: usize,
        pixels: &[u8],
    ) {
        if pixels.len() < TILE * TILE * 4 {
            return;
        }
        let (fw, fh) = (usize::from(frame.width), usize::from(frame.height));
        if px >= fw || py >= fh || w == 0 {
            return;
        }
        let cols = w.min(fw - px).min(TILE - sub_x);
        let rows = h.min(fh - py).min(TILE - sub_y);
        for row in 0..rows {
            let dst = ((py + row) * fw + px) * 4;
            let src = ((sub_y + row) * TILE + sub_x) * 4;
            frame.rgba[dst..dst + cols * 4].copy_from_slice(&pixels[src..src + cols * 4]);
        }
        grow_dirty(frame, px, py, cols, rows);
    }
}

impl GraphicsPipelineHandler for EgfxHandler {
    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        let w = width.clamp(1, MAX_DIMENSION) as u16;
        let h = height.clamp(1, MAX_DIMENSION) as u16;
        if let Ok(mut frame) = self.shared.lock() {
            // Only reallocate (which zeroes) on an actual size change. GNOME sends
            // RESET_GRAPHICS immediately before repainting; zeroing every time would
            // flash the surface black in the gap before the first new tile lands.
            if frame.width != w || frame.height != h || frame.rgba.is_empty() {
                frame.resize(w, h);
            }
        }
    }

    fn on_surface_created(&mut self, surface: &Surface) {
        self.surfaces.insert(
            surface.id,
            SurfaceInfo {
                origin_x: surface.output_origin_x,
                origin_y: surface.output_origin_y,
                width: surface.width,
                height: surface.height,
            },
        );
    }

    fn on_surface_mapped(&mut self, surface_id: u16, origin_x: u32, origin_y: u32) {
        if let Some(surface) = self.surfaces.get_mut(&surface_id) {
            surface.origin_x = origin_x;
            surface.origin_y = origin_y;
        }
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        self.surfaces.remove(&surface_id);
        // Progressive and classic-RFX state are per-surface; they die with it.
        self.progressive.delete_surface(surface_id);
        self.rfx.remove(&surface_id);
    }

    /// `WireToSurface1` path (uncompressed / H.264): IronRDP hands us already-decoded
    /// RGBA. (No H.264 decoder is configured, so in practice this fires only for
    /// uncompressed updates.)
    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        if update.data.is_empty() {
            return; // decode skipped (no decoder for this codec)
        }
        let (ox, oy) = self
            .surfaces
            .get(&update.surface_id)
            .map(|s| (s.origin_x, s.origin_y))
            .unwrap_or((0, 0));
        let dst_x = (ox + u32::from(update.destination_rectangle.left)) as usize;
        let dst_y = (oy + u32::from(update.destination_rectangle.top)) as usize;
        let (tw, th) = (usize::from(update.width), usize::from(update.height));

        if let Ok(mut frame) = self.shared.lock() {
            let (fw, fh) = (usize::from(frame.width), usize::from(frame.height));
            if tw == 0
                || th == 0
                || dst_x + tw > fw
                || dst_y + th > fh
                || update.data.len() < tw * th * 4
            {
                return;
            }
            for row in 0..th {
                let dst = ((dst_y + row) * fw + dst_x) * 4;
                let src = row * tw * 4;
                frame.rgba[dst..dst + tw * 4].copy_from_slice(&update.data[src..src + tw * 4]);
            }
            grow_dirty(&mut frame, dst_x, dst_y, tw, th);
            // Not marked dirty: presents happen on `on_frame_complete`, so a
            // multi-PDU frame reaches the screen whole (see on_wire_to_surface2).
            self.composited = true;
        }
    }

    /// `WireToSurface2` path: RemoteFX Progressive. IronRDP delivers the raw
    /// progressive stream here without decoding it (it only decodes H.264), so we
    /// decode it ourselves and composite the 64×64 tiles. This is what GNOME
    /// Remote Desktop uses — without it the desktop renders solid black.
    fn on_wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        let Some((sw, sh, ox, oy)) = self
            .surfaces
            .get(&pdu.surface_id)
            .map(|s| (s.width, s.height, s.origin_x, s.origin_y))
        else {
            return;
        };

        let decoded =
            match self
                .progressive
                .decode_bitmap(pdu.surface_id, sw, sh, &pdu.bitmap_data)
            {
                Ok(decoded) => decoded,
                Err(error) => {
                    tracing::warn!("EGFX progressive decode failed: {error}");
                    self.dump_stream(pdu, sw, sh, &error.to_string());
                    return;
                }
            };
        self.dump_stream(pdu, sw, sh, "ok");
        if decoded.tiles.is_empty() {
            return;
        }

        // Borrowed out of `self` so the tile loop can log while `self.shared`
        // is locked.
        let mut unclipped = self.unclipped_logged;
        if let Ok(mut frame) = self.shared.lock() {
            for tile in &decoded.tiles {
                // Surface-relative tile bounds, clipped to the region's dirty
                // rects. Tiles are 64-aligned cells of the encoder's tile
                // cache; the rects say which pixels this frame actually
                // touched. Blitting whole tiles paints cache content over
                // areas the frame did not update — 64px-aligned rectangles of
                // stale image hanging off every partial update (FreeRDP clips
                // in update_tiles for the same reason).
                let tx = usize::from(tile.x_idx) * TILE;
                let ty = usize::from(tile.y_idx) * TILE;
                let mut clipped = 0usize;
                for rect in &decoded.rects {
                    let rx0 = usize::from(rect.x);
                    let ry0 = usize::from(rect.y);
                    let rx1 = rx0 + usize::from(rect.width);
                    let ry1 = ry0 + usize::from(rect.height);
                    let cx0 = tx.max(rx0);
                    let cy0 = ty.max(ry0);
                    let cx1 = (tx + TILE).min(rx1);
                    let cy1 = (ty + TILE).min(ry1);
                    if cx0 >= cx1 || cy0 >= cy1 {
                        continue;
                    }
                    clipped += 1;
                    Self::blit_tile_window(
                        &mut frame,
                        ox as usize + cx0,
                        oy as usize + cy0,
                        cx0 - tx,
                        cy0 - ty,
                        cx1 - cx0,
                        cy1 - cy0,
                        &tile.pixels,
                    );
                }
                if clipped == 0 {
                    // Fail-safe: a tile the server bothered to encode but that
                    // no rect covers means our rect interpretation is wrong,
                    // not that the tile is unwanted. Blit it whole rather than
                    // drop it — dropping freezes the desktop, which is far
                    // worse than the stale-cache edges clipping exists to
                    // avoid. The warning says which reading is wrong.
                    if unclipped < 8 {
                        unclipped += 1;
                        tracing::warn!(
                            tile_x = tile.x_idx,
                            tile_y = tile.y_idx,
                            rect_count = decoded.rects.len(),
                            "tile covered by no region rect; blitting it whole"
                        );
                    }
                    Self::blit_tile_window(
                        &mut frame,
                        ox as usize + tx,
                        oy as usize + ty,
                        0,
                        0,
                        TILE,
                        TILE,
                        &tile.pixels,
                    );
                }
            }
            self.composited = true;
            // Deliberately NOT marked dirty here: presents are frame-atomic.
            // A frame's PDUs are decoded back-to-back inside one process()
            // call, but the session loop samples the framebuffer once per
            // transport read — marking dirty per PDU let it emit a frame
            // mid-composite, flashing the blurry base pass of a progressive
            // sequence before its refinements landed. `on_frame_complete`
            // (the EndFrame handler) is the present point.
        }
        self.unclipped_logged = unclipped;
    }

    /// Fill rectangles with a solid colour (MS-RDPEGFX 2.2.2.6).
    ///
    /// Windows clears window backgrounds and control interiors this way rather
    /// than encoding flat pixels. Unimplemented, those areas kept whatever was
    /// underneath — a text box never appeared to clear, so typing looked like
    /// it did nothing.
    fn on_solid_fill(&mut self, pdu: &SolidFillPdu) {
        let Some((ox, oy)) = self.surfaces.get(&pdu.surface_id).map(|s| (s.origin_x, s.origin_y))
        else {
            return;
        };
        let c = &pdu.fill_pixel;
        if let Ok(mut frame) = self.shared.lock() {
            let (fw, fh) = (usize::from(frame.width), usize::from(frame.height));
            for rect in &pdu.rectangles {
                // RDPGFX_RECT16 is exclusive: right/bottom are one-past-end.
                let x0 = (ox as usize + usize::from(rect.left)).min(fw);
                let y0 = (oy as usize + usize::from(rect.top)).min(fh);
                let x1 = (ox as usize + usize::from(rect.right)).min(fw);
                let y1 = (oy as usize + usize::from(rect.bottom)).min(fh);
                if x0 >= x1 || y0 >= y1 {
                    continue;
                }
                for y in y0..y1 {
                    for x in x0..x1 {
                        let i = (y * fw + x) * 4;
                        frame.rgba[i] = c.r;
                        frame.rgba[i + 1] = c.g;
                        frame.rgba[i + 2] = c.b;
                        frame.rgba[i + 3] = 0xFF;
                    }
                }
                grow_dirty(&mut frame, x0, y0, x1 - x0, y1 - y0);
            }
            self.composited = true;
        }
    }

    /// Copy a rectangle to one or more destinations (MS-RDPEGFX 2.2.2.7).
    ///
    /// This is how scrolling and window drags move pixels the client already
    /// has, without re-encoding them.
    fn on_surface_to_surface(&mut self, pdu: &SurfaceToSurfacePdu) {
        let Some((sox, soy)) = self
            .surfaces
            .get(&pdu.source_surface_id)
            .map(|s| (s.origin_x, s.origin_y))
        else {
            return;
        };
        let Some((dox, doy)) = self
            .surfaces
            .get(&pdu.destination_surface_id)
            .map(|s| (s.origin_x, s.origin_y))
        else {
            return;
        };
        let r = &pdu.source_rectangle;
        let (w, h) = (
            usize::from(r.right.saturating_sub(r.left)),
            usize::from(r.bottom.saturating_sub(r.top)),
        );
        if let Ok(mut frame) = self.shared.lock() {
            // Read once, then write every destination: the regions may overlap,
            // and a straight memmove per destination would smear.
            let Some(pixels) = Self::read_rect(
                &frame,
                sox as usize + usize::from(r.left),
                soy as usize + usize::from(r.top),
                w,
                h,
            ) else {
                return;
            };
            for point in &pdu.destination_points {
                Self::blit_rect(
                    &mut frame,
                    dox as usize + usize::from(point.x),
                    doy as usize + usize::from(point.y),
                    w,
                    h,
                    &pixels,
                );
            }
            self.composited = true;
        }
    }

    /// Store a rectangle into the bitmap cache (MS-RDPEGFX 2.2.2.8).
    fn on_surface_to_cache(&mut self, pdu: &SurfaceToCachePdu) {
        let Some((ox, oy)) = self.surfaces.get(&pdu.surface_id).map(|s| (s.origin_x, s.origin_y))
        else {
            return;
        };
        let r = &pdu.source_rectangle;
        let (w, h) = (
            r.right.saturating_sub(r.left),
            r.bottom.saturating_sub(r.top),
        );
        if let Ok(frame) = self.shared.lock() {
            if let Some(rgba) = Self::read_rect(
                &frame,
                ox as usize + usize::from(r.left),
                oy as usize + usize::from(r.top),
                usize::from(w),
                usize::from(h),
            ) {
                self.cache.insert(
                    pdu.cache_slot,
                    CachedBitmap {
                        width: w,
                        height: h,
                        rgba,
                    },
                );
            }
        }
    }

    /// Blit a cached rectangle to one or more destinations (MS-RDPEGFX 2.2.2.9).
    fn on_cache_to_surface(&mut self, pdu: &CacheToSurfacePdu) {
        let Some((ox, oy)) = self.surfaces.get(&pdu.surface_id).map(|s| (s.origin_x, s.origin_y))
        else {
            return;
        };
        let Some(entry) = self.cache.get(&pdu.cache_slot) else {
            // A slot the server believes we hold. It will not re-send those
            // pixels, so the area stays stale — worth knowing about.
            tracing::warn!(slot = pdu.cache_slot, "cache blit for an unknown slot");
            return;
        };
        if let Ok(mut frame) = self.shared.lock() {
            for point in &pdu.destination_points {
                Self::blit_rect(
                    &mut frame,
                    ox as usize + usize::from(point.x),
                    oy as usize + usize::from(point.y),
                    usize::from(entry.width),
                    usize::from(entry.height),
                    &entry.rgba,
                );
            }
            self.composited = true;
        }
    }

    /// Drop cache entries the server is done with (MS-RDPEGFX 2.2.2.10).
    fn on_evict_cache_entry(&mut self, pdu: &ironrdp_egfx::pdu::EvictCacheEntryPdu) {
        self.cache.remove(&pdu.cache_slot);
    }

    /// Anything the pipeline did not route itself. The EGFX client decodes
    /// only uncompressed and H.264 in WireToSurface1; ClearCodec and classic
    /// RemoteFX land here and are decoded with the implementations that
    /// already exist one crate over. Everything else is logged, because an
    /// unhandled PDU is invisible otherwise.
    fn on_unhandled_pdu(&mut self, pdu: &ironrdp_egfx::pdu::GfxPdu) {
        use ironrdp_egfx::pdu::{Codec1Type, GfxPdu};

        if let GfxPdu::WireToSurface1(pdu) = pdu {
            match pdu.codec_id {
                Codec1Type::ClearCodec => return self.decode_clear_codec(pdu),
                Codec1Type::RemoteFx => return self.decode_classic_rfx(pdu),
                _ => {}
            }
        }
        if self.unhandled_logged < 20 {
            self.unhandled_logged += 1;
            tracing::warn!("unhandled EGFX pdu: {pdu:?}");
        }
    }

    fn on_delete_encoding_context(&mut self, pdu: &DeleteEncodingContextPdu) {
        self.progressive.delete_context(pdu.codec_context_id);
    }

    fn on_frame_complete(&mut self, _frame_id: u32) {
        // Present point: publish once per EndFrame, and only when the frame
        // actually painted something — the mirror half of the "no dirty per
        // PDU" rule above.
        if self.frames_seen < 20 {
            self.frames_seen += 1;
            tracing::info!(composited = self.composited, "EGFX frame complete");
        }
        if !self.composited {
            return;
        }
        self.composited = false;
        if let Ok(mut frame) = self.shared.lock() {
            frame.dirty = true;
        }
    }
}
