//! Cloud providers.
//!
//! Each provider is one file implementing [`SyncBackend`](crate::SyncBackend),
//! which is deliberately just fetch-and-push. Merging, conflict policy and
//! what gets stored all live above this layer, so adding a provider cannot get
//! those wrong — the worst a broken backend can do is fail to sync.

pub mod device;
pub mod dropbox;
pub mod gdrive;
pub mod gist;
pub mod oauth;
pub mod onedrive;
pub mod s3;
pub mod webdav;

use crate::SyncError;

/// Which provider a built-in client id belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthProvider {
    GoogleDrive,
    OneDrive,
    Dropbox,
    /// GitHub, for the Gist backend. Reached through the device flow rather
    /// than the loopback one — see [`device`] for why — but the client id is
    /// resolved identically, so it belongs in the same enum.
    GitHub,
}

/// The OAuth client id to use: the user's own if they supplied one, otherwise
/// whatever was compiled into this build.
///
/// Both halves earn their place. The built-in default is what makes the
/// feature work out of the box — nobody should need a Google Cloud project to
/// sync their session list. The override exists because a shared client id is
/// a shared API quota: rclone ships one for Drive and is retiring it for
/// exactly that reason, telling users to create their own. Better to have the
/// escape hatch from the start than to add it under pressure.
///
/// It also means a local or forked build works: the defaults come from
/// build-time environment variables that only the release pipeline sets, so
/// without an override those builds would otherwise have the providers dark.
#[must_use]
pub fn client_id(provider: OAuthProvider, user_override: &str) -> String {
    let trimmed = user_override.trim();
    if !trimmed.is_empty() {
        return trimmed.to_owned();
    }
    match provider {
        OAuthProvider::GoogleDrive => option_env!("ADIT_SYNC_GOOGLE_CLIENT_ID"),
        OAuthProvider::OneDrive => option_env!("ADIT_SYNC_ONEDRIVE_CLIENT_ID"),
        OAuthProvider::Dropbox => option_env!("ADIT_SYNC_DROPBOX_CLIENT_ID"),
        OAuthProvider::GitHub => option_env!("ADIT_SYNC_GITHUB_CLIENT_ID"),
    }
    .unwrap_or_default()
    .to_owned()
}

/// The OAuth client secret, for the one provider that insists on one.
///
/// Resolved exactly like [`client_id`]: the user's override wins, otherwise
/// whatever this build was compiled with. Empty means "send none", which is
/// correct for Dropbox and Microsoft and fatal for Google.
///
/// GitHub takes none either — the device flow is the one GitHub flow that
/// needs no client secret, which is most of why it was chosen over the web
/// application flow (whose token exchange still demands one, PKCE or not).
#[must_use]
pub fn client_secret(provider: OAuthProvider, user_override: &str) -> String {
    let trimmed = user_override.trim();
    if !trimmed.is_empty() {
        return trimmed.to_owned();
    }
    match provider {
        OAuthProvider::GoogleDrive => option_env!("ADIT_SYNC_GOOGLE_CLIENT_SECRET"),
        OAuthProvider::OneDrive | OAuthProvider::Dropbox | OAuthProvider::GitHub => None,
    }
    .unwrap_or_default()
    .to_owned()
}

/// Timeout for a single request. Sync is a background task, but an unbounded
/// wait would leave the status panel saying "syncing" forever.
pub(crate) const TIMEOUT_SECONDS: u64 = 30;

pub(crate) fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECONDS))
        .user_agent(concat!("Adit/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Turn a `ureq` failure into a `SyncError` that names the provider and keeps
/// the server's own message.
///
/// The body matters: every one of these services explains a refusal in it
/// ("Bad credentials", "insufficient scope", an S3 `<Code>`), and a bare
/// status number turns a fixable configuration mistake into a guess.
pub(crate) fn remote_error(provider: &'static str, error: ureq::Error) -> SyncError {
    match error {
        ureq::Error::Status(code, response) => {
            let detail = response.into_string().unwrap_or_default();
            let detail = detail.trim();
            let message = if detail.is_empty() {
                format!("HTTP {code}")
            } else {
                // Cap it: an HTML error page would otherwise fill the panel.
                let mut snippet: String = detail.chars().take(300).collect();
                if detail.chars().count() > 300 {
                    snippet.push('…');
                }
                format!("HTTP {code}: {snippet}")
            };
            SyncError::Remote {
                provider: provider.to_owned(),
                message,
            }
        }
        ureq::Error::Transport(transport) => SyncError::Remote {
            provider: provider.to_owned(),
            message: transport.to_string(),
        },
    }
}
