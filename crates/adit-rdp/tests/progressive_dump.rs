//! Decode a captured RemoteFX Progressive stream from disk.
//!
//! `egfx.rs` writes `progressive-fail-N.bin` next to the helper log whenever a
//! stream fails to decode. This replays one, so a decoder fix can be checked
//! against the bytes a real server actually sent rather than against a guess —
//! which is how the 0xFF-quality bug in `vendor/ironrdp-graphics` was found.
//!
//! Env-driven and `#[ignore]`d because a capture is a picture of somebody's
//! desktop: it stays on the machine that produced it and never enters the repo.
//!
//! ```text
//! ADIT_PROGRESSIVE_SURFACE=1524x920 \
//!   cargo test --manifest-path crates/adit-rdp/Cargo.toml \
//!     --test progressive_dump -- --ignored --nocapture
//! ```

use ironrdp_graphics::progressive::ProgressiveDecoder;

/// Replay every `progressive-N.bin` in a directory, in order, through ONE
/// decoder. The order matters: the stream carrying SYNC + CONTEXT establishes
/// the codec context every later frame refers to, so a capture of failures
/// alone cannot be replayed at all — which is exactly how the first capture
/// turned out to be useless.
///
/// Writes the composited surface to PNG after each frame. "It decoded" stopped
/// being the question once the quant fix landed; the question is whether the
/// pixels are right, and only looking answers that.
#[test]
#[ignore = "needs a captured stream; see the module docs"]
fn a_captured_stream_decodes() {
    let dir = std::env::var("ADIT_PROGRESSIVE_DIR").expect("ADIT_PROGRESSIVE_DIR");
    let surface = std::env::var("ADIT_PROGRESSIVE_SURFACE").unwrap_or_else(|_| "1524x920".into());
    let (w, h) = surface.split_once('x').expect("WIDTHxHEIGHT");
    let (w, h): (u16, u16) = (w.parse().expect("width"), h.parse().expect("height"));

    let mut dumps: Vec<_> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "bin")
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("progressive-"))
        })
        .collect();
    dumps.sort();
    assert!(!dumps.is_empty(), "no progressive-*.bin in {dir}");

    let mut decoder = ProgressiveDecoder::new();
    let mut surface_rgba = vec![0u8; usize::from(w) * usize::from(h) * 4];
    let mut decoded_any = false;

    for path in &dumps {
        // Context ids from the sidecar are for the log only: decoder state is
        // keyed per surface now (Windows mints a fresh codecContextId per
        // sequence while difference tiles reference the surface state), and a
        // capture is a single surface.
        let context_id = std::fs::read_to_string(path.with_extension("txt"))
            .ok()
            .and_then(|meta| {
                meta.lines().find_map(|line| {
                    line.strip_prefix("codec_context_id=")?
                        .trim()
                        .parse::<u32>()
                        .ok()
                })
            })
            .unwrap_or(1);
        let data = std::fs::read(path).expect("read dump");

        match decoder.decode_bitmap(0, w, h, &data) {
            Ok(tiles) => {
                decoded_any = true;
                println!(
                    "{}: ctx {context_id}, {} bytes -> {} tiles",
                    path.display(),
                    data.len(),
                    tiles.len()
                );
                for tile in &tiles {
                    assert_eq!(tile.pixels.len(), 64 * 64 * 4, "tile has the wrong size");
                    blit(&mut surface_rgba, w, h, tile.x_idx, tile.y_idx, &tile.pixels);
                }
            }
            Err(error) => println!("{}: ctx {context_id} FAILED: {error}", path.display()),
        }

        let out = path.with_extension("png");
        image::save_buffer(
            &out,
            &surface_rgba,
            u32::from(w),
            u32::from(h),
            image::ColorType::Rgba8,
        )
        .expect("write png");
        println!("  wrote {}", out.display());
    }

    assert!(decoded_any, "not one captured stream decoded");
}

/// Composite one 64x64 RGBA tile at its tile-grid position.
fn blit(surface: &mut [u8], w: u16, h: u16, x_idx: u16, y_idx: u16, pixels: &[u8]) {
    const TILE: usize = 64;
    let (fw, fh) = (usize::from(w), usize::from(h));
    let (px, py) = (usize::from(x_idx) * TILE, usize::from(y_idx) * TILE);
    for row in 0..TILE {
        if py + row >= fh {
            break;
        }
        let cols = TILE.min(fw.saturating_sub(px));
        if cols == 0 {
            break;
        }
        let dst = ((py + row) * fw + px) * 4;
        let src = row * TILE * 4;
        surface[dst..dst + cols * 4].copy_from_slice(&pixels[src..src + cols * 4]);
    }
}
