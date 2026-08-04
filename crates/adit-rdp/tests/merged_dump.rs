//! Replay a whole captured session — progressive AND ClearCodec — in arrival
//! order, through the live compositing logic.
//!
//! The two single-codec harnesses each lie by omission. `progressive_dump`
//! blits whole tiles and skips ClearCodec entirely, so its surface accumulates
//! residue the live path would have painted over; `clearcodec_dump` leaves
//! every progressive region unpainted. Neither can answer the question that
//! matters once the desktop is *nearly* right: after a scroll settles, why does
//! the frame still hold several animation positions at once?
//!
//! This one mirrors `egfx.rs` exactly — progressive tiles clipped to the
//! region's rects, ClearCodec decoded over the destination and blitted opaque —
//! so a divergence between its output and a captured framebuffer means the loss
//! is downstream of compositing, and a match means it is in the decode.
//!
//! `ADIT_MERGE_CLIP=0` blits whole tiles instead of clipping them. Rendering
//! both and comparing is the point: if the clipped surface keeps stale strips
//! that the unclipped one does not, the rects are too small; if the unclipped
//! one smears tile-cache content the clipped one does not, they are too big.
//!
//! Ordering comes from file mtime, since the dumps carry no timestamp. Streams
//! written inside the same filesystem tick can transpose; that is harmless for
//! codecs whose state is per-tile, and visible as noise if it ever is not.
//!
//! Captures are pictures of somebody's desktop; they stay on the machine that
//! produced them and never enter the repo.
//!
//! ```text
//! ADIT_MERGE_DIR=%APPDATA%\Adit ADIT_MERGE_SURFACE=1908x1152 \
//!   cargo test --manifest-path crates/adit-rdp/Cargo.toml \
//!     --test merged_dump -- --ignored --nocapture
//! ```

use ironrdp_graphics::clearcodec::ClearCodecDecoder;
use ironrdp_graphics::progressive::ProgressiveDecoder;

const TILE: usize = 64;

/// One canvas-mutating event reconstructed from the helper's paint trace.
enum LogOp {
    /// Decode `progressive-<index>.bin` and blit its tiles.
    Progressive { index: u32 },
    /// Decode the next ok ClearCodec dump at the logged rectangle.
    Clear { x: usize, y: usize, w: usize, h: usize },
    /// Copy a canvas rectangle into a cache slot.
    Fill { slot: u16, x: usize, y: usize, w: usize, h: usize },
    /// Blit a cache slot back onto the canvas.
    Restore { slot: u16, x: usize, y: usize },
    /// A solid colour fill, colour carried in the log line.
    Solid { r: u8, g: u8, b: u8, x: usize, y: usize, w: usize, h: usize },
    /// A paint whose pixels the capture does not carry (colourless old logs).
    Unknown,
}

enum Stream {
    Progressive,
    Clear { x: u16, y: u16, w: u16, h: u16 },
}

struct Capture {
    order: (std::time::SystemTime, u32),
    path: std::path::PathBuf,
    kind: Stream,
}

#[test]
#[ignore = "needs captured streams; see the module docs"]
fn a_captured_session_replays() {
    let dir = std::env::var("ADIT_MERGE_DIR").expect("ADIT_MERGE_DIR");
    let surface = std::env::var("ADIT_MERGE_SURFACE").unwrap_or_else(|_| "1908x1152".into());
    // Whole-tile application is the live behaviour (see egfx.rs: the region
    // rects are dirty hints, not a clip mask — a captured session's raw bytes
    // proved a fresh full-tile re-encode arriving under a 3px rect).
    // ADIT_MERGE_CLIP=1 reproduces the legacy clipped compositing that caused
    // the frozen scroll residue, kept for comparison.
    let clip = std::env::var("ADIT_MERGE_CLIP").as_deref() == Ok("1");
    let (w, h) = surface.split_once('x').expect("WIDTHxHEIGHT");
    let (w, h): (u16, u16) = (w.parse().expect("width"), h.parse().expect("height"));
    let (fw, fh) = (usize::from(w), usize::from(h));

    let mut captures = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|ext| ext != "bin") {
            continue;
        }
        let name = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mtime = path
            .metadata()
            .and_then(|meta| meta.modified())
            .expect("mtime");

        let (index, kind) = if let Some(stem) = name.strip_prefix("progressive-") {
            let Ok(index) = stem.parse::<u32>() else {
                continue;
            };
            (index, Stream::Progressive)
        } else if let Some(stem) = name.strip_prefix("clear-") {
            let Ok(index) = stem.parse::<u32>() else {
                continue;
            };
            // "rect=WxH at x,y" in the sidecar.
            let meta = std::fs::read_to_string(path.with_extension("txt")).expect("sidecar");
            let Some(rect) = meta
                .lines()
                .find_map(|line| line.strip_prefix("rect="))
                .map(str::to_owned)
            else {
                continue;
            };
            let (wh, at) = rect.split_once(" at ").expect("rect format");
            let (rw, rh) = wh.split_once('x').expect("rect wh");
            let (rx, ry) = at.split_once(',').expect("rect at");
            (
                index,
                Stream::Clear {
                    x: rx.trim().parse().expect("x"),
                    y: ry.trim().parse().expect("y"),
                    w: rw.trim().parse().expect("w"),
                    h: rh.trim().parse().expect("h"),
                },
            )
        } else {
            continue;
        };
        captures.push(Capture {
            order: (mtime, index),
            path,
            kind,
        });
    }
    captures.sort_by_key(|capture| capture.order);
    assert!(!captures.is_empty(), "no dumps in {dir}");

    let progressive_count = captures
        .iter()
        .filter(|c| matches!(c.kind, Stream::Progressive))
        .count();
    println!(
        "{} streams ({progressive_count} progressive, {} clear), clipping {}",
        captures.len(),
        captures.len() - progressive_count,
        if clip { "ON" } else { "OFF" }
    );

    // ADIT_MERGE_PROBE="x,y[;x,y]": whenever a progressive stream decodes a
    // tile covering a probe point, save that tile's pixels as a PNG. This is
    // what separates "the decoder authored the ghost" from "the compositor
    // lost the correct pixels": the tile PNG is the decoder's output before
    // any clipping or blitting touches it.
    let probes: Vec<(usize, usize)> = std::env::var("ADIT_MERGE_PROBE")
        .map(|s| {
            s.split(';')
                .filter_map(|p| {
                    let (x, y) = p.split_once(',')?;
                    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
                })
                .collect()
        })
        .unwrap_or_default();

    // ADIT_MERGE_LOG=<rdp-helper.log>: replay the session from the helper's
    // own paint trace instead of dump mtimes — including both cache ops as
    // real canvas copies. The dump-only replay cannot predict scroll results
    // at all: the scroll is carried by SurfaceToCache/CacheToSurface, whose
    // pixels exist only as copies of the canvas at fill time.
    if let Ok(log_path) = std::env::var("ADIT_MERGE_LOG") {
        replay_from_log(&dir, &log_path, w, h, &probes);
        return;
    }

    let mut progressive = ProgressiveDecoder::new();
    let mut clear = ClearCodecDecoder::new();
    let mut rgba = vec![0u8; fw * fh * 4];
    let (mut failures, mut uncovered_tiles, mut orphan_upgrades) = (0usize, 0usize, 0usize);
    let (mut clear_pixels, mut clear_untouched, mut clear_blank) = (0usize, 0usize, 0usize);

    for capture in &captures {
        let data = std::fs::read(&capture.path).expect("read stream");
        match capture.kind {
            Stream::Progressive => {
                let decoded = match progressive.decode_bitmap(0, w, h, &data) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        failures += 1;
                        println!("{}: FAILED: {error}", capture.path.display());
                        continue;
                    }
                };
                orphan_upgrades += decoded.upgrades_without_base;
                for tile in &decoded.tiles {
                    let (tx, ty) = (usize::from(tile.x_idx) * TILE, usize::from(tile.y_idx) * TILE);
                    if probes
                        .iter()
                        .any(|&(px, py)| (tx..tx + TILE).contains(&px) && (ty..ty + TILE).contains(&py))
                    {
                        let stem = capture.path.file_stem().unwrap_or_default().to_string_lossy();
                        let out = format!("{dir}/probe-{stem}-t{}x{}.png", tile.x_idx, tile.y_idx);
                        image::save_buffer(&out, &tile.pixels, 64, 64, image::ColorType::Rgba8)
                            .expect("probe png");
                        println!("probe {out}");
                    }
                }
                for tile in &decoded.tiles {
                    let (tx, ty) = (usize::from(tile.x_idx) * TILE, usize::from(tile.y_idx) * TILE);
                    let mut hits = 0usize;
                    if clip {
                        for rect in &decoded.rects {
                            let (rx0, ry0) = (usize::from(rect.x), usize::from(rect.y));
                            let rx1 = rx0 + usize::from(rect.width);
                            let ry1 = ry0 + usize::from(rect.height);
                            let (cx0, cy0) = (tx.max(rx0), ty.max(ry0));
                            let (cx1, cy1) = ((tx + TILE).min(rx1), (ty + TILE).min(ry1));
                            if cx0 >= cx1 || cy0 >= cy1 {
                                continue;
                            }
                            hits += 1;
                            blit_tile(
                                &mut rgba,
                                fw,
                                fh,
                                cx0,
                                cy0,
                                cx0 - tx,
                                cy0 - ty,
                                cx1 - cx0,
                                cy1 - cy0,
                                &tile.pixels,
                            );
                        }
                    }
                    if hits == 0 {
                        if clip {
                            uncovered_tiles += 1;
                        }
                        blit_tile(&mut rgba, fw, fh, tx, ty, 0, 0, TILE, TILE, &tile.pixels);
                    }
                }
            }
            Stream::Clear { x, y, w: cw, h: ch } => {
                let (dx, dy) = (usize::from(x), usize::from(y));
                let (cwu, chu) = (usize::from(cw), usize::from(ch));
                // Seed with the destination, exactly as the live path does.
                let background = (dx + cwu <= fw && dy + chu <= fh).then(|| {
                    let mut bgra = Vec::with_capacity(cwu * chu * 4);
                    for row in 0..chu {
                        let start = ((dy + row) * fw + dx) * 4;
                        bgra.extend_from_slice(&rgba[start..start + cwu * 4]);
                    }
                    for px in bgra.chunks_exact_mut(4) {
                        px.swap(0, 2); // RGBA -> BGRA
                    }
                    bgra
                });
                let mut bgra = match clear.decode_onto(&data, cw, ch, background.as_deref()) {
                    Ok(bgra) => bgra,
                    Err(error) => {
                        failures += 1;
                        println!("{}: FAILED: {error}", capture.path.display());
                        continue;
                    }
                };
                // How much of the tile did the codec actually write? Seeded
                // with the destination, a stream that paints nothing is
                // indistinguishable on screen from one that paints correctly —
                // both leave the pixels alone. Counting output pixels that
                // still equal the seed is the only way to see under-painting.
                if let Some(seed) = background.as_deref() {
                    let same = bgra
                        .chunks_exact(4)
                        .zip(seed.chunks_exact(4))
                        .filter(|(out, bg)| out[..3] == bg[..3])
                        .count();
                    clear_pixels += cwu * chu;
                    clear_untouched += same;
                    if same * 10 >= cwu * chu * 9 {
                        clear_blank += 1;
                    }
                }
                for px in bgra.chunks_exact_mut(4) {
                    px.swap(0, 2); // BGRA -> RGBA
                }
                if dx >= fw || dy >= fh {
                    continue;
                }
                let cols = cwu.min(fw - dx);
                for row in 0..chu.min(fh - dy) {
                    let src = row * cwu * 4;
                    let dst = ((dy + row) * fw + dx) * 4;
                    rgba[dst..dst + cols * 4].copy_from_slice(&bgra[src..src + cols * 4]);
                }
            }
        }
    }

    let out = format!("{dir}/merged-{}.png", if clip { "clipped" } else { "whole" });
    image::save_buffer(&out, &rgba, u32::from(w), u32::from(h), image::ColorType::Rgba8)
        .expect("write png");
    println!("composited -> {out}");
    println!(
        "{failures} decode failures, {uncovered_tiles} tiles no rect covered, \
         {orphan_upgrades} upgrades with no first pass"
    );
    if clear_pixels > 0 {
        println!(
            "ClearCodec coverage: {:.1}% of pixels left at the seeded value,              {clear_blank} streams that painted almost nothing",
            clear_untouched as f64 * 100.0 / clear_pixels as f64
        );
    }
}

/// Replay the session from the helper's paint trace.
///
/// Ordering and geometry come from the log; pixel data comes from the dumps
/// (progressive via the `wrote progressive stream` lines, ClearCodec by
/// pairing the k-th "clear" paint with the k-th ok-outcome dump) and, for the
/// cache ops, from the simulated canvas itself — exactly how the live helper
/// works. Solid fills carry no pixels in the capture and are skipped, counted.
///
/// `ADIT_MERGE_SIM_SNAPSHOT=HH:MM:SS.mmm` saves the canvas the first time a
/// log timestamp reaches that value — e.g. just before the fill burst, to see
/// what the cache would have captured. Output: `merged-sim.png` plus a
/// `sim-cell-<x>x<y>.png` crop per probe.
fn replay_from_log(dir: &str, log_path: &str, w: u16, h: u16, probes: &[(usize, usize)]) {
    let (fw, fh) = (usize::from(w), usize::from(h));
    let log = std::fs::read_to_string(log_path).expect("read log");

    // ClearCodec dumps that decoded ok in live, in index order.
    type ClearBin = (Vec<u8>, (usize, usize, usize, usize));
    let mut clear_bins: Vec<ClearBin> = Vec::new();
    for index in 0..10_000u32 {
        let path = format!("{dir}/clear-{index:03}.bin");
        let Ok(data) = std::fs::read(&path) else { break };
        let meta = std::fs::read_to_string(format!("{dir}/clear-{index:03}.txt")).expect("sidecar");
        let mut rect = None;
        let mut ok = false;
        for line in meta.lines() {
            if let Some(r) = line.strip_prefix("rect=") {
                let (wh, at) = r.split_once(" at ").expect("rect format");
                let (rw, rh) = wh.split_once('x').expect("rect wh");
                let (rx, ry) = at.split_once(',').expect("rect at");
                rect = Some((
                    rx.trim().parse().expect("x"),
                    ry.trim().parse().expect("y"),
                    rw.trim().parse().expect("w"),
                    rh.trim().parse().expect("h"),
                ));
            } else if let Some(o) = line.strip_prefix("outcome=") {
                ok = o == "ok";
            }
        }
        if ok {
            clear_bins.push((data, rect.expect("sidecar rect")));
        }
    }

    let mut ops: Vec<(String, LogOp)> = Vec::new();
    for line in log.lines() {
        let ts = line.get(11..23).unwrap_or_default().to_owned();
        if line.contains("wrote progressive stream to") {
            let index = line
                .rsplit("progressive-")
                .next()
                .and_then(|s| s.strip_suffix(".bin"))
                .and_then(|s| s.parse::<u32>().ok());
            if let Some(index) = index {
                ops.push((ts, LogOp::Progressive { index }));
            }
            continue;
        }
        let Some(rest) = line.split("paint source=\"").nth(1) else {
            continue;
        };
        let Some((source, rest)) = rest.split_once('"') else {
            continue;
        };
        let grab = |key: &str| {
            rest.split(key)
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<usize>().ok())
        };
        let (Some(x), Some(y), Some(gw), Some(gh)) = (grab("x="), grab("y="), grab("w="), grab("h=")) else {
            continue;
        };
        let slot = grab("slot=").map(|v| u16::try_from(v).expect("slot"));
        let op = match source {
            "solid-fill" => match (grab("r="), grab("g="), grab("b=")) {
                (Some(r), Some(g), Some(b)) => LogOp::Solid {
                    r: u8::try_from(r).expect("r"),
                    g: u8::try_from(g).expect("g"),
                    b: u8::try_from(b).expect("b"),
                    x,
                    y,
                    w: gw,
                    h: gh,
                },
                _ => LogOp::Unknown,
            },
            "clear" => LogOp::Clear { x, y, w: gw, h: gh },
            "surface-to-cache" => LogOp::Fill { slot: slot.expect("fill slot"), x, y, w: gw, h: gh },
            "cache-to-surface" => LogOp::Restore { slot: slot.expect("restore slot"), x, y },
            // The decode is driven by the WARN lines; these are the clip-era
            // blit records and carry no pixels of their own.
            "progressive" | "progressive-whole" => continue,
            _ => LogOp::Unknown,
        };
        ops.push((ts, op));
    }
    println!("{} ops from the log, {} ok clear dumps", ops.len(), clear_bins.len());

    let snapshot_at = std::env::var("ADIT_MERGE_SIM_SNAPSHOT").ok();
    let mut snapshot_done = false;
    let mut progressive_decoder = ProgressiveDecoder::new();
    let mut clear_decoder = ClearCodecDecoder::new();
    let mut canvas = vec![0u8; fw * fh * 4];
    let mut slots: std::collections::HashMap<u16, (usize, usize, Vec<u8>)> =
        std::collections::HashMap::new();
    let mut clear_cursor = 0usize;
    let (mut failures, mut solid_skipped, mut rect_mismatch) = (0usize, 0usize, 0usize);

    for (ts, op) in &ops {
        if let Some(at) = &snapshot_at {
            if !snapshot_done && ts.as_str() >= at.as_str() {
                snapshot_done = true;
                image::save_buffer(
                    format!("{dir}/sim-snapshot.png"),
                    &canvas,
                    u32::from(w),
                    u32::from(h),
                    image::ColorType::Rgba8,
                )
                .expect("snapshot png");
                println!("snapshot at {ts} -> sim-snapshot.png");
            }
        }
        match *op {
            LogOp::Progressive { index } => {
                let path = format!("{dir}/progressive-{index}.bin");
                let Ok(data) = std::fs::read(&path) else {
                    println!("missing {path}");
                    failures += 1;
                    continue;
                };
                match progressive_decoder.decode_bitmap(0, w, h, &data) {
                    Ok(decoded) => {
                        // Hybrid rule, mirroring egfx.rs: fresh passes whole,
                        // upgrades clipped to the region rects.
                        for tile in &decoded.tiles {
                            let tx = usize::from(tile.x_idx) * TILE;
                            let ty = usize::from(tile.y_idx) * TILE;
                            if tile.fresh {
                                blit_tile(&mut canvas, fw, fh, tx, ty, 0, 0, TILE, TILE, &tile.pixels);
                                continue;
                            }
                            for rect in &decoded.rects {
                                let (rx0, ry0) = (usize::from(rect.x), usize::from(rect.y));
                                let rx1 = rx0 + usize::from(rect.width);
                                let ry1 = ry0 + usize::from(rect.height);
                                let (cx0, cy0) = (tx.max(rx0), ty.max(ry0));
                                let (cx1, cy1) = ((tx + TILE).min(rx1), (ty + TILE).min(ry1));
                                if cx0 >= cx1 || cy0 >= cy1 {
                                    continue;
                                }
                                blit_tile(
                                    &mut canvas,
                                    fw,
                                    fh,
                                    cx0,
                                    cy0,
                                    cx0 - tx,
                                    cy0 - ty,
                                    cx1 - cx0,
                                    cy1 - cy0,
                                    &tile.pixels,
                                );
                            }
                        }
                    }
                    Err(error) => {
                        failures += 1;
                        println!("progressive-{index}: FAILED: {error}");
                    }
                }
            }
            LogOp::Clear { x, y, w: cw, h: ch } => {
                let Some((data, rect)) = clear_bins.get(clear_cursor) else {
                    println!("clear paint at {ts} beyond dumped streams");
                    failures += 1;
                    continue;
                };
                clear_cursor += 1;
                if *rect != (x, y, cw, ch) {
                    rect_mismatch += 1;
                }
                let background = (x + cw <= fw && y + ch <= fh).then(|| {
                    let mut bgra = Vec::with_capacity(cw * ch * 4);
                    for row in 0..ch {
                        let start = ((y + row) * fw + x) * 4;
                        bgra.extend_from_slice(&canvas[start..start + cw * 4]);
                    }
                    for px in bgra.chunks_exact_mut(4) {
                        px.swap(0, 2); // RGBA -> BGRA
                    }
                    bgra
                });
                let (cw16, ch16) = (
                    u16::try_from(cw).expect("clear w"),
                    u16::try_from(ch).expect("clear h"),
                );
                match clear_decoder.decode_onto(data, cw16, ch16, background.as_deref()) {
                    Ok(mut bgra) => {
                        for px in bgra.chunks_exact_mut(4) {
                            px.swap(0, 2); // BGRA -> RGBA
                        }
                        blit_rect(&mut canvas, fw, fh, x, y, cw, ch, &bgra);
                    }
                    Err(error) => {
                        failures += 1;
                        println!("clear at {ts}: FAILED: {error}");
                    }
                }
            }
            LogOp::Fill { slot, x, y, w: sw, h: sh } => {
                // Mirror the live read_rect: an out-of-bounds source stores
                // nothing at all.
                if x + sw <= fw && y + sh <= fh && sw > 0 && sh > 0 {
                    let mut pixels = Vec::with_capacity(sw * sh * 4);
                    for row in 0..sh {
                        let start = ((y + row) * fw + x) * 4;
                        pixels.extend_from_slice(&canvas[start..start + sw * 4]);
                    }
                    slots.insert(slot, (sw, sh, pixels));
                }
            }
            LogOp::Restore { slot, x, y } => match slots.get(&slot) {
                Some(&(sw, sh, ref pixels)) => blit_rect(&mut canvas, fw, fh, x, y, sw, sh, pixels),
                None => println!("restore at {ts}: slot {slot} never filled"),
            },
            LogOp::Solid { r, g, b, x, y, w: sw, h: sh } => {
                let mut row = Vec::with_capacity(sw * 4);
                for _ in 0..sw {
                    row.extend_from_slice(&[r, g, b, 0xFF]);
                }
                for dy in y..(y + sh).min(fh) {
                    let cols = sw.min(fw.saturating_sub(x));
                    let dst = (dy * fw + x) * 4;
                    canvas[dst..dst + cols * 4].copy_from_slice(&row[..cols * 4]);
                }
            }
            LogOp::Unknown => solid_skipped += 1,
        }
    }

    image::save_buffer(
        format!("{dir}/merged-sim.png"),
        &canvas,
        u32::from(w),
        u32::from(h),
        image::ColorType::Rgba8,
    )
    .expect("sim png");
    for &(px, py) in probes {
        let (cx, cy) = ((px / TILE) * TILE, (py / TILE) * TILE);
        let mut cell = Vec::with_capacity(TILE * TILE * 4);
        for row in 0..TILE.min(fh.saturating_sub(cy)) {
            let start = ((cy + row) * fw + cx) * 4;
            cell.extend_from_slice(&canvas[start..start + TILE.min(fw - cx) * 4]);
        }
        image::save_buffer(
            format!("{dir}/sim-cell-{cx}x{cy}.png"),
            &cell,
            u32::try_from(TILE.min(fw - cx)).expect("cell w"),
            u32::try_from(TILE.min(fh.saturating_sub(cy))).expect("cell h"),
            image::ColorType::Rgba8,
        )
        .expect("cell png");
    }
    println!(
        "sim done -> merged-sim.png: {failures} failures, {solid_skipped} solid fills skipped, \
         {rect_mismatch} clear rect mismatches"
    );
}

/// Opaque rectangle blit, clamped to the canvas.
#[allow(clippy::too_many_arguments)]
fn blit_rect(canvas: &mut [u8], fw: usize, fh: usize, x: usize, y: usize, w: usize, h: usize, pixels: &[u8]) {
    if x >= fw || y >= fh {
        return;
    }
    let cols = w.min(fw - x);
    let rows = h.min(fh - y);
    for row in 0..rows {
        let src = row * w * 4;
        let dst = ((y + row) * fw + x) * 4;
        canvas[dst..dst + cols * 4].copy_from_slice(&pixels[src..src + cols * 4]);
    }
}

/// Composite a window of one 64x64 tile at an absolute surface position.
#[allow(clippy::too_many_arguments)]
fn blit_tile(
    surface: &mut [u8],
    fw: usize,
    fh: usize,
    dst_x: usize,
    dst_y: usize,
    src_x: usize,
    src_y: usize,
    w: usize,
    h: usize,
    pixels: &[u8],
) {
    if dst_x >= fw || dst_y >= fh {
        return;
    }
    let cols = w.min(fw - dst_x);
    let rows = h.min(fh - dst_y);
    for row in 0..rows {
        let src = ((src_y + row) * TILE + src_x) * 4;
        let dst = ((dst_y + row) * fw + dst_x) * 4;
        surface[dst..dst + cols * 4].copy_from_slice(&pixels[src..src + cols * 4]);
    }
}
