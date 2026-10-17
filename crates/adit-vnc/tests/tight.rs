//! Independently implemented synthetic peers for Tight capability framing.
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};
use vnc::{
    PixelFormat, SecurityInfo, SecurityPolicy, VncConnector, VncEncoding, VncError, VncEvent,
    X11Event,
};
const CUT: u32 = 0xfc00_0200;
const ENABLE: u32 = 0xfc00_0201;
fn cap(code: u32, vendor: &[u8; 4], signature: &[u8; 8]) -> Vec<u8> {
    [code.to_be_bytes().as_slice(), vendor, signature].concat()
}
async fn header(s: &mut DuplexStream, offers: &[u8]) -> u8 {
    s.write_all(b"RFB 003.008\n").await.unwrap();
    let mut version = [0; 12];
    s.read_exact(&mut version).await.unwrap();
    assert_eq!(&version, b"RFB 003.008\n");
    s.write_u8(offers.len() as u8).await.unwrap();
    s.write_all(offers).await.unwrap();
    s.read_u8().await.unwrap()
}
async fn auth(s: &mut DuplexStream) {
    s.write_u32(0).await.unwrap(); // no tunneling
    s.write_u32(2).await.unwrap();
    s.write_all(&cap(1, b"STDV", b"NOAUTH__")).await.unwrap();
    s.write_all(&cap(2, b"STDV", b"VNCAUTH_")).await.unwrap();
    assert_eq!(
        s.read_u32().await.unwrap(),
        2,
        "Tight must preserve password policy"
    );
    s.write_all(&[17; 16]).await.unwrap();
    let mut response = [0; 16];
    s.read_exact(&mut response).await.unwrap();
    assert_ne!(response, [0; 16]);
}
async fn init(s: &mut DuplexStream, server: &[Vec<u8>], client: &[Vec<u8>]) {
    s.write_u32(0).await.unwrap(); // SecurityResult
    assert_eq!(s.read_u8().await.unwrap(), 1);
    s.write_u16(2).await.unwrap();
    s.write_u16(2).await.unwrap();
    s.write_all(&Vec::from(PixelFormat::rgba())).await.unwrap();
    s.write_u32(4).await.unwrap();
    s.write_all(b"test").await.unwrap();
    for count in [server.len(), client.len(), 0, 0] {
        s.write_u16(count as u16).await.unwrap();
    }
    for c in server.iter().chain(client) {
        s.write_all(c).await.unwrap();
    }
}
fn caps() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    (
        vec![cap(CUT, b"TGHT", b"UTF8CUTS")],
        vec![
            cap(CUT, b"TGHT", b"UTF8CUTC"),
            cap(ENABLE, b"TGHT", b"UTF8CUTE"),
        ],
    )
}
async fn setup_messages(s: &mut DuplexStream, custom_pixel: bool, enabled: bool) {
    if custom_pixel {
        let mut bytes = [0; 20];
        s.read_exact(&mut bytes).await.unwrap();
        assert_eq!(bytes[0], 0);
    }
    if enabled {
        assert_eq!(s.read_u32().await.unwrap(), ENABLE);
    }
    assert_eq!(
        s.read_u8().await.unwrap(),
        2,
        "no unconfirmed vendor enable may be emitted"
    );
    s.read_u8().await.unwrap();
    let n = s.read_u16().await.unwrap();
    for _ in 0..n {
        s.read_u32().await.unwrap();
    }
    let mut refresh = [0; 10];
    s.read_exact(&mut refresh).await.unwrap();
    assert_eq!(refresh[0], 3);
}
async fn event(client: &vnc::VncClient) -> VncEvent {
    tokio::time::timeout(Duration::from_secs(2), client.recv_event())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn tight_password_capabilities_and_bidirectional_utf8_preserve_permissions() {
    for custom_pixel in [true, false] {
        let (stream, mut server) = duplex(8192);
        let (release, done) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            assert_eq!(header(&mut server, &[2, 16, 1]).await, 16);
            auth(&mut server).await;
            let (s, c) = caps();
            init(&mut server, &s, &c).await;
            setup_messages(&mut server, custom_pixel, true).await;
            for expected in ["café 中文🙂\r\nnext", ""] {
                assert_eq!(server.read_u32().await.unwrap(), CUT);
                let n = server.read_u32().await.unwrap() as usize;
                let mut body = vec![0; n];
                server.read_exact(&mut body).await.unwrap();
                assert_eq!(String::from_utf8(body).unwrap(), expected);
                server.write_u32(CUT).await.unwrap();
                server.write_u32(expected.len() as u32).await.unwrap();
                server.write_all(expected.as_bytes()).await.unwrap();
            }
            // Third incoming text stays queued until permissions have been revoked.
            server.write_u32(CUT).await.unwrap();
            server.write_u32(5).await.unwrap();
            server.write_all(b"stale").await.unwrap();
            let _ = done.await;
        });
        let builder = VncConnector::new(stream)
            .set_auth_method(async { Ok("synthetic".into()) })
            .add_encoding(VncEncoding::Raw);
        let builder = if custom_pixel {
            builder.set_pixel_format(PixelFormat::rgba())
        } else {
            builder
        };
        let client = builder
            .build()
            .unwrap()
            .try_start()
            .await
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(client.security(), SecurityInfo::VncAuth);
        loop {
            if matches!(
                event(&client).await,
                VncEvent::ClipboardCapabilities { utf8: true }
            ) {
                break;
            }
        }
        for text in ["café 中文🙂\nnext", ""] {
            client.input(X11Event::CopyText(text.into())).await.unwrap();
            client.flush_input().await.unwrap();
            loop {
                if let VncEvent::Text(receipt) = event(&client).await {
                    assert_eq!(receipt.into_current_text().as_deref(), Some(text));
                    break;
                }
            }
        }
        let receipt = loop {
            if let VncEvent::Text(receipt) = event(&client).await {
                break receipt;
            }
        };
        client.set_clipboard_permissions(false, false);
        client.set_clipboard_permissions(true, true);
        assert!(
            receipt.into_current_text().is_none(),
            "queued vendor text must retain its receive epoch"
        );
        assert!(matches!(
            client.input(X11Event::CopyText("bad\0text".into())).await,
            Err(VncError::ClipboardUnsupported)
        ));
        assert!(matches!(
            client
                .input(X11Event::CopyText("x".repeat(1_048_577)))
                .await,
            Err(VncError::ClipboardUnsupported)
        ));
        client.close().await.unwrap();
        let _ = release.send(());
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn vendor_capability_identity_direction_and_legacy_switch_are_required() {
    for case in 0..6 {
        let (stream, mut server) = duplex(8192);
        let (release, done) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            assert_eq!(header(&mut server, &[16]).await, 16);
            auth(&mut server).await;
            let (mut s, mut c) = caps();
            match case {
                0 => s[0][4] = b'X',
                1 => c[0][8] = b'X',
                2 => {
                    c.pop();
                }
                3 => {
                    s.clear();
                }
                4 => {
                    c.swap(0, 1);
                    s[0] = cap(CUT, b"TGHT", b"UTF8CUTC");
                }
                _ => {}
            }
            init(&mut server, &s, &c).await;
            setup_messages(&mut server, true, false).await;
            // Ordinary CutText remains exactly Latin-1. None of these peers may
            // see the four-byte Tight vendor prefix or unsolicited enable code.
            assert_eq!(server.read_u8().await.unwrap(), 6);
            let mut padding = [0; 3];
            server.read_exact(&mut padding).await.unwrap();
            assert_eq!(server.read_u32().await.unwrap(), 4);
            let mut body = [0; 4];
            server.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"caf\xe9");
            let _ = done.await;
        });
        let client = VncConnector::new(stream)
            .set_auth_method(async { Ok("synthetic".into()) })
            .utf8_clipboard(case != 5)
            .add_encoding(VncEncoding::Raw)
            .add_encoding(VncEncoding::ExtendedClipboard)
            .set_pixel_format(PixelFormat::rgba())
            .build()
            .unwrap()
            .try_start()
            .await
            .unwrap()
            .finish()
            .unwrap();
        client
            .input(X11Event::CopyText("café".into()))
            .await
            .unwrap();
        client.flush_input().await.unwrap();
        assert!(matches!(
            client.input(X11Event::CopyText("中文".into())).await,
            Err(VncError::ClipboardUnsupported)
        ));
        client.close().await.unwrap();
        let _ = release.send(());
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn standard_vnc_and_tls_policy_never_enter_unoffered_tight() {
    for tls in [false, true] {
        let (stream, mut server) = duplex(1024);
        let peer = tokio::spawn(async move {
            let selected = header(&mut server, if tls { &[16, 19, 2] } else { &[2] }).await;
            assert_eq!(selected, if tls { 19 } else { 2 });
            if tls {
                server.write_all(&[0, 2]).await.unwrap();
                let mut client_version = [0; 2];
                server.read_exact(&mut client_version).await.unwrap();
                assert_eq!(client_version, [0, 2]);
                server.write_u8(1).await.unwrap();
            } else {
                server.write_all(&[1; 16]).await.unwrap();
                let mut response = [0; 16];
                server.read_exact(&mut response).await.unwrap();
                server.write_u32(1).await.unwrap();
                server.write_u32(0).await.unwrap();
            }
        });
        let result = VncConnector::new(stream)
            .set_auth_method(async { Ok("synthetic".into()) })
            .security_policy(if tls {
                SecurityPolicy::RequireTls
            } else {
                SecurityPolicy::RequireVncAuth
            })
            .tls_config(vnc::TlsConfig {
                server_name: "localhost".into(),
                ca_certificates_der: vec![],
            })
            .add_encoding(VncEncoding::Raw)
            .build()
            .unwrap()
            .try_start()
            .await;
        assert!(result.is_err());
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn tight_authentication_rejects_none_spoofed_caps_and_oversized_lists_before_password() {
    for case in 0..7 {
        let (stream, mut server) = duplex(8192);
        let polled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&polled);
        let peer = tokio::spawn(async move {
            assert_eq!(header(&mut server, &[16]).await, 16);
            if case == 0 {
                server.write_u32(1025).await.unwrap();
            } else {
                server.write_u32(0).await.unwrap();
                if case == 1 {
                    server.write_u32(1025).await.unwrap();
                } else if case == 2 {
                    server.write_u32(0).await.unwrap();
                } else {
                    server
                        .write_u32(if case == 6 { 2 } else { 1 })
                        .await
                        .unwrap();
                    server
                        .write_all(&match case {
                            3 => cap(1, b"STDV", b"NOAUTH__"),
                            4 => cap(2, b"FAKE", b"VNCAUTH_"),
                            5 => cap(2, b"STDV", b"FAKEAUTH"),
                            _ => cap(2, b"STDV", b"VNCAUTH_"),
                        })
                        .await
                        .unwrap();
                    if case == 6 {
                        server
                            .write_all(&cap(2, b"FAKE", b"VNCAUTH_"))
                            .await
                            .unwrap();
                    }
                }
            }
            let mut sent = Vec::new();
            server.read_to_end(&mut sent).await.unwrap();
            assert!(
                sent.is_empty(),
                "no auth choice, response or ClientInit on rejected envelope"
            );
        });
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            VncConnector::new(stream)
                .set_auth_method(async move {
                    polled.store(true, Ordering::Release);
                    Ok("synthetic".into())
                })
                .add_encoding(VncEncoding::Raw)
                .build()
                .unwrap()
                .try_start(),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(VncError::SecurityPolicy(_))));
        assert!(!observed.load(Ordering::Acquire));
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn tight_vendor_payloads_are_bounded_and_strict_utf8_without_nul() {
    for body in [vec![0xff], b"x\0y".to_vec(), vec![]] {
        let (stream, mut server) = duplex(8192);
        let (release, done) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            assert_eq!(header(&mut server, &[16]).await, 16);
            auth(&mut server).await;
            let (s, c) = caps();
            init(&mut server, &s, &c).await;
            setup_messages(&mut server, true, true).await;
            server.write_u32(CUT).await.unwrap();
            if body.is_empty() {
                server.write_u32(1_048_577).await.unwrap();
            } else {
                server.write_u32(body.len() as u32).await.unwrap();
                server.write_all(&body).await.unwrap();
            }
            let _ = done.await;
        });
        let client = VncConnector::new(stream)
            .set_auth_method(async { Ok("synthetic".into()) })
            .add_encoding(VncEncoding::Raw)
            .set_pixel_format(PixelFormat::rgba())
            .build()
            .unwrap()
            .try_start()
            .await
            .unwrap()
            .finish()
            .unwrap();
        loop {
            if matches!(event(&client).await, VncEvent::Error(_)) {
                break;
            }
        }
        client.close().await.unwrap();
        let _ = release.send(());
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn explicit_none_envelope_can_initialize_without_obtaining_a_password() {
    let (stream, mut server) = duplex(4096);
    let (release, done) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        assert_eq!(header(&mut server, &[16]).await, 16);
        server.write_u32(0).await.unwrap();
        server.write_u32(0).await.unwrap();
        init(&mut server, &[], &[]).await;
        setup_messages(&mut server, true, false).await;
        let _ = done.await;
    });
    let polled = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&polled);
    let client = VncConnector::new(stream)
        .security_policy(SecurityPolicy::AllowNone)
        .set_auth_method(async move {
            polled.store(true, Ordering::Release);
            Ok(String::new())
        })
        .add_encoding(VncEncoding::Raw)
        .set_pixel_format(PixelFormat::rgba())
        .build()
        .unwrap()
        .try_start()
        .await
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(client.security(), SecurityInfo::None);
    assert!(!observed.load(Ordering::Acquire));
    client.flush_input().await.unwrap();
    client.close().await.unwrap();
    let _ = release.send(());
    peer.await.unwrap();
}
