//! OAuth 2.0 for native applications: PKCE over a loopback redirect.
//!
//! Shared by Google Drive, OneDrive and Dropbox, which differ only in their
//! endpoints and scopes.
//!
//! **Why loopback and not a custom URL scheme.** RFC 8252 prefers a loopback
//! listener for desktop apps: no OS-wide scheme registration to install, no
//! other application able to claim the same scheme and intercept the code, and
//! it behaves identically on all three platforms. The port is whatever the OS
//! hands out — a fixed one would collide with whatever else is running, and
//! providers accept any port on 127.0.0.1 for native clients.
//!
//! **Why no client secret.** These are public clients: whatever is compiled
//! into a binary the user already has is not a secret, which is the problem
//! PKCE exists to solve. Google still issues a "secret" for desktop app types
//! and its own documentation concedes it is not confidential; sending it would
//! add nothing here but the pretence of protection.
//!
//! **What is stored.** The refresh token, in the sealed credential store, and
//! nothing else — access tokens live in memory and are re-minted on demand.
//! The verifier never leaves this process.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use super::{agent, remote_error};
use crate::SyncError;

/// How long to wait for the browser to come back. Long enough to sign in and
/// pick an account, short enough that an abandoned attempt does not hold a
/// socket open all day.
const AUTH_TIMEOUT: Duration = Duration::from_secs(180);

/// Endpoints and scopes for one provider.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub provider: &'static str,
    pub client_id: &'static str,
    pub auth_url: &'static str,
    pub token_url: &'static str,
    pub scope: &'static str,
    /// Extra query parameters the provider needs on the authorize request.
    /// Google needs `access_type=offline` and `prompt=consent`, or it stops
    /// returning a refresh token on repeat authorisations.
    pub extra_auth_params: &'static [(&'static str, &'static str)],
}

/// What a token exchange yields.
#[derive(Debug, Clone)]
pub struct Tokens {
    pub access_token: String,
    /// Absent when the provider chose not to re-issue one; the caller keeps
    /// the token it already had rather than dropping it.
    pub refresh_token: Option<String>,
    pub expires_at: SystemTime,
}

impl Tokens {
    /// A minute of slack, so a token that would expire mid-request is
    /// refreshed before it is used rather than after it fails.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        SystemTime::now() + Duration::from_secs(60) >= self.expires_at
    }
}

/// An authorisation in progress: the URL to open, and a listener already bound
/// so the redirect cannot arrive before anyone is listening.
pub struct PendingAuth {
    pub url: String,
    listener: TcpListener,
    verifier: String,
    state: String,
    config: OAuthConfig,
}

/// Start an authorisation. Binding the listener first is deliberate: the URL
/// carries the port, so it cannot be built before the OS has assigned one.
pub fn begin(config: OAuthConfig) -> Result<PendingAuth, SyncError> {
    if config.client_id.trim().is_empty() {
        return Err(SyncError::NotAuthenticated {
            provider: config.provider.to_owned(),
        });
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let verifier = random_urlsafe(64);
    let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(32);

    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}\
         &code_challenge={challenge}&code_challenge_method=S256",
        config.auth_url,
        percent(config.client_id),
        percent(&redirect_uri),
        percent(config.scope),
        percent(&state),
    );
    for (key, value) in config.extra_auth_params {
        url.push_str(&format!("&{key}={}", percent(value)));
    }

    Ok(PendingAuth {
        url,
        listener,
        verifier,
        state,
        config,
    })
}

impl PendingAuth {
    /// Block until the browser redirects back, then trade the code for tokens.
    /// Runs on a worker thread — it waits on a human.
    pub fn complete(self) -> Result<Tokens, SyncError> {
        let port = self.listener.local_addr()?.port();
        let code = self.wait_for_code()?;
        exchange(&self.config, &code, &self.verifier, port)
    }

    fn wait_for_code(&self) -> Result<String, SyncError> {
        self.listener.set_nonblocking(true).map_err(SyncError::Io)?;
        let deadline = SystemTime::now() + AUTH_TIMEOUT;

        loop {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).map_err(SyncError::Io)?;
                    let mut reader = BufReader::new(stream.try_clone().map_err(SyncError::Io)?);
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).map_err(SyncError::Io)?;

                    let query = request_line
                        .split_whitespace()
                        .nth(1)
                        .and_then(|target| target.split_once('?'))
                        .map(|(_, query)| query.to_owned())
                        .unwrap_or_default();
                    let params = parse_query(&query);

                    // Answer the browser before returning either way, so the
                    // user sees an outcome instead of a connection error.
                    let ok = params.contains_key("code")
                        && params.get("state").map(String::as_str) == Some(self.state.as_str());
                    let body = if ok {
                        "<h2>已授权</h2><p>可以关闭此页面，返回 Adit。</p>"
                    } else {
                        "<h2>授权失败</h2><p>请返回 Adit 重试。</p>"
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();

                    if let Some(error) = params.get("error") {
                        return Err(SyncError::Remote {
                            provider: self.config.provider.to_owned(),
                            message: format!("授权被拒绝: {error}"),
                        });
                    }
                    // The state check is what stops another page on this
                    // machine from feeding us a code for a different account.
                    if params.get("state").map(String::as_str) != Some(self.state.as_str()) {
                        return Err(SyncError::Remote {
                            provider: self.config.provider.to_owned(),
                            message: String::from("授权响应的 state 不匹配，已拒绝"),
                        });
                    }
                    return params.get("code").cloned().ok_or(SyncError::Remote {
                        provider: self.config.provider.to_owned(),
                        message: String::from("授权响应缺少 code"),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if SystemTime::now() >= deadline {
                        return Err(SyncError::Remote {
                            provider: self.config.provider.to_owned(),
                            message: String::from("等待浏览器授权超时"),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
                Err(error) => return Err(SyncError::Io(error)),
            }
        }
    }
}

fn exchange(
    config: &OAuthConfig,
    code: &str,
    verifier: &str,
    port: u16,
) -> Result<Tokens, SyncError> {
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", config.client_id),
        ("redirect_uri", redirect_uri.as_str()),
        ("code_verifier", verifier),
    ];
    post_token(config, &form)
}

/// Trade a refresh token for a fresh access token.
pub fn refresh(config: &OAuthConfig, refresh_token: &str) -> Result<Tokens, SyncError> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", config.client_id),
    ];
    let mut tokens = post_token(config, &form)?;
    // Google and Microsoft usually omit the refresh token on a refresh; the
    // old one stays valid, and dropping it would silently sign the user out at
    // the next expiry.
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh_token.to_owned());
    }
    Ok(tokens)
}

fn post_token(config: &OAuthConfig, form: &[(&str, &str)]) -> Result<Tokens, SyncError> {
    #[derive(serde::Deserialize)]
    struct TokenResponse {
        #[serde(default)]
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_in: Option<u64>,
    }

    let response = agent()
        .post(config.token_url)
        .send_form(form)
        .map_err(|error| remote_error(config.provider, error))?;
    let parsed: TokenResponse = response
        .into_json()
        .map_err(|error| SyncError::Malformed(error.to_string()))?;
    if parsed.access_token.is_empty() {
        return Err(SyncError::Remote {
            provider: config.provider.to_owned(),
            message: String::from("令牌响应中没有 access_token"),
        });
    }
    Ok(Tokens {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        // A provider that omits `expires_in` is treated as short-lived rather
        // than eternal: refreshing needlessly is cheap, using a dead token is
        // a failed sync.
        expires_at: SystemTime::now() + Duration::from_secs(parsed.expires_in.unwrap_or(3600)),
    })
}

/// Base64url without padding, as PKCE requires (RFC 7636 §4.2).
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            let index = (n >> (18 - 6 * i)) & 0x3F;
            out.push(ALPHABET[index as usize] as char);
        }
    }
    out
}

/// A high-entropy string from the OS, for the verifier and the state.
///
/// The OS RNG rather than a PRNG: the verifier is the only thing stopping
/// someone who intercepts the authorisation code from redeeming it, so
/// guessability is the whole security property.
fn random_urlsafe(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    fill_random(&mut buffer);
    base64url(&buffer)
}

#[cfg(windows)]
fn fill_random(buffer: &mut [u8]) {
    // `BCryptGenRandom` with the system-preferred RNG, linked directly rather
    // than adding a dependency for one call.
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(
            h_algorithm: *mut core::ffi::c_void,
            pb_buffer: *mut u8,
            cb_buffer: u32,
            dw_flags: u32,
        ) -> i32;
    }
    const USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    let status = unsafe {
        BCryptGenRandom(
            core::ptr::null_mut(),
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).unwrap_or(0),
            USE_SYSTEM_PREFERRED_RNG,
        )
    };
    assert!(status == 0, "BCryptGenRandom failed: {status:#x}");
}

#[cfg(not(windows))]
fn fill_random(buffer: &mut [u8]) {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(buffer))
        .expect("/dev/urandom");
}

/// Percent-encode a query value: unreserved characters only. A space must be
/// `%20` and never `+`, because providers differ on whether they decode `+`.
fn percent(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), percent_decode(value)))
        .collect()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 7636 appendix B vector. A wrong challenge is only rejected at
    /// the token exchange, long after the browser dance, so it is worth
    /// pinning here.
    #[test]
    fn the_pkce_challenge_matches_the_rfc_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    /// Base64url differs from standard base64 in two characters and drops the
    /// padding; either mistake makes every authorisation fail.
    #[test]
    fn base64url_is_unpadded_and_url_safe() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        // The two characters that differ from standard base64: `+/` there,
        // `-_` here.
        assert_eq!(base64url(&[0xFB, 0xFF]), "-_8");
    }

    #[test]
    fn query_values_round_trip_through_percent_coding() {
        assert_eq!(percent("a b"), "a%20b");
        assert_eq!(
            percent("http://127.0.0.1:1234"),
            "http%3A%2F%2F127.0.0.1%3A1234"
        );
        let parsed = parse_query("code=4%2F0Ab&state=xyz&scope=a%20b");
        assert_eq!(parsed.get("code").map(String::as_str), Some("4/0Ab"));
        assert_eq!(parsed.get("state").map(String::as_str), Some("xyz"));
        assert_eq!(parsed.get("scope").map(String::as_str), Some("a b"));
    }

    /// Two verifiers from the OS RNG must not collide, and must sit inside the
    /// 43–128 character range RFC 7636 requires.
    #[test]
    fn verifiers_are_random_and_long_enough() {
        let a = random_urlsafe(64);
        let b = random_urlsafe(64);
        assert_ne!(a, b);
        assert!(a.len() >= 43 && a.len() <= 128, "length {}", a.len());
    }

    /// Without a client id there is nothing to authorise against, and saying
    /// so locally beats redirecting to a provider error page.
    #[test]
    fn a_missing_client_id_is_refused_before_opening_a_browser() {
        let config = OAuthConfig {
            provider: "Test",
            client_id: "",
            auth_url: "https://example.com/auth",
            token_url: "https://example.com/token",
            scope: "files",
            extra_auth_params: &[],
        };
        assert!(matches!(
            begin(config),
            Err(SyncError::NotAuthenticated { .. })
        ));
    }
}
