//! Real, isolated SSH -> loopback-only VNC integration. Start tests/vnc first.
//! Credentials here are disposable fixture constants, never real host secrets.
use adit_ssh::{
    desktop_gateway, HostKeyDecision, LiveShellCommand, LiveShellEvent, LiveShellRequest, SshError,
};
use std::{sync::mpsc, time::Duration};
use tokio::sync::mpsc as tmpsc;
use vnc::{VncConnector, VncEncoding, VncEvent};

async fn gateway(
    password: &str,
    decision: HostKeyDecision,
) -> Result<desktop_gateway::GatewayStream, SshError> {
    let mut request = LiveShellRequest::new("127.0.0.1", 12222, "adit-test", password);
    request.known_hosts_path = std::env::temp_dir().join(format!(
        "adit-gateway-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    request.connect_timeout_secs = 5;
    let (tx, rx) = tmpsc::unbounded_channel();
    let (events, receiver) = mpsc::channel();
    let connect = desktop_gateway::connect(request, "127.0.0.1".into(), 15900, rx, events);
    tokio::pin!(connect);
    let mut prompted = false;
    loop {
        tokio::select! {
            result = &mut connect => { assert!(prompted, "unknown gateway must prompt even with terminal TOFU enabled"); return result; }
            _ = tokio::time::sleep(Duration::from_millis(5)) => for event in receiver.try_iter() {
                if matches!(event, LiveShellEvent::HostKeyPrompt(_)) {
                    prompted = true;
                    tx.send(LiveShellCommand::HostKeyDecision(decision)).unwrap();
                }
            }
        }
    }
}

#[tokio::test]
#[ignore = "needs isolated Docker fixture tests/vnc with host ports 12222 and 15901"]
async fn ssh_direct_tcpip_reaches_an_unpublished_vnc_port_and_checks_both_identities() {
    // Only 15901 is published. The VNC target 15900 is reachable via SSH only.
    assert!(tokio::net::TcpStream::connect("127.0.0.1:15900")
        .await
        .is_err());
    let stream = gateway("adit-test-isolated", HostKeyDecision::TrustOnce)
        .await
        .unwrap();
    let client = VncConnector::new(stream)
        .set_auth_method(async { Ok("adit-ci".to_owned()) })
        .add_encoding(VncEncoding::Tight)
        .add_encoding(VncEncoding::Raw)
        .set_pixel_format(vnc::PixelFormat::rgba())
        .allow_shared(true)
        .build()
        .unwrap()
        .try_start()
        .await
        .unwrap()
        .finish()
        .unwrap();
    let paint = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.recv_event().await.unwrap() {
                VncEvent::RawImage(rect, pixels) if rect.width > 0 && !pixels.is_empty() => break,
                VncEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await;
    assert!(paint.is_ok(), "SSH tunneled VNC did not paint");
    client.close().await.unwrap();
    assert!(matches!(
        gateway("wrong-ssh-password", HostKeyDecision::TrustOnce).await,
        Err(SshError::AuthenticationRejected)
    ));
    assert!(matches!(
        gateway("adit-test-isolated", HostKeyDecision::Reject).await,
        Err(SshError::Russh(_)) | Err(SshError::HostKeyRejected)
    ));
}
