use adit_domain::ConnectionProfile;
use serde::{Deserialize, Serialize};
use std::{
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

#[derive(Debug, Clone)]
pub struct ProfileStore {
    path: PathBuf,
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
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&self.path)?;
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }

        let document: StoredProfiles = serde_json::from_str(&content)?;
        Ok(document.profiles)
    }

    pub fn save_profiles(&self, profiles: &[ConnectionProfile]) -> Result<(), StorageError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let document = StoredProfiles {
            version: 1,
            profiles: profiles.to_vec(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredProfiles {
    version: u16,
    profiles: Vec<ConnectionProfile>,
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
            ConnectionProfile::with_folder("Local", "local-lab", "127.0.0.1", 22, "root"),
            ConnectionProfile::with_folder("Prod", "web-01", "10.0.0.12", 22, "deploy"),
        ];

        store
            .save_profiles(&profiles)
            .expect("profiles should save");
        let loaded = store.load_profiles().expect("profiles should load");

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].folder, "Local");
        assert_eq!(loaded[0].name, "local-lab");
        assert_eq!(loaded[1].folder, "Prod");
        assert_eq!(loaded[1].endpoint(), "deploy@10.0.0.12:22");

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

        let _ = fs::remove_file(path);
    }
}
