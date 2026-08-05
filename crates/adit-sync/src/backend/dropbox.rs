//! Dropbox, scoped to this app's own folder.
//!
//! The best conflict story of the three cloud drives: an upload carries the
//! `rev` it expects to replace, and Dropbox refuses the write outright if
//! anyone else got there first. That is real compare-and-swap, like WebDAV's
//! `If-Match` and unlike Gist, so a concurrent sync is *detected* rather than
//! silently overwritten.
//!
//! App-folder access means Dropbox creates `Apps/Adit/` and the token cannot
//! reach anything outside it — while the user can still open the file and read
//! it, which matters: their data should never be locked inside this program.

use serde::Deserialize;

use super::oauth::{OAuthConfig, OAuthSession};
use super::{agent, remote_error};
use crate::{RemoteRevision, SyncBackend, SyncDocument, SyncError};

const PROVIDER: &str = "Dropbox";
/// Relative to the app folder, so this is `Apps/Adit/adit-sync.json`.
const PATH: &str = "/adit-sync.json";

/// The loopback port the Dropbox App Console must have registered, as
/// `http://localhost:53682/`.
///
/// Dropbox compares `redirect_uri` against that list literally and documents no
/// loopback exception, so unlike Google and Microsoft it cannot be handed an
/// OS-assigned port. 53682 is not special to Dropbox — it is the port rclone
/// has had its users register for years, which makes it the one least likely to
/// be claimed by something else on the same machine.
const REDIRECT_PORT: u16 = 53682;

/// Dropbox only returns a refresh token when the authorize request asks for
/// offline access; without it the app works for four hours and then silently
/// stops with nothing to renew.
#[must_use]
pub fn oauth_config(client_id: String) -> OAuthConfig {
    OAuthConfig {
        provider: PROVIDER,
        client_id,
        auth_url: "https://www.dropbox.com/oauth2/authorize",
        token_url: "https://api.dropboxapi.com/oauth2/token",
        scope: "files.content.write files.content.read",
        extra_auth_params: &[("token_access_type", "offline")],
        redirect_port: Some(REDIRECT_PORT),
    }
}

pub struct DropboxBackend {
    session: OAuthSession,
    agent: ureq::Agent,
}

/// The metadata Dropbox returns in the `Dropbox-API-Result` header on
/// download, and in the body on upload.
#[derive(Deserialize)]
struct FileMetadata {
    #[serde(default)]
    rev: String,
}

impl DropboxBackend {
    #[must_use]
    pub fn new(client_id: String, refresh_token: String) -> Self {
        Self {
            session: OAuthSession::new(oauth_config(client_id), refresh_token),
            agent: agent(),
        }
    }

    /// The refresh token as it now stands. Dropbox can rotate it, so the
    /// caller re-saves this after every sync.
    #[must_use]
    pub fn refresh_token(&self) -> &str {
        self.session.refresh_token()
    }
}

impl SyncBackend for DropboxBackend {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn fetch(&mut self) -> Result<Option<(SyncDocument, RemoteRevision)>, SyncError> {
        let token = self.session.access_token()?;
        let arg = serde_json::json!({ "path": PATH }).to_string();

        let response = match self
            .agent
            .post("https://content.dropboxapi.com/2/files/download")
            .set("Authorization", &format!("Bearer {token}"))
            .set("Dropbox-API-Arg", &arg)
            .call()
        {
            Ok(response) => response,
            // 409 is Dropbox's answer for `path/not_found` as well as other
            // path problems; nothing stored yet is overwhelmingly the likely
            // one, and it is not an error.
            Err(ureq::Error::Status(409, _)) => return Ok(None),
            Err(error) => return Err(remote_error(PROVIDER, error)),
        };

        // The rev arrives in a header, not the body — the body is the file.
        let revision = response
            .header("Dropbox-API-Result")
            .and_then(|raw| serde_json::from_str::<FileMetadata>(raw).ok())
            .map(|metadata| RemoteRevision {
                token: metadata.rev,
            })
            .unwrap_or_default();

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
        let token = self.session.access_token()?;
        let bytes = document.to_bytes()?;

        // `update` with a rev is compare-and-swap; `add` refuses to overwrite
        // an existing file. Either way `autorename: false` is essential —
        // with it, Dropbox "resolves" a conflict by inventing
        // `adit-sync (1).json`, which nobody would ever look in again.
        let mode = match expected.map(|revision| revision.token.as_str()) {
            Some(rev) if !rev.is_empty() => {
                serde_json::json!({ ".tag": "update", "update": rev })
            }
            _ => serde_json::json!("add"),
        };
        let arg = serde_json::json!({
            "path": PATH,
            "mode": mode,
            "autorename": false,
            "mute": true,
        })
        .to_string();

        let response = match self
            .agent
            .post("https://content.dropboxapi.com/2/files/upload")
            .set("Authorization", &format!("Bearer {token}"))
            .set("Dropbox-API-Arg", &arg)
            .set("Content-Type", "application/octet-stream")
            .send_bytes(&bytes)
        {
            Ok(response) => response,
            // A rejected write is a lost race, not a failure to report: the
            // caller re-fetches, re-merges and tries again.
            Err(ureq::Error::Status(409, _)) => {
                return Err(SyncError::Conflict {
                    provider: PROVIDER.to_owned(),
                });
            }
            Err(error) => return Err(remote_error(PROVIDER, error)),
        };

        let metadata: FileMetadata = response
            .into_json()
            .map_err(|error| SyncError::Malformed(error.to_string()))?;
        Ok(RemoteRevision {
            token: metadata.rev,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Offline access is what makes a refresh token appear at all; without
    /// this parameter Dropbox hands back an access token that dies in four
    /// hours and nothing to renew it with.
    #[test]
    fn the_authorize_request_asks_for_offline_access() {
        let config = oauth_config("key".to_owned());
        assert!(config
            .extra_auth_params
            .contains(&("token_access_type", "offline")));
        assert!(config.scope.contains("files.content.write"));
        assert!(config.scope.contains("files.content.read"));
    }

    /// No refresh token means the user has not connected yet, and saying so
    /// locally beats a 401 from Dropbox.
    #[test]
    fn an_unconnected_account_is_caught_locally() {
        let mut backend = DropboxBackend::new("key".to_owned(), String::new());
        assert!(matches!(
            backend.fetch(),
            Err(SyncError::NotAuthenticated { .. })
        ));
    }
}
