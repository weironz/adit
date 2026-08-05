//! Cloud providers.
//!
//! Each provider is one file implementing [`SyncBackend`](crate::SyncBackend),
//! which is deliberately just fetch-and-push. Merging, conflict policy and
//! what gets stored all live above this layer, so adding a provider cannot get
//! those wrong — the worst a broken backend can do is fail to sync.

pub mod gist;
pub mod s3;
pub mod webdav;

use crate::SyncError;

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
