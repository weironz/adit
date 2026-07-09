use adit_domain::{ConnectionProfile, ProfileId};
use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("profile json failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credential store failed: {0}")]
    Keyring(#[from] keyring::Error),
}

#[derive(Debug, Clone)]
pub struct ProfileStore {
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CredentialStore {
    service: String,
}

#[derive(Debug, Clone, Default)]
pub struct ProfileCatalog {
    pub groups: Vec<String>,
    pub profiles: Vec<ConnectionProfile>,
}

impl ProfileCatalog {
    #[must_use]
    pub fn new(groups: Vec<String>, profiles: Vec<ConnectionProfile>) -> Self {
        Self {
            groups: normalize_groups(groups, &profiles),
            profiles,
        }
    }

    #[must_use]
    pub fn from_profiles(profiles: Vec<ConnectionProfile>) -> Self {
        Self::new(Vec::new(), profiles)
    }
}

impl ProfileStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn default_path() -> PathBuf {
        config_dir().join("profiles.json")
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load_profiles(&self) -> Result<Vec<ConnectionProfile>, StorageError> {
        Ok(self.load_catalog()?.profiles)
    }

    pub fn load_catalog(&self) -> Result<ProfileCatalog, StorageError> {
        if !self.path.exists() {
            return Ok(ProfileCatalog::default());
        }

        let content = fs::read_to_string(&self.path)?;
        if content.trim().is_empty() {
            return Ok(ProfileCatalog::default());
        }

        let document: StoredProfiles = serde_json::from_str(&content)?;
        Ok(ProfileCatalog::new(document.groups, document.profiles))
    }

    pub fn save_profiles(&self, profiles: &[ConnectionProfile]) -> Result<(), StorageError> {
        self.save_catalog(&ProfileCatalog::from_profiles(profiles.to_vec()))
    }

    pub fn save_catalog(&self, catalog: &ProfileCatalog) -> Result<(), StorageError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let groups = normalize_groups(catalog.groups.clone(), &catalog.profiles);
        let document = StoredProfiles {
            version: 2,
            groups,
            profiles: catalog.profiles.clone(),
        };
        let content = serde_json::to_string_pretty(&document)?;
        fs::write(&self.path, content)?;

        Ok(())
    }
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self::new(Self::default_path())
    }
}

impl CredentialStore {
    #[must_use]
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    pub fn load_profile_password(
        &self,
        profile_id: ProfileId,
    ) -> Result<Option<String>, CredentialError> {
        match self.profile_password_entry(profile_id)?.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save_profile_password(
        &self,
        profile_id: ProfileId,
        password: &str,
    ) -> Result<(), CredentialError> {
        self.profile_password_entry(profile_id)?
            .set_password(password)
            .map_err(Into::into)
    }

    pub fn delete_profile_password(&self, profile_id: ProfileId) -> Result<(), CredentialError> {
        match self.profile_password_entry(profile_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn profile_password_entry(&self, profile_id: ProfileId) -> Result<Entry, CredentialError> {
        Ok(Entry::new(
            &self.service,
            &format!("profile:{profile_id}:password"),
        )?)
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self::new("Adit SSH")
    }
}

/// Persisted application/UI preferences: anything that should survive a restart
/// but is not a connection profile or a secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSettings {
    pub dark_mode: bool,
    #[serde(default)]
    pub collapsed_groups: Vec<String>,
    pub window_width: f32,
    pub window_height: f32,
    #[serde(default = "default_auto_reconnect")]
    pub auto_reconnect: bool,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    #[serde(default = "default_sidebar_visible")]
    pub sidebar_visible: bool,
    /// Terminal font family (a display name from the UI's preset list, or empty
    /// for the system monospace default).
    #[serde(default)]
    pub font_family: String,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    /// Terminal color scheme name (from the UI's built-in list; empty/unknown
    /// falls back to the default palette).
    #[serde(default)]
    pub color_scheme: String,
    /// Session-log folder; empty ⇒ [`default_log_dir`].
    #[serde(default)]
    pub log_dir: String,
    /// Log filename pattern with `%N/%H/%Y/%M/%D/%h/%m/%s` tokens; empty ⇒ the
    /// UI's built-in default pattern.
    #[serde(default)]
    pub log_name_pattern: String,
    /// Automatically start logging a session as soon as it connects.
    #[serde(default)]
    pub auto_log_on_connect: bool,
    /// Write logs as plain text (escape sequences stripped) instead of the raw
    /// PTY transcript.
    #[serde(default)]
    pub log_plaintext: bool,
    /// Selecting text in the terminal copies it to the clipboard on release
    /// (PuTTY-style), without an explicit copy command.
    #[serde(default)]
    pub copy_on_select: bool,
    /// A right-click in the terminal pastes the clipboard immediately instead of
    /// opening the context menu (PuTTY-style).
    #[serde(default)]
    pub right_click_paste: bool,
    /// Confirm before pasting clipboard text that spans multiple lines (guards
    /// against accidentally executing a pasted command block).
    #[serde(default = "default_true")]
    pub confirm_multiline_paste: bool,
    /// Abort a connection attempt after this many seconds (0 disables the cap).
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u32,
    /// Scrollback history size (lines kept per terminal).
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: u32,
    /// Reusable command snippets sent to the active session on demand.
    #[serde(default)]
    pub snippets: Vec<Snippet>,
    /// Check GitHub for a newer release on startup (only surfaces if one exists).
    #[serde(default)]
    pub auto_check_updates: bool,
    /// Show the bottom command window (a line-oriented send box, SecureCRT-style).
    #[serde(default)]
    pub command_window_open: bool,
    /// Command window sends each typed character to the target immediately,
    /// instead of only sending the whole line on Enter.
    #[serde(default)]
    pub command_send_immediately: bool,
}

/// A saved command snippet (a name + the command text sent to a session).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    pub name: String,
    pub command: String,
}

fn default_connect_timeout() -> u32 {
    20
}

fn default_scrollback_lines() -> u32 {
    5000
}

fn default_true() -> bool {
    true
}

fn default_auto_reconnect() -> bool {
    true
}

fn default_sidebar_width() -> f32 {
    348.0
}

fn default_sidebar_visible() -> bool {
    true
}

fn default_font_size() -> f32 {
    13.0
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            // Dark-first, matching the Termius-style look.
            dark_mode: true,
            collapsed_groups: Vec::new(),
            window_width: 1360.0,
            window_height: 860.0,
            auto_reconnect: true,
            sidebar_width: default_sidebar_width(),
            sidebar_visible: default_sidebar_visible(),
            font_family: String::new(),
            font_size: default_font_size(),
            color_scheme: String::new(),
            log_dir: String::new(),
            log_name_pattern: String::new(),
            auto_log_on_connect: false,
            log_plaintext: false,
            copy_on_select: false,
            right_click_paste: false,
            confirm_multiline_paste: true,
            connect_timeout_secs: default_connect_timeout(),
            scrollback_lines: default_scrollback_lines(),
            snippets: Vec::new(),
            auto_check_updates: false,
            command_window_open: false,
            command_send_immediately: false,
        }
    }
}

/// JSON-backed store for [`AppSettings`], saved next to the profile store.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn default_path() -> PathBuf {
        config_dir().join("settings.json")
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<AppSettings, StorageError> {
        if !self.path.exists() {
            return Ok(AppSettings::default());
        }

        let content = fs::read_to_string(&self.path)?;
        if content.trim().is_empty() {
            return Ok(AppSettings::default());
        }

        let document: StoredSettings = serde_json::from_str(&content)?;
        Ok(document.settings)
    }

    pub fn save(&self, settings: &AppSettings) -> Result<(), StorageError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let document = StoredSettings {
            version: 1,
            settings: settings.clone(),
        };
        let content = serde_json::to_string_pretty(&document)?;
        fs::write(&self.path, content)?;

        Ok(())
    }
}

impl Default for SettingsStore {
    fn default() -> Self {
        Self::new(Self::default_path())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSettings {
    version: u16,
    settings: AppSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredProfiles {
    version: u16,
    #[serde(default)]
    groups: Vec<String>,
    profiles: Vec<ConnectionProfile>,
}

fn normalize_groups(groups: Vec<String>, profiles: &[ConnectionProfile]) -> Vec<String> {
    // Preserve the given folder order (it is user-arrangeable); dedup keeping the
    // first occurrence, then append any groups seen only on profiles.
    let mut ordered: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    let push = |group: String, ordered: &mut Vec<String>, seen: &mut BTreeSet<String>| {
        let group = normalize_group_name(group);
        if !group.is_empty() && seen.insert(group.clone()) {
            ordered.push(group);
        }
    };

    for group in groups {
        push(group, &mut ordered, &mut seen);
    }
    for profile in profiles {
        push(profile.group.clone(), &mut ordered, &mut seen);
    }

    ordered
}

fn normalize_group_name(group: impl AsRef<str>) -> String {
    group.as_ref().trim().to_string()
}

/// The active configuration folder — where `profiles.json`, `settings.json`,
/// logs, and downloads live. Honors the `ADIT_CONFIG_DIR` environment override
/// (SecureCRT-style relocatable config, e.g. onto a synced folder); otherwise
/// the per-platform default.
#[must_use]
pub fn config_dir() -> PathBuf {
    if let Some(dir) = env::var_os("ADIT_CONFIG_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return dir;
        }
    }
    platform_config_dir()
}

/// Default directory for session output (transcript) logs.
#[must_use]
pub fn default_log_dir() -> PathBuf {
    config_dir().join("logs")
}

/// Default directory for SFTP downloads.
#[must_use]
pub fn default_download_dir() -> PathBuf {
    config_dir().join("downloads")
}

/// The current user's home directory (`USERPROFILE` on Windows, else `HOME`).
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// The current OS user name, used as the default SSH user when a `~/.ssh/config`
/// host omits `User`.
#[must_use]
pub fn current_username() -> Option<String> {
    env::var("USERNAME")
        .ok()
        .or_else(|| env::var("USER").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Path to the OpenSSH client config (`~/.ssh/config`).
#[must_use]
pub fn ssh_config_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".ssh").join("config"))
}

/// A host entry parsed from `~/.ssh/config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshConfigHost {
    /// The `Host` alias (a concrete, non-wildcard pattern).
    pub alias: String,
    /// `HostName`, or the alias when `HostName` is absent.
    pub hostname: String,
    /// `User` (empty ⇒ caller substitutes the current OS user).
    pub user: String,
    /// `Port` (defaults to 22).
    pub port: u16,
    /// `IdentityFile` with a leading `~` expanded (empty when absent).
    pub identity_file: String,
}

/// Parse an OpenSSH client config into concrete host entries. Wildcard patterns
/// (`*`, `?`, negations) and `Match` blocks are skipped; only literal `Host`
/// aliases become entries.
#[must_use]
pub fn parse_ssh_config(text: &str) -> Vec<SshConfigHost> {
    let mut hosts = Vec::new();
    let mut patterns: Vec<String> = Vec::new();
    let mut hostname = String::new();
    let mut user = String::new();
    let mut port: u16 = 22;
    let mut identity = String::new();

    fn flush(
        patterns: &[String],
        hostname: &str,
        user: &str,
        port: u16,
        identity: &str,
        out: &mut Vec<SshConfigHost>,
    ) {
        for alias in patterns {
            let hostname = if hostname.is_empty() {
                alias.clone()
            } else {
                hostname.to_string()
            };
            out.push(SshConfigHost {
                alias: alias.clone(),
                hostname,
                user: user.to_string(),
                port,
                identity_file: identity.to_string(),
            });
        }
    }

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = split_ssh_kv(line);
        match key.to_ascii_lowercase().as_str() {
            "host" => {
                flush(&patterns, &hostname, &user, port, &identity, &mut hosts);
                patterns = value
                    .split_whitespace()
                    .filter(|p| !p.contains('*') && !p.contains('?') && !p.starts_with('!'))
                    .map(str::to_string)
                    .collect();
                hostname = String::new();
                user = String::new();
                port = 22;
                identity = String::new();
            }
            // A Match block ends the current Host's applicability.
            "match" => {
                flush(&patterns, &hostname, &user, port, &identity, &mut hosts);
                patterns.clear();
            }
            _ if patterns.is_empty() => {}
            "hostname" => hostname = unquote(value).to_string(),
            "user" => user = unquote(value).to_string(),
            "port" => port = unquote(value).parse().unwrap_or(22),
            "identityfile" => identity = expand_tilde(unquote(value)),
            _ => {}
        }
    }
    flush(&patterns, &hostname, &user, port, &identity, &mut hosts);
    hosts
}

/// Split an ssh_config line into its keyword and value (`Key value` or `Key=value`).
fn split_ssh_kv(line: &str) -> (&str, &str) {
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let key = &line[..end];
    let value = line[end..]
        .trim_start_matches(|c: char| c.is_whitespace() || c == '=')
        .trim();
    (key, value)
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value)
}

fn expand_tilde(path: &str) -> String {
    let path = path.trim();
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = home_dir() {
            return home.join(rest).display().to_string();
        }
    }
    path.to_string()
}

fn platform_config_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        if let Some(app_data) = env::var_os("APPDATA") {
            return PathBuf::from(app_data).join("Adit");
        }
    }

    if cfg!(target_os = "macos") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Adit");
        }
    }

    if let Some(xdg_config_home) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg_config_home).join("adit");
    }

    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("adit");
    }

    PathBuf::from(".").join(".adit")
}

#[cfg(test)]
mod tests {
    use super::*;
    use adit_domain::AuthMethod;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_ssh_config_hosts() {
        let text = "\
# comment
Host *
  User ignored

Host web1 web2
  HostName 10.0.0.5
  User deploy
  Port 2222

Host db
  User root
";
        let hosts = parse_ssh_config(text);
        // Wildcard Host * is skipped; web1/web2 share the block options; db uses
        // its alias as the hostname (no HostName) and default port 22.
        assert_eq!(hosts.len(), 3);
        assert_eq!(hosts[0].alias, "web1");
        assert_eq!(hosts[0].hostname, "10.0.0.5");
        assert_eq!(hosts[0].user, "deploy");
        assert_eq!(hosts[0].port, 2222);
        assert_eq!(hosts[1].alias, "web2");
        assert_eq!(hosts[1].hostname, "10.0.0.5");
        assert_eq!(hosts[2].alias, "db");
        assert_eq!(hosts[2].hostname, "db");
        assert_eq!(hosts[2].port, 22);
    }

    #[test]
    fn config_dir_honors_env_override() {
        let base = env::temp_dir().join("adit-cfgdir-override-test");
        env::set_var("ADIT_CONFIG_DIR", &base);

        assert_eq!(config_dir(), base);
        assert_eq!(ProfileStore::default_path(), base.join("profiles.json"));
        assert_eq!(SettingsStore::default_path(), base.join("settings.json"));
        assert_eq!(default_log_dir(), base.join("logs"));
        assert_eq!(default_download_dir(), base.join("downloads"));

        env::remove_var("ADIT_CONFIG_DIR");
        // With the override cleared, the config dir is no longer that base.
        assert_ne!(config_dir(), base);
    }

    #[test]
    fn saves_and_loads_profiles() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let path = env::temp_dir()
            .join(format!("adit-storage-test-{unique}"))
            .join("profiles.json");
        let store = ProfileStore::new(&path);
        let profiles = vec![
            ConnectionProfile::with_group("Local", "local-lab", "127.0.0.1", 22, "root"),
            ConnectionProfile::with_group("Prod", "web-01", "10.0.0.12", 22, "deploy"),
        ];

        store
            .save_profiles(&profiles)
            .expect("profiles should save");
        let loaded = store.load_profiles().expect("profiles should load");

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].group, "Local");
        assert_eq!(loaded[0].name, "local-lab");
        assert_eq!(loaded[1].group, "Prod");
        assert_eq!(loaded[1].endpoint(), "deploy@10.0.0.12:22");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn saves_and_loads_empty_groups() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let path = env::temp_dir()
            .join(format!("adit-storage-groups-test-{unique}"))
            .join("profiles.json");
        let store = ProfileStore::new(&path);
        let catalog = ProfileCatalog::new(
            vec![String::from("Lab"), String::from("Empty")],
            vec![ConnectionProfile::with_group(
                "Lab",
                "local-lab",
                "127.0.0.1",
                22,
                "root",
            )],
        );

        store.save_catalog(&catalog).expect("catalog should save");
        let loaded = store.load_catalog().expect("catalog should load");

        // Folder order is preserved as given (user-arrangeable), not alphabetized.
        assert_eq!(
            loaded.groups,
            vec![String::from("Lab"), String::from("Empty")]
        );
        assert_eq!(loaded.profiles.len(), 1);
        assert_eq!(loaded.profiles[0].group, "Lab");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn saves_and_loads_settings() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let path = env::temp_dir()
            .join(format!("adit-settings-test-{unique}"))
            .join("settings.json");
        let store = SettingsStore::new(&path);

        // Missing file falls back to defaults.
        assert_eq!(store.load().expect("default load"), AppSettings::default());

        let settings = AppSettings {
            dark_mode: true,
            collapsed_groups: vec![String::from("Lab"), String::from("Prod")],
            window_width: 1500.0,
            window_height: 900.0,
            auto_reconnect: false,
            sidebar_width: 300.0,
            sidebar_visible: false,
            font_family: String::from("Consolas"),
            font_size: 15.0,
            color_scheme: String::from("Dracula"),
            log_dir: String::from("D:/logs"),
            log_name_pattern: String::from("%N-%Y%M%D.log"),
            auto_log_on_connect: true,
            log_plaintext: true,
            copy_on_select: true,
            right_click_paste: true,
            confirm_multiline_paste: false,
            connect_timeout_secs: 30,
            scrollback_lines: 8000,
            snippets: vec![Snippet {
                name: String::from("uptime"),
                command: String::from("uptime"),
            }],
            auto_check_updates: true,
            command_window_open: true,
            command_send_immediately: true,
        };
        store.save(&settings).expect("settings should save");
        let loaded = store.load().expect("settings should load");

        assert_eq!(loaded, settings);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn loads_legacy_profiles_without_auth_fields() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let path = env::temp_dir()
            .join(format!("adit-storage-legacy-test-{unique}"))
            .join("profiles.json");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("test directory should be created");
        }
        fs::write(
            &path,
            r#"{
              "version": 1,
              "profiles": [
                {
                  "id": "48a38d24-2d5e-459d-9ecb-536844dce1d2",
                  "folder": "Legacy",
                  "name": "old-host",
                  "host": "192.168.1.10",
                  "port": 22,
                  "username": "root"
                }
              ]
            }"#,
        )
        .expect("legacy profile json should be written");

        let profiles = ProfileStore::new(&path)
            .load_profiles()
            .expect("legacy profiles should load");

        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].auth_method, AuthMethod::Auto);
        assert!(profiles[0].identity_file.is_empty());
        assert_eq!(profiles[0].sort_order, 0);
        assert_eq!(profiles[0].group, "Legacy");

        let _ = fs::remove_file(path);
    }
}
