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
        platform_config_dir().join("profiles.json")
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
}

fn default_auto_reconnect() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            dark_mode: false,
            collapsed_groups: Vec::new(),
            window_width: 1360.0,
            window_height: 860.0,
            auto_reconnect: true,
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
        platform_config_dir().join("settings.json")
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
    let mut normalized = BTreeSet::new();

    for group in groups {
        let group = normalize_group_name(group);
        if !group.is_empty() {
            normalized.insert(group);
        }
    }

    for profile in profiles {
        let group = normalize_group_name(&profile.group);
        if !group.is_empty() {
            normalized.insert(group);
        }
    }

    if normalized.is_empty() {
        normalized.insert(String::from("Default"));
    }

    normalized.into_iter().collect()
}

fn normalize_group_name(group: impl AsRef<str>) -> String {
    group.as_ref().trim().to_string()
}

/// Default directory for session output (transcript) logs.
#[must_use]
pub fn default_log_dir() -> PathBuf {
    platform_config_dir().join("logs")
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

        assert_eq!(
            loaded.groups,
            vec![String::from("Empty"), String::from("Lab")]
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
