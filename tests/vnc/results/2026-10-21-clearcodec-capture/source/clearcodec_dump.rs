//! Replay captured ClearCodec streams offline, pixel-visibly.
//!
//! With `ADIT_RDP_DUMP=1` the helper records received ClearCodec streams as
//! `clear-NNN.bin` + a `clear-NNN.txt` sidecar under `%APPDATA%\Adit`.
//! Dumping does not change codec negotiation. Use a fresh private APPDATA for
//! each helper connection; overlapping captures cannot supply a valid cache.
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
//!     --test clearcodec_dump captured_streams_replay -- --ignored --nocapture
//! ```
//!
//! In-flight records left by a panic are rejected by default. For forensic
//! replay only, `ADIT_CLEAR_ALLOW_PENDING=1` permits them; this never proves the
//! captured live session completed. Offline decode errors still fail the test.

use ironrdp_graphics::clearcodec::ClearCodecDecoder;
use ironrdp_pdu::codecs::clearcodec::ClearCodecBitmapStream;

/// Metadata only: never retain or print glyph pixels. Keep the original store
/// shape across hits, just as the decoder does; CACHE_RESET resets v-bars, not
/// glyph entries in the implementation being replayed.
#[derive(Default)]
struct GlyphStats {
    flags: std::collections::BTreeMap<u8, usize>,
    slots: std::collections::HashMap<u16, (u16, u16, usize)>,
    stores: usize,
    hits: usize,
    successful_hits: usize,
    missing_stores: usize,
    shape_changes: usize,
    aspect_changes: usize,
    legacy_oob_hits: usize,
}

impl GlyphStats {
    fn observe(
        &mut self,
        stream: &ClearCodecBitmapStream<'_>,
        width: u16,
        height: u16,
        decoded_bytes: Option<usize>,
    ) {
        *self.flags.entry(stream.flags).or_default() += 1;
        let Some(index) = stream.glyph_index else {
            return;
        };
        if stream.is_glyph_hit() {
            self.hits += 1;
            self.successful_hits += usize::from(decoded_bytes.is_some());
            let Some(&(stored_w, stored_h, stored_bytes)) = self.slots.get(&index) else {
                self.missing_stores += 1;
                return;
            };
            self.shape_changes += usize::from((width, height) != (stored_w, stored_h));
            self.aspect_changes += usize::from(
                u64::from(width) * u64::from(stored_h) != u64::from(height) * u64::from(stored_w),
            );
            // Reconstruct the OLD row-stride access arithmetically, without
            // executing a vulnerable decoder. Only successful current decodes
            // can witness this same failure class on an actual capture.
            if height > 0 && decoded_bytes.is_some() {
                let legacy_end =
                    ((u64::from(height) - 1) * u64::from(stored_w) + u64::from(width)) * 4;
                self.legacy_oob_hits += usize::from(legacy_end > stored_bytes as u64);
            }
        } else if let Some(bytes) = decoded_bytes {
            if usize::from(width) * usize::from(height) <= 1024 * 1024 {
                self.slots.insert(index, (width, height, bytes));
                self.stores += 1;
            }
        }
    }
}

#[derive(Debug)]
struct Capture {
    index: u32,
    data: Vec<u8>,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    outcome: String,
    helper_pid: Option<u32>,
}

fn load_captures(dir: &std::path::Path) -> Result<Vec<Capture>, String> {
    let mut captures = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let Some(stem) = name
            .strip_prefix("clear-")
            .and_then(|n| n.strip_suffix(".bin"))
        else {
            continue;
        };
        let index: u32 = stem.parse().map_err(|_| "invalid ClearCodec dump index")?;
        let meta = std::fs::read_to_string(path.with_extension("txt"))
            .map_err(|error| format!("clear-{index:03} sidecar: {error}"))?;
        let mut rect = None;
        let mut outcome = String::new();
        let mut byte_count = None;
        let mut helper_pid = None;
        for line in meta.lines() {
            if let Some(r) = line.strip_prefix("rect=") {
                // "WxH at x,y"
                let (wh, at) = r.split_once(" at ").ok_or("invalid rect format")?;
                let (w, h) = wh.split_once('x').ok_or("invalid rect dimensions")?;
                let (x, y) = at.split_once(',').ok_or("invalid rect origin")?;
                let number = |value: &str| {
                    value
                        .trim()
                        .parse::<u16>()
                        .map_err(|_| "invalid rect number")
                };
                rect = Some((number(x)?, number(y)?, number(w)?, number(h)?));
            } else if let Some(o) = line.strip_prefix("outcome=") {
                // Capture-before-decode appends the final outcome to its
                // pending sidecar. The last record is authoritative.
                outcome = o.to_owned();
            } else if let Some(count) = line.strip_prefix("bytes=") {
                byte_count = Some(count.parse::<usize>().map_err(|_| "invalid byte count")?);
            } else if let Some(pid) = line.strip_prefix("helper_pid=") {
                helper_pid = Some(pid.parse::<u32>().map_err(|_| "invalid helper PID")?);
            }
        }
        let (x, y, w, h) = rect.ok_or("missing sidecar rect")?;
        let data = std::fs::read(&path).map_err(|error| error.to_string())?;
        if data.is_empty()
            || byte_count != Some(data.len())
            || w == 0
            || h == 0
            || outcome.is_empty()
        {
            return Err(format!(
                "clear-{index:03}: incomplete or inconsistent stream metadata"
            ));
        }
        captures.push(Capture {
            index,
            data,
            x,
            y,
            w,
            h,
            outcome,
            helper_pid,
        });
    }
    captures.sort_by_key(|c| c.index);
    if captures.is_empty() {
        return Err("no clear-*.bin in capture directory".into());
    }
    let owner = captures[0].helper_pid;
    for (expected, capture) in captures.iter().enumerate() {
        if usize::try_from(capture.index).ok() != Some(expected) {
            return Err(
                "ClearCodec cache prefix must start at zero without gaps or duplicate indices"
                    .into(),
            );
        }
        if capture.helper_pid != owner {
            return Err("ClearCodec records mix different helper connections".into());
        }
    }
    Ok(captures)
}

fn capture_status(
    captures: &[Capture],
    allow_pending: bool,
) -> Result<(usize, usize), &'static str> {
    let pending = captures
        .iter()
        .filter(|capture| capture.outcome == "pending")
        .count();
    let live_errors = captures
        .iter()
        .filter(|capture| !matches!(capture.outcome.as_str(), "ok" | "pending"))
        .count();
    if pending > 0 && !allow_pending {
        return Err("pending capture is not a completed live session; explicitly opt in for forensic replay");
    }
    Ok((pending, live_errors))
}

#[test]
#[ignore = "needs captured streams; see the module docs"]
fn captured_streams_replay() {
    let dir = std::env::var("ADIT_CLEAR_DIR").expect("ADIT_CLEAR_DIR");
    let surface = std::env::var("ADIT_CLEAR_SURFACE").unwrap_or_else(|_| "1908x1152".into());
    let (sw, sh) = surface.split_once('x').expect("WIDTHxHEIGHT");
    let (sw, sh): (usize, usize) = (sw.parse().expect("width"), sh.parse().expect("height"));
    assert!(
        sw > 0 && sh > 0 && sw <= 8192 && sh <= 8192,
        "invalid replay surface"
    );
    let captures = load_captures(std::path::Path::new(&dir)).expect("complete ordered capture");
    let (pending, live_errors) = capture_status(
        &captures,
        std::env::var("ADIT_CLEAR_ALLOW_PENDING").as_deref() == Ok("1"),
    )
    .expect("completed live capture or explicit forensic override");
    println!(
        "{} captured streams, {pending} pending, {live_errors} live decode errors",
        captures.len()
    );

    let mut decoder = ClearCodecDecoder::new();
    let mut surface_rgba = vec![0u8; sw * sh * 4];
    let mut failures = 0usize;
    let mut glyphs = GlyphStats::default();

    for capture in &captures {
        // Seed the decode with the destination, exactly as the live path does:
        // ClearCodec's layers need not cover the tile, and what they skip keeps
        // the surface's existing pixels. Replaying against a zeroed buffer
        // would hide precisely the class of bug this harness exists to find.
        let (cx, cy) = (usize::from(capture.x), usize::from(capture.y));
        let (cw, ch) = (usize::from(capture.w), usize::from(capture.h));
        assert!(
            cx + cw <= sw && cy + ch <= sh,
            "capture rectangle is outside replay surface"
        );
        let background = (cx + cw <= sw && cy + ch <= sh).then(|| {
            let mut bgra = Vec::with_capacity(cw * ch * 4);
            for row in 0..ch {
                let start = ((cy + row) * sw + cx) * 4;
                bgra.extend_from_slice(&surface_rgba[start..start + cw * 4]);
            }
            for px in bgra.as_chunks_mut::<4>().0 {
                px.swap(0, 2); // RGBA -> BGRA
            }
            bgra
        });
        let outcome =
            decoder.decode_onto(&capture.data, capture.w, capture.h, background.as_deref());
        if let Ok(stream) =
            ClearCodecBitmapStream::decode(&mut ironrdp_core::ReadCursor::new(&capture.data))
        {
            glyphs.observe(
                &stream,
                capture.w,
                capture.h,
                outcome.as_ref().ok().map(Vec::len),
            );
        }
        match outcome {
            Ok(mut bgra) => {
                for px in bgra.as_chunks_mut::<4>().0 {
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

                // Opaque composite, matching the live path: the buffer was
                // seeded with this rectangle, so every pixel is authoritative.
                for row in 0..ch.min(sh.saturating_sub(cy)) {
                    let cols = cw.min(sw.saturating_sub(cx));
                    let src = row * cw * 4;
                    let dst = ((cy + row) * sw + cx) * 4;
                    surface_rgba[dst..dst + cols * 4].copy_from_slice(&bgra[src..src + cols * 4]);
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
    println!(
        "{failures} offline decode failures out of {}",
        captures.len()
    );
    println!(
        "ClearCodec metadata: flags={:?}, glyph_stores={}, glyph_hits={}, successful_hits={}, missing_stores={}, shape_changes={}, aspect_changes={}, legacy_stride_oob_successes={}",
        glyphs.flags, glyphs.stores, glyphs.hits, glyphs.successful_hits,
        glyphs.missing_stores, glyphs.shape_changes, glyphs.aspect_changes, glyphs.legacy_oob_hits,
    );
    if glyphs.legacy_oob_hits == 0 {
        println!("No successful captured glyph hit covered the old stride-overrun class; clean replay alone does not reproduce the original panic.");
    }
    assert_eq!(
        failures, 0,
        "captured ClearCodec sequence did not replay cleanly"
    );
}

#[cfg(test)]
mod manifest_tests {
    use super::*;

    #[test]
    fn glyph_metadata_tracks_original_store_shape_across_hits_and_vbar_reset() {
        let mut decoder = ClearCodecDecoder::new();
        let mut stats = GlyphStats::default();
        // These are synthetic metadata regressions, not captured-host evidence.
        for (data, w, h) in [
            ([1, 0, 0, 0], 2, 2), // Store four pixels in slot zero.
            ([3, 1, 0, 0], 1, 4), // Legal hit, old stride end=20 > len=16.
            ([7, 2, 0, 0], 2, 2), // V-bar reset must not clear or reshape glyphs.
            ([1, 3, 0, 0], 1, 4), // A new store DOES replace the slot shape.
            ([3, 4, 0, 0], 1, 4),
        ] {
            let decoded = decoder.decode_onto(&data, w, h, None).unwrap();
            let stream =
                ClearCodecBitmapStream::decode(&mut ironrdp_core::ReadCursor::new(&data)).unwrap();
            stats.observe(&stream, w, h, Some(decoded.len()));
        }
        assert_eq!(stats.stores, 2);
        assert_eq!(stats.hits, 3);
        assert_eq!(stats.successful_hits, 3);
        assert_eq!(stats.missing_stores, 0);
        assert_eq!(stats.shape_changes, 1);
        assert_eq!(stats.aspect_changes, 1);
        assert_eq!(stats.legacy_oob_hits, 1);
        assert_eq!(stats.slots.get(&0), Some(&(1, 4, 16)));
    }

    struct Directory(std::path::PathBuf);

    impl Directory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "adit-clear-manifest-{}-{}-{sequence}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }

        fn write(&self, index: u32, metadata: &str) {
            // Public parser fixtures, not a captured or valid codec stream.
            std::fs::write(self.0.join(format!("clear-{index:03}.bin")), [1]).unwrap();
            std::fs::write(self.0.join(format!("clear-{index:03}.txt")), metadata).unwrap();
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            // Only this freshly-created flat fixture directory belongs to us.
            if let Ok(entries) = std::fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    const OK: &str = "outcome=pending\nhelper_pid=7\nrect=1x1 at 0,0\nbytes=1\noutcome=ok\n";

    #[test]
    fn a_completed_outcome_replaces_pending_on_the_same_index() {
        let directory = Directory::new();
        directory.write(0, OK);
        directory.write(1, OK);
        let captures = load_captures(&directory.0).unwrap();
        assert_eq!(captures.len(), 2);
        assert_eq!(capture_status(&captures, false), Ok((0, 0)));
    }

    #[test]
    fn missing_cache_prefix_truncated_bytes_and_mixed_connections_are_rejected() {
        let directory = Directory::new();
        directory.write(1, OK);
        assert!(load_captures(&directory.0)
            .unwrap_err()
            .contains("cache prefix"));
        directory.write(0, &OK.replace("bytes=1", "bytes=2"));
        assert!(load_captures(&directory.0)
            .unwrap_err()
            .contains("metadata"));
        directory.write(0, OK);
        directory.write(1, &OK.replace("helper_pid=7", "helper_pid=8"));
        assert!(load_captures(&directory.0)
            .unwrap_err()
            .contains("different helper"));
    }

    #[test]
    fn pending_requires_explicit_forensic_override_and_does_not_become_live_success() {
        let directory = Directory::new();
        directory.write(
            0,
            "outcome=pending\nhelper_pid=7\nrect=1x1 at 0,0\nbytes=1\n",
        );
        let captures = load_captures(&directory.0).unwrap();
        assert!(capture_status(&captures, false).is_err());
        assert_eq!(capture_status(&captures, true), Ok((1, 0)));
    }
}
