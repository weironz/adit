//! Cloud sync for Adit's sessions, groups and settings.
//!
//! The transport is the easy half and is deliberately kept behind one small
//! trait ([`SyncBackend`]) so a provider is a self-contained file. The half
//! that can lose a user's work is [`merge`], and it is where the care went.
//!
//! **What travels.** Sessions and groups, settings, and — only if the user
//! opts in — the credential file. That file is already sealed with
//! XChaCha20-Poly1305 under an Argon2id key (see `adit_storage::credentials`),
//! so the bytes are safe to park on any provider. The passphrase never leaves
//! the machine, which is the point: a provider breach yields ciphertext, and
//! moving to a new machine costs one passphrase entry.
//!
//! **Why an ancestor is stored.** Merging two catalogs without knowing what
//! they last agreed on cannot tell an addition from a deletion. So each
//! successful sync records the catalog it produced, and the next merge uses it
//! as the common ancestor. Losing that file is not fatal — the merge falls
//! back to a union, which over-keeps rather than deletes.

pub mod backend;
pub mod merge;

use adit_storage::{AppSettings, ProfileCatalog};
use serde::{Deserialize, Serialize};

/// Format version of the document parked on the provider. Bumped only for a
/// change old clients cannot read; they refuse rather than guess.
pub const DOCUMENT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("the remote document is version {found}, this build understands {DOCUMENT_VERSION}")]
    UnsupportedVersion { found: u32 },
    #[error("the remote document could not be parsed: {0}")]
    Malformed(String),
    #[error("{provider} rejected the request: {message}")]
    Remote { provider: String, message: String },
    #[error("not signed in to {provider}")]
    NotAuthenticated { provider: String },
    /// Someone else wrote between our fetch and our push. Not a failure to
    /// report — the caller re-fetches, re-merges and retries, which is why
    /// concurrent syncs cannot lose a session.
    #[error("{provider} was written by another device; merging again")]
    Conflict { provider: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The payload stored on the provider.
///
/// Plain JSON on purpose: a user must be able to open their own Gist and see
/// what Adit put there, and recover it by hand if the app is unavailable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncDocument {
    pub version: u32,
    pub catalog: ProfileCatalog,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<AppSettings>,
    /// The sealed credential file, hex-encoded, when the user opted in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
    /// Which machine wrote this, for the status panel. Cosmetic only — never
    /// used to decide a merge, since a device name is not an identity.
    #[serde(default)]
    pub device: String,
    /// RFC 3339, for display. Deliberately not a merge input: two machines'
    /// clocks disagree, and a restored backup carries a stale one.
    #[serde(default)]
    pub written_at: String,
}

impl SyncDocument {
    #[must_use]
    pub fn new(catalog: ProfileCatalog, device: String, written_at: String) -> Self {
        Self {
            version: DOCUMENT_VERSION,
            catalog,
            settings: None,
            credentials: None,
            device,
            written_at,
        }
    }

    /// Parse a document fetched from a provider, refusing a future version
    /// rather than silently dropping the fields it does not know.
    pub fn parse(bytes: &[u8]) -> Result<Self, SyncError> {
        let document: Self = serde_json::from_slice(bytes)
            .map_err(|error| SyncError::Malformed(error.to_string()))?;
        if document.version > DOCUMENT_VERSION {
            return Err(SyncError::UnsupportedVersion {
                found: document.version,
            });
        }
        Ok(document)
    }

    /// Serialize for upload. Pretty-printed because a user reading their own
    /// Gist is a supported way to use this.
    pub fn to_bytes(&self) -> Result<Vec<u8>, SyncError> {
        serde_json::to_vec_pretty(self).map_err(|error| SyncError::Malformed(error.to_string()))
    }
}

/// What a provider hands back alongside the bytes, so the next upload can tell
/// whether anyone else wrote in the meantime.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRevision {
    /// Provider-specific version handle: a Gist revision sha, a WebDAV ETag,
    /// an S3 version id. Opaque — only ever compared, never interpreted.
    pub token: String,
}

/// One cloud provider.
///
/// Deliberately tiny: fetch the document, upload a new one. Everything about
/// merging, conflict policy and what gets stored lives above this line, so a
/// new provider cannot get those wrong.
pub trait SyncBackend: Send {
    /// Human-readable provider name, used in errors and the status panel.
    fn provider(&self) -> &'static str;

    /// Fetch the current document. `Ok(None)` means the provider is reachable
    /// and simply has nothing yet — a first sync, not an error.
    fn fetch(&mut self) -> Result<Option<(SyncDocument, RemoteRevision)>, SyncError>;

    /// Upload `document`. `expected` is the revision this upload is based on;
    /// a backend that can detect a lost update should refuse when the remote
    /// has moved on, and the caller re-merges.
    fn push(
        &mut self,
        document: &SyncDocument,
        expected: Option<&RemoteRevision>,
    ) -> Result<RemoteRevision, SyncError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_document_is_refused_rather_than_truncated() {
        let json = br#"{"version":999,"catalog":{"groups":[],"profiles":[]}}"#;
        assert!(matches!(
            SyncDocument::parse(json),
            Err(SyncError::UnsupportedVersion { found: 999 })
        ));
    }

    #[test]
    fn a_document_round_trips() {
        let document = SyncDocument::new(
            ProfileCatalog::new(vec!["prod".into()], Vec::new()),
            "willpc".into(),
            "2026-08-05T02:30:00Z".into(),
        );
        let bytes = document.to_bytes().expect("serialize");
        let back = SyncDocument::parse(&bytes).expect("parse");
        assert_eq!(back.catalog.groups, ["prod"]);
        assert_eq!(back.device, "willpc");
        assert!(back.credentials.is_none());
    }
}
