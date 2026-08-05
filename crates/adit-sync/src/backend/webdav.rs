//! WebDAV — Nextcloud, ownCloud, 坚果云, Synology, any plain DAV share.
//!
//! The one provider here that supports a real conditional write: `ETag` plus
//! `If-Match` gives compare-and-swap, so a second machine writing between our
//! fetch and our push is *detected* rather than silently overwritten. That
//! surfaces as [`SyncError::Conflict`], and the caller re-fetches, re-merges
//! and retries — nothing is lost even when two machines sync at once.

use super::{agent, remote_error};
use crate::{RemoteRevision, SyncBackend, SyncDocument, SyncError};

const PROVIDER: &str = "WebDAV";

#[derive(Debug, Clone)]
pub struct WebDavConfig {
    /// Full URL of the file, e.g.
    /// `https://dav.example.com/remote.php/dav/files/alice/adit-sync.json`.
    /// A file rather than a folder: it keeps the backend to GET and PUT, with
    /// no PROPFIND XML to parse and no directory creation to get wrong.
    pub url: String,
    pub username: String,
    pub password: String,
}

pub struct WebDavBackend {
    config: WebDavConfig,
    agent: ureq::Agent,
}

impl WebDavBackend {
    #[must_use]
    pub fn new(config: WebDavConfig) -> Self {
        Self {
            config,
            agent: agent(),
        }
    }

    /// HTTP Basic. WebDAV servers vary in what else they accept, and Basic
    /// over TLS is the one thing all of them do.
    fn auth(&self, request: ureq::Request) -> ureq::Request {
        let credentials = base64_lite::encode(&format!(
            "{}:{}",
            self.config.username, self.config.password
        ));
        request.set("Authorization", &format!("Basic {credentials}"))
    }
}

impl SyncBackend for WebDavBackend {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn fetch(&mut self) -> Result<Option<(SyncDocument, RemoteRevision)>, SyncError> {
        if self.config.url.trim().is_empty() {
            return Err(SyncError::NotAuthenticated {
                provider: PROVIDER.to_owned(),
            });
        }
        let response = match self.auth(self.agent.get(&self.config.url)).call() {
            Ok(response) => response,
            // Nothing stored yet. 404 is the usual answer; some servers say
            // 405 for a path that has never existed under a collection.
            Err(ureq::Error::Status(404 | 405, _)) => return Ok(None),
            Err(error) => return Err(remote_error(PROVIDER, error)),
        };

        let revision = RemoteRevision {
            token: response.header("ETag").unwrap_or_default().to_owned(),
        };
        let body = response.into_string().map_err(SyncError::Io)?;
        if body.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some((SyncDocument::parse(body.as_bytes())?, revision)))
    }

    fn push(
        &mut self,
        document: &SyncDocument,
        expected: Option<&RemoteRevision>,
    ) -> Result<RemoteRevision, SyncError> {
        if self.config.url.trim().is_empty() {
            return Err(SyncError::NotAuthenticated {
                provider: PROVIDER.to_owned(),
            });
        }
        let bytes = document.to_bytes()?;
        let mut request = self
            .auth(self.agent.put(&self.config.url))
            .set("Content-Type", "application/json");

        // Compare-and-swap. `If-None-Match: *` for the first write means "only
        // if it does not exist yet", so two machines starting at the same
        // moment cannot both believe they created the file.
        request = match expected.map(|revision| revision.token.as_str()) {
            Some(etag) if !etag.is_empty() => request.set("If-Match", etag),
            _ => request.set("If-None-Match", "*"),
        };

        let response = match request.send_bytes(&bytes) {
            Ok(response) => response,
            // 412 Precondition Failed is the server saying someone else wrote
            // first. Not an error to show the user — a signal to re-merge.
            Err(ureq::Error::Status(412, _)) => {
                return Err(SyncError::Conflict {
                    provider: PROVIDER.to_owned(),
                });
            }
            Err(error) => return Err(remote_error(PROVIDER, error)),
        };

        // Most servers return the new ETag on PUT; those that do not leave the
        // token empty, which simply costs one extra fetch next time.
        Ok(RemoteRevision {
            token: response.header("ETag").unwrap_or_default().to_owned(),
        })
    }
}

/// Minimal base64 for the Basic auth header.
///
/// Inlined rather than pulled in as a dependency: this is the only base64 in
/// the crate, encode-only, on inputs measured in bytes.
mod base64_lite {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &str) -> String {
        let bytes = input.as_bytes();
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    let index = (n >> (18 - 6 * i)) & 0x3F;
                    out.push(ALPHABET[index as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked against the RFC 4648 vectors; a wrong pad length here would
    /// look like a password problem on every server.
    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(base64_lite::encode(""), "");
        assert_eq!(base64_lite::encode("f"), "Zg==");
        assert_eq!(base64_lite::encode("fo"), "Zm8=");
        assert_eq!(base64_lite::encode("foo"), "Zm9v");
        assert_eq!(base64_lite::encode("foob"), "Zm9vYg==");
        assert_eq!(base64_lite::encode("fooba"), "Zm9vYmE=");
        assert_eq!(base64_lite::encode("foobar"), "Zm9vYmFy");
    }

    /// The usual `user:password` shape, so a wrong header is caught here and
    /// not against someone's live server.
    #[test]
    fn credentials_encode_as_user_colon_password() {
        assert_eq!(base64_lite::encode("alice:secret"), "YWxpY2U6c2VjcmV0");
    }

    #[test]
    fn an_empty_url_is_caught_locally() {
        let mut backend = WebDavBackend::new(WebDavConfig {
            url: String::new(),
            username: "alice".into(),
            password: "secret".into(),
        });
        assert!(matches!(
            backend.fetch(),
            Err(SyncError::NotAuthenticated { .. })
        ));
    }
}
