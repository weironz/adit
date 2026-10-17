use super::{
    auth::{AuthHelper, AuthResult, SecurityType},
    connection::VncClient,
};
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tracing::{info, trace};

use crate::{PixelFormat, VncEncoding, VncError, VncVersion};

pub enum VncState<S, F>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    F: Future<Output = Result<String, VncError>> + Send + Sync + 'static,
{
    Handshake(VncConnector<S, F>),
    Authenticate(VncConnector<S, F>),
    Connected(VncClient),
}

impl<S, F> VncState<S, F>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    F: Future<Output = Result<String, VncError>> + Send + Sync + 'static,
{
    pub fn try_start(
        self,
    ) -> Pin<Box<dyn Future<Output = Result<Self, VncError>> + Send + Sync + 'static>> {
        Box::pin(async move {
            match self {
                VncState::Handshake(mut connector) => {
                    // Read the rfbversion informed by the server
                    let rfbversion = VncVersion::read(&mut connector.stream).await?;
                    trace!(
                        "Our version {:?}, server version {:?}",
                        connector.rfb_version,
                        rfbversion
                    );
                    let rfbversion = if connector.rfb_version < rfbversion {
                        connector.rfb_version
                    } else {
                        rfbversion
                    };

                    // Record the negotiated rfbversion
                    connector.rfb_version = rfbversion;
                    trace!("Negotiated rfb version: {:?}", rfbversion);
                    rfbversion.write(&mut connector.stream).await?;
                    Ok(VncState::Authenticate(connector).try_start().await?)
                }
                VncState::Authenticate(mut connector) => {
                    let security_types =
                        SecurityType::read(&mut connector.stream, &connector.rfb_version).await?;

                    if security_types.is_empty() {
                        return Err(VncError::ConnectError);
                    }

                    let selected = match connector.security_policy {
                        crate::SecurityPolicy::RequireTls
                            if security_types.contains(&SecurityType::VeNCrypt) =>
                        {
                            SecurityType::VeNCrypt
                        }
                        crate::SecurityPolicy::RequireTls => {
                            return Err(VncError::SecurityPolicy(
                                "Verified TLS is required; server has no VeNCrypt".into(),
                            ))
                        }
                        _ if security_types.contains(&SecurityType::Tight)
                            && connector.utf8_clipboard =>
                        {
                            SecurityType::Tight
                        }
                        _ if security_types.contains(&SecurityType::VncAuth) => {
                            SecurityType::VncAuth
                        }
                        crate::SecurityPolicy::AllowNone
                            if security_types.contains(&SecurityType::None) =>
                        {
                            SecurityType::None
                        }
                        _ if security_types.contains(&SecurityType::Tight) => SecurityType::Tight,
                        _ => {
                            return Err(VncError::SecurityPolicy(
                                "Password authentication is required; server has no VncAuth".into(),
                            ))
                        }
                    };
                    if connector.rfb_version != VncVersion::RFB33 {
                        selected.write(&mut connector.stream).await?;
                    }
                    let mut stream: crate::security::Stream = Box::new(connector.stream);
                    let security = if selected == SecurityType::VeNCrypt {
                        let config = connector.tls_config.ok_or_else(|| {
                            VncError::SecurityPolicy("TLS target name/configuration missing".into())
                        })?;
                        let (tls, info) = crate::security::vencrypt(stream, config).await?;
                        stream = tls;
                        info
                    } else if selected == SecurityType::Tight {
                        crate::tight::authenticate(&mut stream, connector.security_policy).await?
                    } else if selected == SecurityType::VncAuth {
                        crate::SecurityInfo::VncAuth
                    } else {
                        crate::SecurityInfo::None
                    };
                    if matches!(
                        security,
                        crate::SecurityInfo::VncAuth | crate::SecurityInfo::X509Vnc
                    ) {
                        let credential = connector
                            .auth_methond
                            .take()
                            .ok_or(VncError::NoPassword)?
                            .await?;
                        let auth = AuthHelper::read(&mut stream, &credential).await?;
                        auth.write(&mut stream).await?;
                        let result = auth.finish(&mut stream).await?;
                        if let AuthResult::Failed = result {
                            // Consume bounded reason text, but do not let server-controlled text
                            // change the typed authentication failure into a retryable I/O error.
                            if connector.rfb_version == VncVersion::RFB38 {
                                let _ = crate::limits::string(&mut stream, crate::limits::MAX_NAME)
                                    .await;
                            }
                            return Err(VncError::WrongPassword);
                        }
                    } else if (security == crate::SecurityInfo::X509None
                        || connector.rfb_version == VncVersion::RFB38)
                        && stream.read_u32().await? != 0
                    {
                        return Err(VncError::SecurityPolicy(
                            "Server rejected security negotiation".into(),
                        ));
                    }
                    info!("auth done, client connected");

                    Ok(VncState::Connected(
                        VncClient::new_with_options(
                            stream,
                            connector.allow_shared,
                            connector.pixel_format,
                            connector.encodings,
                            selected == SecurityType::Tight,
                            connector.utf8_clipboard,
                        )
                        .await?
                        .with_security(security),
                    ))
                }
                _ => unreachable!(),
            }
        })
    }

    pub fn finish(self) -> Result<VncClient, VncError> {
        if let VncState::Connected(client) = self {
            Ok(client)
        } else {
            Err(VncError::ConnectError)
        }
    }
}

/// Connection Builder to setup a vnc client
pub struct VncConnector<S, F>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: Future<Output = Result<String, VncError>> + Send + Sync + 'static,
{
    stream: S,
    auth_methond: Option<F>,
    rfb_version: VncVersion,
    allow_shared: bool,
    pixel_format: Option<PixelFormat>,
    encodings: Vec<VncEncoding>,
    security_policy: crate::SecurityPolicy,
    tls_config: Option<crate::TlsConfig>,
    utf8_clipboard: bool,
}

impl<S, F> VncConnector<S, F>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    F: Future<Output = Result<String, VncError>> + Send + Sync + 'static,
{
    /// To new a vnc client configuration with stream `S`
    ///
    /// `S` should implement async I/O methods
    ///
    /// ```no_run
    /// use vnc::{PixelFormat, VncConnector, VncError};
    /// use tokio::{self, net::TcpStream};
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), VncError> {
    ///     let tcp = TcpStream::connect("127.0.0.1:5900").await?;
    ///     let vnc = VncConnector::new(tcp)
    ///         .set_auth_method(async move { Ok("password".to_string()) })
    ///         .add_encoding(vnc::VncEncoding::Tight)
    ///         .add_encoding(vnc::VncEncoding::Zrle)
    ///         .add_encoding(vnc::VncEncoding::CopyRect)
    ///         .add_encoding(vnc::VncEncoding::Raw)
    ///         .allow_shared(true)
    ///         .set_pixel_format(PixelFormat::bgra())
    ///         .build()?
    ///         .try_start()
    ///         .await?
    ///         .finish()?;
    ///     Ok(())
    /// }
    /// ```
    ///
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            auth_methond: None,
            allow_shared: true,
            rfb_version: VncVersion::RFB38,
            pixel_format: None,
            encodings: Vec::new(),
            security_policy: crate::SecurityPolicy::default(),
            tls_config: None,
            utf8_clipboard: true,
        }
    }

    /// An async callback which is used to query credentials if the vnc server has set
    ///
    /// ```no_compile
    /// connector = connector.set_auth_method(async move { Ok("password".to_string()) })
    /// ```
    ///
    /// if you're building a wasm app,
    /// the async callback also allows you to combine it to a promise
    ///
    /// ```no_compile
    /// #[wasm_bindgen]
    /// extern "C" {
    ///     fn get_password() -> js_sys::Promise;
    /// }
    ///
    /// connector = connector
    ///        .set_auth_method(async move {
    ///            let auth = JsFuture::from(get_password()).await.unwrap();
    ///            Ok(auth.as_string().unwrap())
    ///     });
    /// ```
    ///
    /// While in the js code
    ///
    ///
    /// ```javascript
    /// var password = '';
    /// function get_password() {
    ///     return new Promise((reslove, reject) => {
    ///        document.getElementById("submit_password").addEventListener("click", () => {
    ///             password = window.document.getElementById("input_password").value
    ///             reslove(password)
    ///         })
    ///     });
    /// }
    /// ```
    ///
    /// The future won't be polled if the sever doesn't apply any password protections to the session
    ///
    pub fn set_auth_method(mut self, auth_callback: F) -> Self {
        self.auth_methond = Some(auth_callback);
        self
    }

    /// Disable both auto-negotiated UTF-8 clipboard paths for explicit legacy interoperability.
    pub fn utf8_clipboard(mut self, enabled: bool) -> Self {
        self.utf8_clipboard = enabled;
        self
    }

    pub fn security_policy(mut self, policy: crate::SecurityPolicy) -> Self {
        self.security_policy = policy;
        self
    }
    pub fn tls_config(mut self, config: crate::TlsConfig) -> Self {
        self.tls_config = Some(config);
        self
    }
    /// Tight quality 0..9; None suppresses JPEG for a lossless session.
    pub fn tight_quality(mut self, compression: u8, quality: Option<u8>) -> Self {
        self.encodings
            .push(VncEncoding::compression(compression.min(9)));
        if let Some(quality) = quality {
            self.encodings.push(VncEncoding::quality(quality.min(9)));
        }
        self
    }

    /// The max vnc version that we supported
    ///
    /// Version should be one of the [VncVersion]
    ///
    pub fn set_version(mut self, version: VncVersion) -> Self {
        self.rfb_version = version;
        self
    }

    /// Set the rgb order which you will use to resolve the image data
    ///
    /// In most of the case, use `PixelFormat::bgra()` on little endian PCs
    ///
    /// And use `PixelFormat::rgba()` on wasm apps (with canvas)
    ///
    /// Also, customized format is allowed
    ///
    /// Will use the default format informed by the vnc server if not set
    ///
    /// In this condition, the client will get a [crate::VncEvent::SetPixelFormat] event notified
    ///
    pub fn set_pixel_format(mut self, pf: PixelFormat) -> Self {
        self.pixel_format = Some(pf);
        self
    }

    /// Shared-flag is non-zero (true) if the server should try to share the
    ///
    /// desktop by leaving other clients connected, and zero (false) if it
    ///
    /// should give exclusive access to this client by disconnecting all
    ///
    /// other clients.
    ///
    pub fn allow_shared(mut self, allow_shared: bool) -> Self {
        self.allow_shared = allow_shared;
        self
    }

    /// Client encodings that we want to use
    ///
    /// One of [VncEncoding]
    ///
    /// [VncEncoding::Raw] must be sent as the RFC required
    ///
    /// The order to add encodings is the order to inform the server
    ///
    pub fn add_encoding(mut self, encoding: VncEncoding) -> Self {
        self.encodings.push(encoding);
        self
    }

    /// Complete the client configuration
    ///
    pub fn build(mut self) -> Result<VncState<S, F>, VncError> {
        if !self.utf8_clipboard {
            self.encodings
                .retain(|encoding| *encoding != VncEncoding::ExtendedClipboard);
        }
        if self.encodings.is_empty() {
            return Err(VncError::NoEncoding);
        }
        if let Some(format) = &self.pixel_format {
            format.validate()?;
        }
        Ok(VncState::Handshake(self))
    }
}
