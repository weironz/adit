//! GitHub Gist.
//!
//! Chosen as the first provider because it needed no OAuth application at all:
//! a personal access token with the `gist` scope was the whole setup, and
//! GitHub keeps every revision by itself, so "restore an older version" costs
//! nothing to implement.
//!
//! **Two ways to get the token, and both are kept.** The normal one is now the
//! device flow in [`super::device`] — a code the user types into their browser,
//! no token minting by hand and no chance of pasting one with the wrong scope.
//! The manual path stays because the browser flow is the part most likely to be
//! unavailable: a locked-down corporate network can block `github.com/login`
//! outright, and an operator who already has a fine-grained token should not be
//! made to authorise an OAuth app to use it. This backend cannot tell the two
//! apart and does not try — a token is a token, both land in the same sealed
//! credential slot, and [`GistConfig::token`] is opaque to everything here.
//!
//! A **secret** gist is not public, but it is also not private — anyone with
//! the URL can read it. That is fine for the session list and settings, and it
//! is exactly why credentials only ever travel as the sealed blob.

use serde::Deserialize;

use super::device::DeviceFlowConfig;
use super::{agent, remote_error};
use crate::{RemoteRevision, SyncBackend, SyncDocument, SyncError};

const PROVIDER: &str = "GitHub Gist";

/// The device-flow authorisation for GitHub.
///
/// `gist` is the whole ask: it grants read and write over the user's gists and
/// nothing else — not repositories, not their profile. A narrower scope does
/// not exist, and a broader one would be asking for reach this backend never
/// uses.
#[must_use]
pub fn device_flow_config(client_id: String) -> DeviceFlowConfig {
    DeviceFlowConfig {
        provider: PROVIDER,
        client_id,
        // Note these are `github.com`, not `api.github.com` — the OAuth
        // endpoints live on the web host, and pointing them at the API host
        // yields a 404 that reads like a broken client id.
        device_code_url: "https://github.com/login/device/code",
        token_url: "https://github.com/login/oauth/access_token",
        scope: "gist",
    }
}

/// The file inside the gist. Fixed so a user can find it, and so a gist that
/// also holds other content keeps working.
const FILE_NAME: &str = "adit-sync.json";

#[derive(Debug, Clone)]
pub struct GistConfig {
    /// A token carrying the `gist` scope: either one the device flow minted or
    /// one the user pasted in by hand. Deliberately indistinguishable here.
    pub token: String,
    /// Existing gist id, or `None` to create one on the first push.
    pub gist_id: Option<String>,
}

pub struct GistBackend {
    config: GistConfig,
    agent: ureq::Agent,
}

#[derive(Deserialize)]
struct GistFile {
    #[serde(default)]
    content: String,
    /// GitHub truncates inline content past ~1 MB and expects the client to
    /// follow this URL. A large session list is well within reach of that.
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    raw_url: String,
}

#[derive(Deserialize)]
struct GistHistoryEntry {
    #[serde(default)]
    version: String,
}

#[derive(Deserialize)]
struct GistResponse {
    #[serde(default)]
    id: String,
    #[serde(default)]
    files: std::collections::HashMap<String, GistFile>,
    #[serde(default)]
    history: Vec<GistHistoryEntry>,
}

impl GistBackend {
    #[must_use]
    pub fn new(config: GistConfig) -> Self {
        Self {
            config,
            agent: agent(),
        }
    }

    /// The gist id, which the caller must persist after a first push created
    /// one — otherwise the next sync would make a second gist.
    #[must_use]
    pub fn gist_id(&self) -> Option<&str> {
        self.config.gist_id.as_deref()
    }

    fn auth(&self, request: ureq::Request) -> ureq::Request {
        request
            .set("Authorization", &format!("Bearer {}", self.config.token))
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28")
    }

    fn body_of(&self, file: &GistFile) -> Result<String, SyncError> {
        if !file.truncated {
            return Ok(file.content.clone());
        }
        self.agent
            .get(&file.raw_url)
            .call()
            .map_err(|error| remote_error(PROVIDER, error))?
            .into_string()
            .map_err(SyncError::Io)
    }
}

impl SyncBackend for GistBackend {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn fetch(&mut self) -> Result<Option<(SyncDocument, RemoteRevision)>, SyncError> {
        if self.config.token.trim().is_empty() {
            return Err(SyncError::NotAuthenticated {
                provider: PROVIDER.to_owned(),
            });
        }
        // No gist yet: nothing to fetch, and that is a first sync rather than
        // a failure.
        let Some(id) = self.config.gist_id.clone() else {
            return Ok(None);
        };

        let response = self
            .auth(self.agent.get(&format!("https://api.github.com/gists/{id}")))
            .call();
        let response = match response {
            Ok(response) => response,
            // A gist deleted from the web UI should re-create itself on the
            // next push, not wedge sync forever.
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(error) => return Err(remote_error(PROVIDER, error)),
        };

        let gist: GistResponse = response
            .into_json()
            .map_err(|error| SyncError::Malformed(error.to_string()))?;
        let Some(file) = gist.files.get(FILE_NAME) else {
            // The gist exists but holds other things; treat as empty and let
            // the push add our file alongside them.
            return Ok(None);
        };
        let body = self.body_of(file)?;
        if body.trim().is_empty() {
            return Ok(None);
        }
        let document = SyncDocument::parse(body.as_bytes())?;
        let revision = RemoteRevision {
            token: gist
                .history
                .first()
                .map(|entry| entry.version.clone())
                .unwrap_or_default(),
        };
        Ok(Some((document, revision)))
    }

    fn push(
        &mut self,
        document: &SyncDocument,
        _expected: Option<&RemoteRevision>,
    ) -> Result<RemoteRevision, SyncError> {
        if self.config.token.trim().is_empty() {
            return Err(SyncError::NotAuthenticated {
                provider: PROVIDER.to_owned(),
            });
        }
        let content = String::from_utf8(document.to_bytes()?)
            .map_err(|error| SyncError::Malformed(error.to_string()))?;

        // GitHub has no conditional write for gists — no If-Match, no
        // compare-and-swap — so `expected` cannot be enforced here, and a
        // second machine writing inside the fetch-push window is overwritten
        // silently.
        //
        // That does NOT heal by itself, and assuming it did was wrong: on the
        // next sync the loser's ancestor still holds the profile, the remote
        // does not, and a three-way merge reads that as "deleted remotely" and
        // removes it for good. The orchestration layer is what makes this
        // safe, by reading back after every push and only advancing the stored
        // ancestor once the remote is confirmed to be what we wrote. Keeping
        // the old ancestor is the whole trick: our own additions stay
        // additions rather than becoming remote deletions.
        let response = match &self.config.gist_id {
            Some(id) => self
                .auth(
                    self.agent
                        .patch(&format!("https://api.github.com/gists/{id}")),
                )
                .send_json(ureq::json!({
                    "description": "Adit sessions and settings",
                    "files": { FILE_NAME: { "content": content } },
                })),
            None => self
                .auth(self.agent.post("https://api.github.com/gists"))
                .send_json(ureq::json!({
                    "description": "Adit sessions and settings",
                    // Secret, not public: still URL-readable, never listed.
                    "public": false,
                    "files": { FILE_NAME: { "content": content } },
                })),
        }
        .map_err(|error| remote_error(PROVIDER, error))?;

        let gist: GistResponse = response
            .into_json()
            .map_err(|error| SyncError::Malformed(error.to_string()))?;
        if self.config.gist_id.is_none() && !gist.id.is_empty() {
            self.config.gist_id = Some(gist.id.clone());
        }
        Ok(RemoteRevision {
            token: gist
                .history
                .first()
                .map(|entry| entry.version.clone())
                .unwrap_or_default(),
        })
    }

    fn assigned_id(&self) -> Option<String> {
        self.config.gist_id.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty token is refused before any request, so the panel says "not
    /// signed in" instead of surfacing a 401 from GitHub.
    #[test]
    fn a_missing_token_is_caught_locally() {
        let mut backend = GistBackend::new(GistConfig {
            token: String::new(),
            gist_id: Some("abc".into()),
        });
        assert!(matches!(
            backend.fetch(),
            Err(SyncError::NotAuthenticated { .. })
        ));
    }

    /// The id is reported back through the trait, which is how the caller
    /// avoids creating a second gist on the next sync.
    #[test]
    fn a_configured_id_is_reported_for_persisting() {
        let backend = GistBackend::new(GistConfig {
            token: "token".into(),
            gist_id: Some("abc123".into()),
        });
        assert_eq!(backend.assigned_id().as_deref(), Some("abc123"));

        let fresh = GistBackend::new(GistConfig {
            token: "token".into(),
            gist_id: None,
        });
        assert!(fresh.assigned_id().is_none());
    }

    /// No gist id yet is a first sync, not an error — the push creates one.
    #[test]
    fn no_gist_yet_reads_as_empty() {
        let mut backend = GistBackend::new(GistConfig {
            token: "token".into(),
            gist_id: None,
        });
        assert!(backend.fetch().expect("no request is made").is_none());
    }
}
