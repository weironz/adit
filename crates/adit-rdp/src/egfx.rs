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
use ironrdp_egfx::pdu::{DeleteEncodingContextPdu, WireToSurface2Pdu};
use ironrdp_graphics::progressive::ProgressiveDecoder;

/// Match the framebuffer clamp on the app side.
const MAX_DIMENSION: u32 = 8192;

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
}

impl EgfxFrame {
    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.rgba = vec![0u8; usize::from(width) * usize::from(height) * 4];
        self.dirty = true;
    }
}

pub(crate) type SharedEgfx = Arc<Mutex<EgfxFrame>>;

pub(crate) fn new_shared() -> SharedEgfx {
    Arc::new(Mutex::new(EgfxFrame {
        rgba: Vec::new(),
        width: 0,
        height: 0,
        dirty: false,
    }))
}

/// If a new EGFX frame is ready, return its size + a full-frame RGBA copy and
/// clear the dirty flag. `None` when nothing changed (or EGFX isn't in use).
pub(crate) fn take_frame(shared: &SharedEgfx) -> Option<(u16, u16, Vec<u8>)> {
    let mut frame = shared.lock().ok()?;
    if !frame.dirty || frame.rgba.is_empty() {
        return None;
    }
    frame.dirty = false;
    Some((frame.width, frame.height, frame.rgba.clone()))
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
}

impl EgfxHandler {
    pub(crate) fn new(shared: SharedEgfx) -> Self {
        Self {
            shared,
            surfaces: HashMap::new(),
            progressive: ProgressiveDecoder::new(),
        }
    }

    /// Composite a 64×64 RGBA tile at output pixel (`px`, `py`), clamped to the
    /// framebuffer (edge tiles overhang a surface whose size isn't a multiple of 64).
    fn blit_tile(frame: &mut EgfxFrame, px: usize, py: usize, pixels: &[u8]) {
        if pixels.len() < TILE * TILE * 4 {
            return;
        }
        let (fw, fh) = (usize::from(frame.width), usize::from(frame.height));
        if px >= fw || py >= fh {
            return;
        }
        let cols = TILE.min(fw - px);
        let rows = TILE.min(fh - py);
        for row in 0..rows {
            let dst = ((py + row) * fw + px) * 4;
            let src = row * TILE * 4;
            frame.rgba[dst..dst + cols * 4].copy_from_slice(&pixels[src..src + cols * 4]);
        }
    }
}

impl GraphicsPipelineHandler for EgfxHandler {
    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        let w = width.clamp(1, MAX_DIMENSION) as u16;
        let h = height.clamp(1, MAX_DIMENSION) as u16;
        if let Ok(mut frame) = self.shared.lock() {
            frame.resize(w, h);
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
            frame.dirty = true;
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

        let tiles =
            match self
                .progressive
                .decode_bitmap(pdu.codec_context_id, sw, sh, &pdu.bitmap_data)
            {
                Ok(tiles) => tiles,
                Err(error) => {
                    tracing::warn!("EGFX progressive decode failed: {error}");
                    return;
                }
            };
        if tiles.is_empty() {
            return;
        }

        if let Ok(mut frame) = self.shared.lock() {
            for tile in &tiles {
                let px = ox as usize + usize::from(tile.x_idx) * TILE;
                let py = oy as usize + usize::from(tile.y_idx) * TILE;
                Self::blit_tile(&mut frame, px, py, &tile.pixels);
            }
            frame.dirty = true;
        }
    }

    fn on_delete_encoding_context(&mut self, pdu: &DeleteEncodingContextPdu) {
        self.progressive.delete_context(pdu.codec_context_id);
    }

    fn on_frame_complete(&mut self, _frame_id: u32) {
        if let Ok(mut frame) = self.shared.lock() {
            frame.dirty = true;
        }
    }
}
