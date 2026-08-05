//! OneDrive, scoped to this app's own folder.
//!
//! Microsoft Graph addresses the app folder by path, so there is no file id to
//! look up first — `special/approot:/adit-sync.json` is the whole address.
//! That makes this the simplest of the three cloud drives, and it supports
//! `if-match`, so a concurrent write is detected rather than overwritten.
//!
//! `Files.ReadWrite.AppFolder` cannot reach anything outside `Apps/Adit/`, and
//! the folder is visible to the user like any other — they can open the file,
//! copy it, or delete it without going near this program.

use serde::Deserialize;

use super::oauth::{OAuthConfig, OAuthSession};
use super::{agent, remote_error};
use crate::{RemoteRevision, SyncBackend, SyncDocument, SyncError};

const PROVIDER: &str = "OneDrive";
const ITEM_URL: &str = "https://graph.microsoft.com/v1.0/me/drive/special/approot:/adit-sync.json";

/// `offline_access` is what produces a refresh token; `common` as the tenant
/// is what lets personal Microsoft accounts sign in alongside work ones.
#[must_use]
pub fn oauth_config(client_id: String) -> OAuthConfig {
    OAuthConfig {
        provider: PROVIDER,
        client_id,
        // A public client, exactly as PKCE intends.
        client_secret: String::new(),
        auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        scope: "Files.ReadWrite.AppFolder offline_access",
        extra_auth_params: &[],
        // Azure matches `http://localhost` for a mobile-and-desktop platform
        // registration without pinning the port.
        redirect_port: None,
    }
}

pub struct OneDriveBackend {
    session: OAuthSession,
    agent: ureq::Agent,
}

#[derive(Deserialize)]
struct DriveItem {
    #[serde(rename = "eTag", default)]
    etag: String,
}

impl OneDriveBackend {
    #[must_use]
    pub fn new(client_id: String, refresh_token: String) -> Self {
        Self {
            session: OAuthSession::new(oauth_config(client_id), refresh_token),
            agent: agent(),
        }
    }

    #[must_use]
    pub fn refresh_token(&self) -> &str {
        self.session.refresh_token()
    }
}

impl SyncBackend for OneDriveBackend {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn fetch(&mut self) -> Result<Option<(SyncDocument, RemoteRevision)>, SyncError> {
        let token = self.session.access_token()?;

        // Metadata first, for the eTag. Fetching `/content` alone would work,
        // but Graph answers it with a redirect to a storage host that carries
        // no eTag, leaving nothing to make the next write conditional on.
        let item = match self
            .agent
            .get(&format!("{ITEM_URL}?$select=eTag"))
            .set("Authorization", &format!("Bearer {token}"))
            .call()
        {
            Ok(response) => response,
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(error) => return Err(remote_error(PROVIDER, error)),
        };
        let item: DriveItem = item
            .into_json()
            .map_err(|error| SyncError::Malformed(error.to_string()))?;

        let body = match self
            .agent
            .get(&format!("{ITEM_URL}:/content"))
            .set("Authorization", &format!("Bearer {token}"))
            .call()
        {
            Ok(response) => response.into_string().map_err(SyncError::Io)?,
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(error) => return Err(remote_error(PROVIDER, error)),
        };
        if body.trim().is_empty() {
            return Ok(None);
        }

        Ok(Some((
            SyncDocument::parse(body.as_bytes())?,
            RemoteRevision { token: item.etag },
        )))
    }

    fn push(
        &mut self,
        document: &SyncDocument,
        expected: Option<&RemoteRevision>,
    ) -> Result<RemoteRevision, SyncError> {
        let token = self.session.access_token()?;
        let bytes = document.to_bytes()?;

        let mut request = self
            .agent
            .put(&format!("{ITEM_URL}:/content"))
            .set("Authorization", &format!("Bearer {token}"))
            .set("Content-Type", "application/json");
        // `*` on a first write means "only if it does not exist", so two
        // machines starting at once cannot both believe they created it.
        request = match expected.map(|revision| revision.token.as_str()) {
            Some(etag) if !etag.is_empty() => request.set("if-match", etag),
            _ => request.set("if-none-match", "*"),
        };

        let response = match request.send_bytes(&bytes) {
            Ok(response) => response,
            Err(ureq::Error::Status(412 | 409, _)) => {
                return Err(SyncError::Conflict {
                    provider: PROVIDER.to_owned(),
                });
            }
            Err(error) => return Err(remote_error(PROVIDER, error)),
        };

        let item: DriveItem = response
            .into_json()
            .map_err(|error| SyncError::Malformed(error.to_string()))?;
        Ok(RemoteRevision { token: item.etag })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both parts matter and both fail late: without `offline_access` there is
    /// no refresh token, and the app-folder scope is what keeps the token away
    /// from the rest of the user's OneDrive.
    #[test]
    fn the_scope_asks_for_offline_access_and_only_the_app_folder() {
        let config = oauth_config("id".to_owned());
        assert!(config.scope.contains("offline_access"));
        assert!(config.scope.contains("Files.ReadWrite.AppFolder"));
        assert!(
            !config.scope.contains("Files.ReadWrite.All"),
            "must never ask for the whole drive"
        );
    }

    /// `common` is what admits personal Microsoft accounts; a tenant id here
    /// would turn away exactly the users this is for.
    #[test]
    fn the_endpoint_admits_personal_accounts() {
        let config = oauth_config("id".to_owned());
        assert!(config.auth_url.contains("/common/"));
        assert!(config.token_url.contains("/common/"));
    }

    #[test]
    fn an_unconnected_account_is_caught_locally() {
        let mut backend = OneDriveBackend::new("id".to_owned(), String::new());
        assert!(matches!(
            backend.fetch(),
            Err(SyncError::NotAuthenticated { .. })
        ));
    }
}
