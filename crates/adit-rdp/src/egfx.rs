//! EGFX (RDPGFX graphics pipeline, MS-RDPEGFX) client handler.
//!
//! GNOME Remote Desktop — and modern Windows — serve graphics only over the EGFX
//! pipeline, not the legacy bitmap path; a client that doesn't advertise it is
//! rejected at capabilities exchange. We attach a [`GraphicsPipelineClient`] to
//! the dynamic virtual channels and composite the decoded RGBA surface updates it
//! hands us into a shared framebuffer the session loop emits as tiles.
//!
//! No H.264 decoder is configured, so IronRDP advertises the V8 (no-AVC)
//! capability set and the server falls back to a codec we can decode (RemoteFX
//! Progressive / planar / uncompressed / ClearCodec).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineHandler, Surface};

/// Match the framebuffer clamp on the app side.
const MAX_DIMENSION: u32 = 8192;

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

pub(crate) struct EgfxHandler {
    shared: SharedEgfx,
    /// surface_id → output origin (x, y). A bitmap update targets a surface; the
    /// surface is mapped to a position on the output.
    origins: HashMap<u16, (u32, u32)>,
}

impl EgfxHandler {
    pub(crate) fn new(shared: SharedEgfx) -> Self {
        Self {
            shared,
            origins: HashMap::new(),
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
        if surface.is_mapped {
            self.origins
                .insert(surface.id, (surface.output_origin_x, surface.output_origin_y));
        }
    }

    fn on_surface_mapped(&mut self, surface_id: u16, origin_x: u32, origin_y: u32) {
        self.origins.insert(surface_id, (origin_x, origin_y));
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        self.origins.remove(&surface_id);
    }

    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        if update.data.is_empty() {
            // No decoded pixels for this codec/command — nothing to composite.
            tracing::debug!(surface_id = update.surface_id, "EGFX bitmap update with no decoded data");
            return;
        }
        let (ox, oy) = self.origins.get(&update.surface_id).copied().unwrap_or((0, 0));
        let dst_x = (ox + u32::from(update.destination_rectangle.left)) as usize;
        let dst_y = (oy + u32::from(update.destination_rectangle.top)) as usize;
        let (tw, th) = (usize::from(update.width), usize::from(update.height));

        if let Ok(mut frame) = self.shared.lock() {
            let (fw, fh) = (usize::from(frame.width), usize::from(frame.height));
            // Drop anything out of bounds or short of the declared size.
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

    fn on_frame_complete(&mut self, _frame_id: u32) {
        if let Ok(mut frame) = self.shared.lock() {
            frame.dirty = true;
        }
    }
}
