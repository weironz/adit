//! Google Drive, restricted to files this app created.
//!
//! The `drive.file` scope is the point: it grants access only to files this
//! application itself created, so the token cannot see the user's photos,
//! documents or anything else — the consent screen says so in those words.
//! `drive.appdata` would also work and is arguably tidier, but it hides the
//! file in a folder the user cannot browse, and a sync file the owner cannot
//! find or copy is the wrong trade.
//!
//! Unlike OneDrive and Dropbox there is no path addressing: Drive works in
//! file ids, so the first request of every sync looks the file up by name.
//! Under `drive.file` that search only ever sees our own file, which is what
//! makes searching by a fixed name safe.

use serde::Deserialize;

use super::oauth::{OAuthConfig, OAuthSession};
use super::{agent, remote_error};
use crate::{RemoteRevision, SyncBackend, SyncDocument, SyncError};

const PROVIDER: &str = "Google Drive";
const FILE_NAME: &str = "adit-sync.json";

/// `access_type=offline` is what makes Google issue a refresh token at all,
/// and `prompt=consent` is what makes it issue one *again* on a repeat
/// authorisation — without it a user who reconnects gets an access token that
/// dies in an hour and nothing to renew it with.
#[must_use]
pub fn oauth_config(client_id: String, client_secret: String) -> OAuthConfig {
    OAuthConfig {
        provider: PROVIDER,
        client_id,
        client_secret,
        auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
        token_url: "https://oauth2.googleapis.com/token",
        scope: "https://www.googleapis.com/auth/drive.file",
        extra_auth_params: &[("access_type", "offline"), ("prompt", "consent")],
        // Google accepts any loopback port for a desktop client, so nothing
        // has to be registered and nothing can be occupied.
        redirect_port: None,
    }
}

pub struct GoogleDriveBackend {
    session: OAuthSession,
    agent: ureq::Agent,
    /// Cached across a sync so fetch-then-push costs one lookup, not two.
    file_id: Option<String>,
}

#[derive(Deserialize)]
struct FileEntry {
    #[serde(default)]
    id: String,
    /// Monotonic per-file counter. Compared, never parsed.
    #[serde(default)]
    version: String,
}

#[derive(Deserialize)]
struct FileList {
    #[serde(default)]
    files: Vec<FileEntry>,
}

impl GoogleDriveBackend {
    #[must_use]
    pub fn new(client_id: String, client_secret: String, refresh_token: String) -> Self {
        Self {
            session: OAuthSession::new(oauth_config(client_id, client_secret), refresh_token),
            agent: agent(),
            file_id: None,
        }
    }

    #[must_use]
    pub fn refresh_token(&self) -> &str {
        self.session.refresh_token()
    }

    /// Locate our file, or `None` on a first sync.
    fn locate(&mut self, token: &str) -> Result<Option<FileEntry>, SyncError> {
        let query = format!("name = '{FILE_NAME}' and trashed = false");
        let response = self
            .agent
            .get("https://www.googleapis.com/drive/v3/files")
            .query("q", &query)
            .query("fields", "files(id,version)")
            .query("pageSize", "1")
            .set("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|error| remote_error(PROVIDER, error))?;
        let list: FileList = response
            .into_json()
            .map_err(|error| SyncError::Malformed(error.to_string()))?;
        Ok(list.files.into_iter().next())
    }
}

impl SyncBackend for GoogleDriveBackend {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn fetch(&mut self) -> Result<Option<(SyncDocument, RemoteRevision)>, SyncError> {
        let token = self.session.access_token()?;
        let Some(entry) = self.locate(&token)? else {
            self.file_id = None;
            return Ok(None);
        };
        self.file_id = Some(entry.id.clone());

        let body = self
            .agent
            .get(&format!(
                "https://www.googleapis.com/drive/v3/files/{}",
                entry.id
            ))
            .query("alt", "media")
            .set("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|error| remote_error(PROVIDER, error))?
            .into_string()
            .map_err(SyncError::Io)?;
        if body.trim().is_empty() {
            return Ok(None);
        }

        Ok(Some((
            SyncDocument::parse(body.as_bytes())?,
            RemoteRevision {
                token: entry.version,
            },
        )))
    }

    fn push(
        &mut self,
        document: &SyncDocument,
        _expected: Option<&RemoteRevision>,
    ) -> Result<RemoteRevision, SyncError> {
        let token = self.session.access_token()?;
        let bytes = document.to_bytes()?;

        // Drive v3 has no conditional write for a media upload — no If-Match,
        // no compare-and-swap. Same position as Gist, and safe for the same
        // reason: the orchestration layer reads back after every push and only
        // advances the stored ancestor once the remote is confirmed to be
        // ours, so a lost race costs a retry rather than a session.
        let entry: FileEntry = match self.file_id.clone() {
            Some(id) => self
                .agent
                .request(
                    "PATCH",
                    &format!("https://www.googleapis.com/upload/drive/v3/files/{id}"),
                )
                .query("uploadType", "media")
                .query("fields", "id,version")
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_bytes(&bytes)
                .map_err(|error| remote_error(PROVIDER, error))?
                .into_json()
                .map_err(|error| SyncError::Malformed(error.to_string()))?,
            None => {
                // Create: metadata and content in one multipart body, so a
                // failure between two requests cannot leave an unnamed file.
                const BOUNDARY: &str = "adit-sync-boundary";
                let metadata = serde_json::json!({ "name": FILE_NAME }).to_string();
                let mut multipart = Vec::new();
                multipart.extend_from_slice(
                    format!(
                        "--{BOUNDARY}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n\
                         {metadata}\r\n--{BOUNDARY}\r\nContent-Type: application/json\r\n\r\n"
                    )
                    .as_bytes(),
                );
                multipart.extend_from_slice(&bytes);
                multipart.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

                self.agent
                    .post("https://www.googleapis.com/upload/drive/v3/files")
                    .query("uploadType", "multipart")
                    .query("fields", "id,version")
                    .set("Authorization", &format!("Bearer {token}"))
                    .set(
                        "Content-Type",
                        &format!("multipart/related; boundary={BOUNDARY}"),
                    )
                    .send_bytes(&multipart)
                    .map_err(|error| remote_error(PROVIDER, error))?
                    .into_json()
                    .map_err(|error| SyncError::Malformed(error.to_string()))?
            }
        };

        self.file_id = Some(entry.id);
        Ok(RemoteRevision {
            token: entry.version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The narrow scope is the promise made to the user on the consent screen;
    /// widening it would silently change what they agreed to.
    #[test]
    fn the_scope_is_limited_to_files_this_app_created() {
        let config = oauth_config("id".to_owned(), String::new());
        assert_eq!(
            config.scope, "https://www.googleapis.com/auth/drive.file",
            "must never ask for the whole drive"
        );
    }

    /// Both parameters are needed and each fails differently: without
    /// `access_type=offline` there is no refresh token at all, and without
    /// `prompt=consent` a *reconnecting* user gets none.
    #[test]
    fn the_authorize_request_asks_for_a_refresh_token_every_time() {
        let config = oauth_config("id".to_owned(), String::new());
        assert!(config
            .extra_auth_params
            .contains(&("access_type", "offline")));
        assert!(config.extra_auth_params.contains(&("prompt", "consent")));
    }

    #[test]
    fn an_unconnected_account_is_caught_locally() {
        let mut backend = GoogleDriveBackend::new("id".to_owned(), String::new(), String::new());
        assert!(matches!(
            backend.fetch(),
            Err(SyncError::NotAuthenticated { .. })
        ));
    }
}
