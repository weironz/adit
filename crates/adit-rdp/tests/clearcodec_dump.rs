//! Replay captured ClearCodec streams offline, pixel-visibly.
//!
//! With `ADIT_RDP_DUMP=1` the helper advertises full (non-thin-client) caps so
//! the server sends ClearCodec again, and writes each stream as
//! `clear-NNN.bin` + a `clear-NNN.txt` sidecar under `%APPDATA%\Adit`.
//!
//! This test replays them IN ORDER through ONE `ClearCodecDecoder` — the glyph
//! and v-bar caches are stateful, so order is meaning — composites each onto a
//! persistent surface at its recorded rectangle (alpha-masked, exactly like
//! the live path), and writes PNGs. ClearCodec is lossless: any region that
//! looks wrong localises a bug precisely, and the per-stream tile PNGs say
//! which stream painted it.
//!
//! Captures are pictures of somebody's desktop; they stay on the machine that
//! produced them and never enter the repo.
//!
//! ```text
//! ADIT_CLEAR_DIR=%APPDATA%\Adit ADIT_CLEAR_SURFACE=1908x1152 \
//!   cargo test --manifest-path crates/adit-rdp/Cargo.toml \
//!     --test clearcodec_dump -- --ignored --nocapture
//! ```

use ironrdp_graphics::clearcodec::ClearCodecDecoder;

struct Capture {
    index: u32,
    data: Vec<u8>,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    outcome: String,
}

#[test]
#[ignore = "needs captured streams; see the module docs"]
fn captured_streams_replay() {
    let dir = std::env::var("ADIT_CLEAR_DIR").expect("ADIT_CLEAR_DIR");
    let surface = std::env::var("ADIT_CLEAR_SURFACE").unwrap_or_else(|_| "1908x1152".into());
    let (sw, sh) = surface.split_once('x').expect("WIDTHxHEIGHT");
    let (sw, sh): (usize, usize) = (sw.parse().expect("width"), sh.parse().expect("height"));

    let mut captures = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read dir") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let Some(stem) = name.strip_prefix("clear-").and_then(|n| n.strip_suffix(".bin")) else {
            continue;
        };
        let index: u32 = stem.parse().expect("dump index");
        let meta = std::fs::read_to_string(path.with_extension("txt")).expect("sidecar");
        let mut rect = None;
        let mut outcome = String::new();
        for line in meta.lines() {
            if let Some(r) = line.strip_prefix("rect=") {
                // "WxH at x,y"
                let (wh, at) = r.split_once(" at ").expect("rect format");
                let (w, h) = wh.split_once('x').expect("rect wh");
                let (x, y) = at.split_once(',').expect("rect at");
                rect = Some((
                    x.trim().parse().expect("x"),
                    y.trim().parse().expect("y"),
                    w.trim().parse().expect("w"),
                    h.trim().parse().expect("h"),
                ));
            } else if let Some(o) = line.strip_prefix("outcome=") {
                outcome = o.to_owned();
            }
        }
        let (x, y, w, h) = rect.expect("sidecar rect");
        captures.push(Capture {
            index,
            data: std::fs::read(&path).expect("read stream"),
            x,
            y,
            w,
            h,
            outcome,
        });
    }
    captures.sort_by_key(|c| c.index);
    assert!(!captures.is_empty(), "no clear-*.bin in {dir}");
    println!("{} captured streams", captures.len());

    let mut decoder = ClearCodecDecoder::new();
    let mut surface_rgba = vec![0u8; sw * sh * 4];
    let mut failures = 0usize;

    for capture in &captures {
        match decoder.decode(&capture.data, capture.w, capture.h) {
            Ok(mut bgra) => {
                for px in bgra.chunks_exact_mut(4) {
                    px.swap(0, 2); // BGRA -> RGBA
                }
                // Per-stream tile, so a wrong region on the surface can be
                // traced to the stream that painted it.
                let tile_path = format!("{dir}/clear-{:03}.tile.png", capture.index);
                image::save_buffer(
                    &tile_path,
                    &bgra,
                    u32::from(capture.w),
                    u32::from(capture.h),
                    image::ColorType::Rgba8,
                )
                .expect("write tile png");

                // Alpha-masked composite, matching the live path.
                let (x, y) = (usize::from(capture.x), usize::from(capture.y));
                let (w, h) = (usize::from(capture.w), usize::from(capture.h));
                for row in 0..h.min(sh.saturating_sub(y)) {
                    for col in 0..w.min(sw.saturating_sub(x)) {
                        let src = (row * w + col) * 4;
                        if bgra[src + 3] == 0 {
                            continue;
                        }
                        let dst = ((y + row) * sw + x + col) * 4;
                        surface_rgba[dst..dst + 4].copy_from_slice(&bgra[src..src + 4]);
                    }
                }
            }
            Err(error) => {
                failures += 1;
                println!(
                    "clear-{:03}: FAILED offline: {error} (live outcome: {})",
                    capture.index, capture.outcome
                );
            }
        }
    }

    let out = format!("{dir}/clear-surface.png");
    image::save_buffer(
        &out,
        &surface_rgba,
        u32::try_from(sw).expect("width"),
        u32::try_from(sh).expect("height"),
        image::ColorType::Rgba8,
    )
    .expect("write surface png");
    println!("composited surface -> {out}");
    println!("{failures} offline decode failures out of {}", captures.len());
}
