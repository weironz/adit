//! Real wire/crypto peers, with in-memory transports and synthetic credentials.
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{duplex, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use vnc::{
    PixelFormat, SecurityPolicy, TlsConfig, VncConnector, VncEncoding, VncError, VncEvent, X11Event,
};

async fn header<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, types: &[u8]) {
    s.write_all(b"RFB 003.008\n").await.unwrap();
    let mut v = [0; 12];
    s.read_exact(&mut v).await.unwrap();
    assert_eq!(&v, b"RFB 003.008\n");
    s.write_u8(types.len() as u8).await.unwrap();
    s.write_all(types).await.unwrap();
}
async fn server_init<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S) {
    s.write_u32(0).await.unwrap();
    assert_eq!(s.read_u8().await.unwrap(), 1);
    s.write_all(&[
        0, 2, 0, 2, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0, 0, 0, 0, 4,
    ])
    .await
    .unwrap();
    s.write_all(b"test").await.unwrap();
}
async fn client_message<S: AsyncRead + Unpin>(s: &mut S) -> (u8, Vec<u8>) {
    let tag = s.read_u8().await.unwrap();
    let n = match tag {
        0 => 19,
        2 => {
            s.read_u8().await.unwrap();
            s.read_u16().await.unwrap() as usize * 4
        }
        3 => 9,
        4 => 7,
        5 => 5,
        6 => {
            let mut padding = [0; 3];
            s.read_exact(&mut padding).await.unwrap();
            s.read_i32().await.unwrap().unsigned_abs() as usize
        }
        248 => {
            let mut header = [0; 7];
            s.read_exact(&mut header).await.unwrap();
            s.read_u8().await.unwrap() as usize
        }
        _ => panic!("bad tag {tag}"),
    };
    assert!(n < 2_000_000);
    let mut bytes = vec![0; n];
    s.read_exact(&mut bytes).await.unwrap();
    (tag, bytes)
}

#[tokio::test]
async fn policy_prefers_password_and_does_not_silently_allow_none() {
    for policy in [SecurityPolicy::RequireVncAuth, SecurityPolicy::AllowNone] {
        let (stream, mut server) = duplex(1024);
        let peer = tokio::spawn(async move {
            header(&mut server, &[1, 2]).await;
            assert_eq!(server.read_u8().await.unwrap(), 2);
            server.write_all(&[7; 16]).await.unwrap();
            let mut response = [0; 16];
            server.read_exact(&mut response).await.unwrap();
            server.write_u32(1).await.unwrap();
            server.write_u32(6).await.unwrap();
            server.write_all(b"denied").await.unwrap();
        });
        let result = VncConnector::new(stream)
            .security_policy(policy)
            .set_auth_method(async { Ok("testpass".into()) })
            .add_encoding(VncEncoding::Raw)
            .build()
            .unwrap()
            .try_start()
            .await;
        assert!(result.is_err());
        peer.await.unwrap();
    }
    for policy in [SecurityPolicy::RequireVncAuth, SecurityPolicy::RequireTls] {
        let (stream, mut server) = duplex(1024);
        let peer = tokio::spawn(async move {
            header(&mut server, &[1]).await;
            let mut rest = vec![];
            server.read_to_end(&mut rest).await.unwrap();
            assert!(rest.is_empty());
        });
        let result = VncConnector::new(stream)
            .set_auth_method(async { Ok("synthetic-test-password".into()) })
            .security_policy(policy)
            .add_encoding(VncEncoding::Raw)
            .build()
            .unwrap()
            .try_start()
            .await;
        assert!(matches!(result, Err(VncError::SecurityPolicy(_))));
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn complete_update_releases_exactly_one_outstanding_request() {
    let (stream, mut server) = duplex(8192);
    let (step_tx, mut step_rx) = tokio::sync::mpsc::channel(1);
    let peer = tokio::spawn(async move {
        header(&mut server, &[1]).await;
        assert_eq!(server.read_u8().await.unwrap(), 1);
        server_init(&mut server).await;
        while client_message(&mut server).await.0 != 3 {}
        step_tx.send(()).await.unwrap();
        // Client repeatedly asks refresh while initial paint is pending.
        assert!(
            tokio::time::timeout(Duration::from_millis(180), server.read_u8())
                .await
                .is_err()
        );
        // Raw + CopyRect + LastRect in a single update, then a zero-rect update.
        let mut update = vec![0, 0, 0, 3];
        for v in [0_u16, 0, 2, 2] {
            update.extend(v.to_be_bytes());
        }
        update.extend(0_i32.to_be_bytes());
        update.extend([5, 6, 7, 0].repeat(4));
        for v in [0_u16, 0, 1, 1] {
            update.extend(v.to_be_bytes());
        }
        update.extend(1_i32.to_be_bytes());
        update.extend([0, 1, 0, 1]);
        for v in [0_u16, 0, 0, 0] {
            update.extend(v.to_be_bytes());
        }
        update.extend((-224_i32).to_be_bytes());
        server.write_all(&update).await.unwrap();
        while client_message(&mut server).await.0 != 3 {}
        server.write_all(&[0, 0, 0, 0]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let client = VncConnector::new(stream)
        .set_auth_method(async { Ok("synthetic-test-password".into()) })
        .security_policy(SecurityPolicy::AllowNone)
        .set_pixel_format(PixelFormat::rgba())
        .add_encoding(VncEncoding::Raw)
        .add_encoding(VncEncoding::CopyRect)
        .add_encoding(VncEncoding::LastRectPseudo)
        .build()
        .unwrap()
        .try_start()
        .await
        .unwrap()
        .finish()
        .unwrap();
    step_rx.recv().await.unwrap();
    assert_eq!(client.pending_updates(), 1);
    for _ in 0..1000 {
        client.input(X11Event::Refresh).await.unwrap();
    }
    let mut completed = 0;
    while completed < 2 {
        let event = tokio::time::timeout(Duration::from_secs(2), client.recv_event())
            .await
            .unwrap()
            .unwrap();
        if let VncEvent::FramebufferUpdateComplete(stats) = event {
            completed += 1;
            assert_eq!(stats.sequence, completed);
            assert_eq!(client.pending_updates(), 0);
            if completed == 1 {
                assert!(stats.wire_bytes > 32);
                assert_eq!(stats.rectangles, 3);
                client.input(X11Event::Refresh).await.unwrap();
            } else {
                assert_eq!(stats.rectangles, 0);
            }
        }
    }
    client.close().await.unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn vencrypt_validates_target_identity_and_expiry_before_credentials() {
    for (name, trust, expired, should_pass) in [
        ("localhost", true, false, true),
        ("wrong.example", true, false, false),
        ("localhost", false, false, false),
        ("localhost", true, true, false),
    ] {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(if expired { 2021 } else { 2040 }, 1, 1);
        let cert = params.self_signed(&key).unwrap();
        let der = cert.der().to_vec();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        let (stream, mut server) = duplex(16_384);
        let peer = tokio::spawn(async move {
            header(&mut server, &[19]).await;
            assert_eq!(server.read_u8().await.unwrap(), 19);
            server.write_all(&[0, 2]).await.unwrap();
            assert_eq!(server.read_u16().await.unwrap(), 2);
            server.write_all(&[0, 1]).await.unwrap();
            server.write_u32(260).await.unwrap();
            assert_eq!(server.read_u32().await.unwrap(), 260);
            server.write_u8(1).await.unwrap();
            let tls = tokio_rustls::TlsAcceptor::from(Arc::new(config))
                .accept(server)
                .await;
            if should_pass {
                let mut tls = tls.unwrap();
                server_init(&mut tls).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            } else {
                assert!(tls.is_err());
            }
        });
        let config = TlsConfig {
            server_name: name.into(),
            ca_certificates_der: if trust { vec![der] } else { vec![] },
        };
        let result = VncConnector::new(stream)
            .security_policy(SecurityPolicy::RequireTls)
            .tls_config(config)
            .set_auth_method(async { panic!("X509None must not request a password") })
            .set_pixel_format(PixelFormat::rgba())
            .add_encoding(VncEncoding::Raw)
            .build()
            .unwrap()
            .try_start()
            .await;
        if should_pass {
            let client = result.unwrap().finish().unwrap();
            assert_eq!(client.security(), vnc::SecurityInfo::X509None);
            client.close().await.unwrap();
        } else {
            assert!(matches!(result, Err(VncError::Tls(_))));
        }
        peer.await.unwrap();
    }
}
