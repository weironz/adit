//! Authentication policy and verified TLS transport. No certificate bypass exists.
use crate::VncError;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SecurityPolicy {
    #[default]
    RequireVncAuth,
    /// Permit unauthenticated desktop access only when deliberately configured.
    AllowNone,
    /// Require X.509 identity verification before any password is obtained.
    RequireTls,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityInfo {
    None,
    VncAuth,
    X509None,
    X509Vnc,
}
#[derive(Clone, Debug)]
pub struct TlsConfig {
    pub server_name: String,
    /// Target-scoped trust anchors. Empty uses public WebPKI roots.
    pub ca_certificates_der: Vec<Vec<u8>>,
}
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> Io for T {}
pub(crate) type Stream = Box<dyn Io>;

pub(crate) async fn vencrypt(
    mut stream: Stream,
    config: TlsConfig,
) -> Result<(Stream, SecurityInfo), VncError> {
    let major = stream.read_u8().await?;
    let minor = stream.read_u8().await?;
    if major != 0 || minor < 2 {
        return Err(VncError::SecurityPolicy("VeNCrypt 0.2 required".into()));
    }
    stream.write_all(&[0, 2]).await?;
    if stream.read_u8().await? != 0 {
        return Err(VncError::SecurityPolicy("VeNCrypt version rejected".into()));
    }
    let count = stream.read_u8().await?;
    let mut types = Vec::with_capacity(count as usize);
    for _ in 0..count {
        types.push(stream.read_u32().await?);
    }
    let (subtype, info) = if types.contains(&261) {
        (261, SecurityInfo::X509Vnc)
    } else if types.contains(&260) {
        (260, SecurityInfo::X509None)
    } else {
        return Err(VncError::SecurityPolicy(
            "Server has no supported X509None/X509Vnc subtype".into(),
        ));
    };
    stream.write_u32(subtype).await?;
    if stream.read_u8().await? != 1 {
        return Err(VncError::SecurityPolicy(
            "VeNCrypt X509 subtype rejected".into(),
        ));
    }
    let mut roots = rustls::RootCertStore::empty();
    if config.ca_certificates_der.is_empty() {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    } else {
        for der in config.ca_certificates_der {
            roots
                .add(der.into())
                .map_err(|e| VncError::Tls(e.to_string()))?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| VncError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(config.server_name)
        .map_err(|_| VncError::Tls("Invalid certificate target name".into()))?;
    let stream = tokio_rustls::TlsConnector::from(Arc::new(client))
        .connect(name, stream)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::NotConnected => VncError::IoError(e),
            _ => VncError::Tls(e.to_string()),
        })?;
    Ok((Box::new(stream), info))
}
