//! Bounded Sixel decoding. Unknown DCS strings never enter this decoder.

pub(crate) const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_SIDE: usize = 4096;
const MAX_PIXELS: usize = 4 * 1024 * 1024;
const MAX_WORK: usize = 32 * 1024 * 1024;

pub(crate) struct Raster {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// Decode twice: first determine the canvas, then paint it. Neither a giant
/// repeat nor a raster declaration may allocate or iterate before validation.
pub(crate) fn decode(bytes: &[u8], macro_parameter: u16) -> Option<Raster> {
    if bytes.len() > MAX_BYTES {
        return None;
    }
    let aspect = match macro_parameter {
        2 => 5,
        3 | 4 => 3,
        7..=9 => 1,
        _ => 2,
    };
    let (width, height) = walk(bytes, None, aspect)?;
    if width == 0
        || height == 0
        || width > MAX_SIDE
        || height > MAX_SIDE
        || width.checked_mul(height)? > MAX_PIXELS
    {
        return None;
    }
    let mut raster = Raster {
        width,
        height,
        pixels: vec![0; width * height * 4],
    };
    // For opaque DCS, the grid replaces the underlying cells with the current
    // background. Resolving Default here to black would break light themes.
    walk(bytes, Some(&mut raster), aspect)?;
    Some(raster)
}

fn number(bytes: &[u8], pos: &mut usize) -> Option<usize> {
    let mut n = 0usize;
    while let Some(b'0'..=b'9') = bytes.get(*pos) {
        n = n
            .checked_mul(10)?
            .checked_add(usize::from(bytes[*pos] - b'0'))?;
        *pos += 1;
    }
    Some(n)
}

fn parameters(bytes: &[u8], pos: &mut usize) -> Option<Vec<usize>> {
    let mut values = Vec::new();
    loop {
        if values.len() == 5 {
            return None;
        }
        values.push(number(bytes, pos)?);
        if bytes.get(*pos) != Some(&b';') {
            return Some(values);
        }
        *pos += 1;
    }
}

fn walk(
    bytes: &[u8],
    mut raster: Option<&mut Raster>,
    mut aspect: usize,
) -> Option<(usize, usize)> {
    let mut palette = [[0, 0, 0, 255]; 256];
    // DEC's default 16-color register set; higher registers start at black.
    let defaults = [
        [0, 0, 0],
        [20, 20, 80],
        [80, 13, 13],
        [20, 80, 20],
        [80, 20, 80],
        [20, 80, 80],
        [80, 80, 20],
        [53, 53, 53],
        [26, 26, 26],
        [33, 33, 60],
        [60, 26, 26],
        [33, 60, 33],
        [60, 33, 60],
        [33, 60, 60],
        [60, 60, 33],
        [80, 80, 80],
    ];
    for (slot, rgb) in palette.iter_mut().zip(defaults) {
        *slot = [percent(rgb[0]), percent(rgb[1]), percent(rgb[2]), 255];
    }
    let (mut x, mut y, mut width, mut height) = (0usize, 0usize, 0usize, 0usize);
    let mut selected = 0;
    let mut work = 0usize;
    let mut pos = 0;
    while pos < bytes.len() {
        let byte = bytes[pos];
        pos += 1;
        match byte {
            b'"' => {
                let p = parameters(bytes, &mut pos)?;
                if !matches!(p.len(), 2 | 4) {
                    return None;
                }
                if let Some(ratio) = p[0].checked_div(p[1]) {
                    aspect = (ratio + usize::from(p[0] % p[1] >= p[1].div_ceil(2))).max(1);
                    if aspect > MAX_SIDE / 6 {
                        return None;
                    }
                }
                if p.len() == 4 {
                    if p[2] > MAX_SIDE || p[3] > MAX_SIDE {
                        return None;
                    }
                    width = width.max(p[2]);
                    height = height.max(p[3].checked_mul(aspect)?);
                }
            }
            b'#' => {
                let p = parameters(bytes, &mut pos)?;
                selected = *p.first()?;
                if selected >= palette.len() {
                    return None;
                }
                if p.len() > 1 {
                    if p.len() != 5 {
                        return None;
                    }
                    let rgb = match p[1] {
                        2 if p[2..].iter().all(|v| *v <= 100) => {
                            [percent(p[2]), percent(p[3]), percent(p[4])]
                        }
                        1 if p[2] <= 360 && p[3] <= 100 && p[4] <= 100 => hls(p[2], p[3], p[4]),
                        _ => return None,
                    };
                    palette[selected] = [rgb[0], rgb[1], rgb[2], 255];
                }
            }
            b'$' => x = 0,
            b'-' => {
                x = 0;
                y = y.checked_add(6 * aspect)?;
                if y >= MAX_SIDE {
                    return None;
                }
            }
            b'!' | b'?'..=b'~' => {
                let (repeat, pattern) = if byte == b'!' {
                    let repeat = number(bytes, &mut pos)?.max(1);
                    let pattern = *bytes.get(pos)?;
                    pos += 1;
                    if !(b'?'..=b'~').contains(&pattern) {
                        return None;
                    }
                    (repeat, pattern - b'?')
                } else {
                    (1, byte - b'?')
                };
                let right = x.checked_add(repeat)?;
                let bottom = y.checked_add(6 * aspect)?;
                if right > MAX_SIDE || bottom > MAX_SIDE {
                    return None;
                }
                work = work.checked_add(repeat.checked_mul(6 * aspect)?)?;
                if work > MAX_WORK {
                    return None;
                }
                width = width.max(right);
                height = height.max(bottom);
                if width.checked_mul(height)? > MAX_PIXELS {
                    return None;
                }
                if let Some(raster) = raster.as_deref_mut() {
                    for col in x..right {
                        for bit in 0..6 {
                            if pattern & (1 << bit) != 0 {
                                for dy in 0..aspect {
                                    let offset = ((y + bit * aspect + dy) * raster.width + col) * 4;
                                    raster.pixels[offset..offset + 4]
                                        .copy_from_slice(&palette[selected]);
                                }
                            }
                        }
                    }
                }
                x = right;
            }
            0x00..=0x20 => {}
            _ => return None,
        }
    }
    Some((width, height))
}

fn percent(value: usize) -> u8 {
    ((value * 255 + 50) / 100) as u8
}

fn hls(hue: usize, light: usize, saturation: usize) -> [u8; 3] {
    // DEC rotates the conventional HSL wheel: 0 blue, 120 red, 240 green.
    let h = ((hue + 240) % 360) as f64 / 60.0;
    let l = light as f64 / 100.0;
    let s = saturation as f64 / 100.0;
    let chroma = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = chroma * (1.0 - (h % 2.0 - 1.0).abs());
    let rgb = match h as usize {
        0 => [chroma, x, 0.0],
        1 => [x, chroma, 0.0],
        2 => [0.0, chroma, x],
        3 => [0.0, x, chroma],
        4 => [x, 0.0, chroma],
        _ => [chroma, 0.0, x],
    };
    rgb.map(|v| ((v + l - chroma / 2.0) * 255.0).round().clamp(0.0, 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_repeat_and_overpaint() {
        let image = decode(b"\"1;1;3;6#1;2;100;0;0!3~$#2;2;0;100;0@", 7).unwrap();
        assert_eq!((image.width, image.height), (3, 6));
        assert_eq!(&image.pixels[..8], &[0, 255, 0, 255, 255, 0, 0, 255]);
        assert_eq!(&image.pixels[12..16], &[255, 0, 0, 255]);
    }

    #[test]
    fn rejects_expansion_and_number_overflow() {
        for input in [
            b"!9999999999999999999999999999~".as_slice(),
            b"!4097~",
            b"\"1;1;4096;4096~",
            b"#256~",
            b"#1;2;101;0;0~",
            b"!2x!",
        ] {
            assert!(decode(input, 7).is_none(), "{input:?}");
        }
    }

    #[test]
    fn transparent_background_and_hls() {
        let image = decode(b"#1;1;120;50;100@", 7).unwrap();
        assert_eq!(&image.pixels[..8], &[255, 0, 0, 255, 0, 0, 0, 0]);
        assert_eq!(&decode(b"?", 7).unwrap().pixels[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn macro_and_raster_pixel_aspect_are_bounded_and_applied() {
        assert_eq!(decode(b"@", 0).unwrap().height, 12);
        assert_eq!(decode(b"@", 2).unwrap().height, 30);
        let image = decode(b"\"3;2#1;2;100;0;0@", 7).unwrap();
        assert_eq!(image.height, 12);
        assert_eq!(&image.pixels[..8], &[255, 0, 0, 255, 255, 0, 0, 255]);
        assert!(decode(b"\"999999;1~", 7).is_none());
    }
}
