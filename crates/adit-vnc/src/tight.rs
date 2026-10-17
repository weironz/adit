//! Tight envelope framing, independently implemented from published wire layouts.
//! It carries capabilities; it is not encrypted transport.
use crate::{SecurityInfo, SecurityPolicy, VncError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
const MAX_CAPABILITIES: usize = 1024;
pub(crate) const CUT_TEXT: u32 = 0xfc00_0200;
pub(crate) const ENABLE_UTF8: u32 = 0xfc00_0201;
struct Capability {
    code: u32,
    vendor: [u8; 4],
    signature: [u8; 8],
}
impl Capability {
    fn matches(&self, code: u32, vendor: &[u8; 4], signature: &[u8; 8]) -> bool {
        self.code == code && &self.vendor == vendor && &self.signature == signature
    }
}
async fn capabilities<S: AsyncRead + Unpin>(
    stream: &mut S,
    count: usize,
) -> Result<Vec<Capability>, VncError> {
    if count > MAX_CAPABILITIES {
        return Err(VncError::SecurityPolicy(
            "Tight capability limit exceeded".into(),
        ));
    }
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        let mut bytes = [0; 16];
        stream.read_exact(&mut bytes).await?;
        let cap = Capability {
            code: u32::from_be_bytes(bytes[..4].try_into().unwrap()),
            vendor: bytes[4..8].try_into().unwrap(),
            signature: bytes[8..].try_into().unwrap(),
        };
        // A numeric message identifier cannot have two competing meanings in one direction.
        if result.iter().any(|previous: &Capability| {
            previous.code == cap.code
                && (previous.vendor != cap.vendor || previous.signature != cap.signature)
        }) {
            return Err(VncError::SecurityPolicy(
                "Conflicting Tight capability identifiers".into(),
            ));
        }
        result.push(cap);
    }
    Ok(result)
}
pub(crate) async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    policy: SecurityPolicy,
) -> Result<SecurityInfo, VncError> {
    let count = stream.read_u32().await? as usize;
    let tunnels = capabilities(stream, count).await?;
    if count > 0 {
        if !tunnels.iter().any(|c| c.matches(0, b"TGHT", b"NOTUNNEL")) {
            return Err(VncError::SecurityPolicy(
                "No supported Tight tunnel capability".into(),
            ));
        }
        stream.write_u32(0).await?;
    }
    let count = stream.read_u32().await? as usize;
    let auth = capabilities(stream, count).await?;
    let selection = if auth.iter().any(|c| c.matches(2, b"STDV", b"VNCAUTH_")) {
        (2, SecurityInfo::VncAuth)
    } else if policy == SecurityPolicy::AllowNone
        && (count == 0 || auth.iter().any(|c| c.matches(1, b"STDV", b"NOAUTH__")))
    {
        (1, SecurityInfo::None)
    } else {
        return Err(VncError::SecurityPolicy(
            "Tight envelope does not offer required VNC password authentication".into(),
        ));
    };
    if count > 0 {
        stream.write_u32(selection.0).await?;
    }
    Ok(selection.1)
}
pub(crate) async fn interaction<S: AsyncRead + Unpin>(stream: &mut S) -> Result<bool, VncError> {
    let server = stream.read_u16().await? as usize;
    let client = stream.read_u16().await? as usize;
    let encoding = stream.read_u16().await? as usize;
    let _padding = stream.read_u16().await?;
    // Check every count before waiting for any list payload.
    if [server, client, encoding]
        .iter()
        .any(|n| *n > MAX_CAPABILITIES)
    {
        return Err(VncError::SecurityPolicy(
            "Tight interaction capability limit exceeded".into(),
        ));
    }
    let server = capabilities(stream, server).await?;
    let client = capabilities(stream, client).await?;
    let _encodings = capabilities(stream, encoding).await?;
    Ok(server
        .iter()
        .any(|c| c.matches(CUT_TEXT, b"TGHT", b"UTF8CUTS"))
        && client
            .iter()
            .any(|c| c.matches(CUT_TEXT, b"TGHT", b"UTF8CUTC"))
        && client
            .iter()
            .any(|c| c.matches(ENABLE_UTF8, b"TGHT", b"UTF8CUTE")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;
    fn cap(code: u32, vendor: &[u8; 4], signature: &[u8; 8]) -> Vec<u8> {
        [code.to_be_bytes().as_slice(), vendor, signature].concat()
    }
    #[tokio::test]
    async fn nonzero_tunnel_list_chooses_only_exact_no_tunnel_then_authenticates() {
        let (mut stream, mut peer) = duplex(1024);
        peer.write_u32(2).await.unwrap();
        peer.write_all(&cap(9, b"FAKE", b"OTHER___")).await.unwrap();
        peer.write_all(&cap(0, b"TGHT", b"NOTUNNEL")).await.unwrap();
        peer.write_u32(1).await.unwrap();
        peer.write_all(&cap(2, b"STDV", b"VNCAUTH_")).await.unwrap();
        assert_eq!(
            authenticate(&mut stream, SecurityPolicy::RequireVncAuth)
                .await
                .unwrap(),
            SecurityInfo::VncAuth
        );
        assert_eq!(peer.read_u32().await.unwrap(), 0);
        assert_eq!(peer.read_u32().await.unwrap(), 2);
    }
    #[tokio::test]
    async fn none_requires_explicit_policy_and_tunnel_name_must_match() {
        for auth_count in [0, 1] {
            let (mut stream, mut peer) = duplex(1024);
            peer.write_u32(0).await.unwrap();
            peer.write_u32(auth_count).await.unwrap();
            if auth_count == 1 {
                peer.write_all(&cap(1, b"STDV", b"NOAUTH__")).await.unwrap();
            }
            assert_eq!(
                authenticate(&mut stream, SecurityPolicy::AllowNone)
                    .await
                    .unwrap(),
                SecurityInfo::None
            );
            if auth_count == 1 {
                assert_eq!(peer.read_u32().await.unwrap(), 1);
            }
        }
        let (mut stream, mut peer) = duplex(1024);
        peer.write_u32(1).await.unwrap();
        peer.write_all(&cap(0, b"FAKE", b"NOTUNNEL")).await.unwrap();
        assert!(matches!(
            authenticate(&mut stream, SecurityPolicy::RequireVncAuth).await,
            Err(VncError::SecurityPolicy(_))
        ));
    }
    #[tokio::test]
    async fn every_interaction_list_is_bounded_before_waiting_for_records() {
        for group in 0..3 {
            let (mut stream, mut peer) = duplex(1024);
            for i in 0..4 {
                peer.write_u16(if i == group { 1025 } else { 0 })
                    .await
                    .unwrap();
            }
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                interaction(&mut stream),
            )
            .await
            .unwrap();
            assert!(matches!(result, Err(VncError::SecurityPolicy(_))));
        }
        // A truncated capability cannot be accepted as a partial advertisement.
        let mut bytes = &[0, 1, 0, 0, 0, 0, 0, 0, 0xfc, 0, 2, 0, b'T', b'G'][..];
        assert!(matches!(
            interaction(&mut bytes).await,
            Err(VncError::IoError(_))
        ));
    }
}
