use super::*;
use crate::{PixelFormat, Rect, VncError, VncEvent};
use std::sync::Mutex;

#[tokio::test]
async fn tight_fill_copy_and_palette_modes_preserve_pixels() {
    let rect = Rect {
        x: 0,
        y: 0,
        width: 2,
        height: 1,
    };
    for (bytes, expected) in [
        (vec![0x80, 1, 2, 3], vec![1, 2, 3, 255, 1, 2, 3, 255]),
        (vec![0, 1, 2, 3, 4, 5, 6], vec![1, 2, 3, 255, 4, 5, 6, 255]),
        (
            vec![0x40, 1, 1, 1, 2, 3, 4, 5, 6, 0x40],
            vec![1, 2, 3, 255, 4, 5, 6, 255],
        ),
        (
            vec![0x40, 1, 2, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 2],
            vec![1, 2, 3, 255, 7, 8, 9, 255],
        ),
    ] {
        let output = Mutex::new(Vec::new());
        TightDecoder::new()
            .decode(
                &PixelFormat::rgba(),
                &rect,
                &mut bytes.as_slice(),
                &|event| {
                    output.lock().unwrap().push(event);
                    async { Ok(()) }
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(&output.into_inner().unwrap()[0], VncEvent::RawImage(_, pixels) if pixels == &expected)
        );
    }
}

#[tokio::test]
async fn tight_rejects_invalid_palette_indexes_and_unsupported_formats() {
    let rect = Rect {
        x: 0,
        y: 0,
        width: 2,
        height: 1,
    };
    for bytes in [
        vec![0x40, 1, 0], // a single color must use Fill
        vec![0x40, 1, 2, 1, 2, 3, 4, 5, 6, 7, 8, 9, 3, 0],
    ] {
        let result = TightDecoder::new()
            .decode(
                &PixelFormat::rgba(),
                &rect,
                &mut bytes.as_slice(),
                &|_| async { Ok(()) },
            )
            .await;
        assert!(matches!(result, Err(VncError::InvalidImageData)));
    }
    let mut format = PixelFormat::rgba();
    format.true_color_flag = 0;
    format.red_shift = 255; // ignored for indexed color, must never be shifted
    let result = TightDecoder::new()
        .decode(&format, &rect, &mut &[][..], &|_| async { Ok(()) })
        .await;
    assert!(matches!(result, Err(VncError::WrongPixelFormat)));
}

#[tokio::test]
async fn cursor_preserves_alpha_mask_and_accepts_empty_shapes() {
    for (width, bytes, expected) in [
        (1, vec![1, 2, 3, 0, 0x80], vec![1, 2, 3, 255]),
        (1, vec![1, 2, 3, 255, 0], vec![1, 2, 3, 0]),
        (0, vec![], vec![]),
    ] {
        let output = Mutex::new(Vec::new());
        CursorDecoder::new()
            .decode(
                &PixelFormat::rgba(),
                &Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 1,
                },
                &mut bytes.as_slice(),
                &|event| {
                    output.lock().unwrap().push(event);
                    async { Ok(()) }
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(&output.into_inner().unwrap()[0], VncEvent::SetCursor(_, pixels) if pixels == &expected)
        );
    }
}

#[tokio::test]
async fn cursor_rejects_size_and_hotspot_before_polling_the_payload() {
    struct NoPayloadRead;
    impl tokio::io::AsyncRead for NoPayloadRead {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            panic!("invalid cursor must be rejected before any payload read");
        }
    }

    for (width, height, x, y) in [
        (513, 1, 0, 0),
        (1, 513, 0, 0),
        (3840, 2160, 0, 0),
        (512, 512, 512, 0),
        (512, 512, 0, 512),
        (1, 1, u16::MAX, 0),
        (0, 513, 0, 0),
        (513, 0, 0, 0),
        (0, 1, u16::MAX, 1),
    ] {
        let result = CursorDecoder::new()
            .decode(
                &PixelFormat::rgba(),
                &Rect {
                    x,
                    y,
                    width,
                    height,
                },
                &mut NoPayloadRead,
                &|_| async { panic!("invalid cursor must not publish an event") },
            )
            .await;
        assert!(matches!(result, Err(VncError::InvalidImageData)));
    }
}

#[tokio::test]
async fn cursor_accepts_the_budget_boundary_and_hidden_shapes() {
    let mut bytes = [1, 2, 3, 0].repeat(512 * 512);
    bytes.extend(std::iter::repeat_n(0xff, 64 * 512));
    let output = Mutex::new(Vec::new());
    CursorDecoder::new()
        .decode(
            &PixelFormat::rgba(),
            &Rect {
                x: 511,
                y: 511,
                width: 512,
                height: 512,
            },
            &mut bytes.as_slice(),
            &|event| {
                output.lock().unwrap().push(event);
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        &output.into_inner().unwrap()[0],
        VncEvent::SetCursor(_, pixels)
            if pixels.len() == 512 * 512 * 4
                && pixels.as_chunks::<4>().0.iter().all(|p| *p == [1, 2, 3, 255])
    ));

    for (width, height, x, y) in [
        (0, 0, u16::MAX, u16::MAX),
        (0, 512, u16::MAX, 511),
        (512, 0, 511, u16::MAX),
    ] {
        let output = Mutex::new(Vec::new());
        CursorDecoder::new()
            .decode(
                &PixelFormat::rgba(),
                &Rect {
                    x,
                    y,
                    width,
                    height,
                },
                &mut &[][..],
                &|event| {
                    output.lock().unwrap().push(event);
                    async { Ok(()) }
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            &output.into_inner().unwrap()[0],
            VncEvent::SetCursor(rect, pixels)
                if rect.width == width && rect.height == height && pixels.is_empty()
        ));
    }
}

#[tokio::test]
async fn tight_rejects_component_widths_its_filters_cannot_decode() {
    let mut format = PixelFormat::rgba();
    format.red_max = 65535;
    format.red_shift = 0;
    format.green_max = 127;
    format.green_shift = 16;
    format.blue_max = 1;
    format.blue_shift = 23;
    assert!(format.validate().is_ok());
    let result = TightDecoder::new()
        .decode(
            &format,
            &Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &mut &[0x40, 2, 255, 255, 255][..],
            &|_| async { Ok(()) },
        )
        .await;
    assert!(matches!(result, Err(VncError::WrongPixelFormat)));
}

#[tokio::test]
async fn tight_gradient_filter_reconstructs_channels_and_rejects_png_control() {
    let output = Mutex::new(Vec::new());
    let rect = Rect {
        x: 0,
        y: 0,
        width: 2,
        height: 1,
    };
    TightDecoder::new()
        .decode(
            &PixelFormat::rgba(),
            &rect,
            &mut &[0x40, 2, 10, 20, 30, 5, 15, 25][..],
            &|event| {
                output.lock().unwrap().push(event);
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
    let events = output.into_inner().unwrap();
    assert!(matches!(&events[0],VncEvent::RawImage(_,rgba) if rgba==&[10,20,30,0,15,35,55,0]));
    assert!(matches!(
        TightDecoder::new()
            .decode(&PixelFormat::rgba(), &rect, &mut &[0xa0][..], &|_| async {
                Ok(())
            })
            .await,
        Err(VncError::InvalidImageData)
    ));
}
