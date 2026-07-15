//! Credential storage: secrets encrypted at rest in Adit's own config folder.
//!
//! Passwords used to live in the OS keyring, which is machine-local — so a config
//! folder synced between machines (Dropbox, etc.) arrived without its passwords
//! and every host had to be re-entered. Secrets now sit next to `profiles.json`
//! in [`crate::config_dir`], which the user can point at a synced folder, so they
//! travel with the profiles (this is what SecureCRT does).
//!
//! # Security model — read this before touching the crypto
//!
//! The file is sealed with XChaCha20-Poly1305 under a key derived by Argon2id.
//! The KDF input is a **key built into the binary**, not a user secret. That is a
//! deliberate, user-chosen trade-off: it makes the store sync across machines with
//! zero setup, and it stops a password from being read by casually opening the
//! file, landing in a backup as plaintext, or being scraped out of a synced
//! folder by an unrelated tool. It is **obfuscation, not secrecy**: anyone who has
//! the file *and* the (open-source) built-in key can recover every password. Do
//! not describe this as protecting secrets from someone who has the file.
//!
//! The KDF deliberately takes a `master` input so a real user passphrase can be
//! mixed in later without changing the file format — that, and only that, would
//! make the store secret rather than merely obfuscated.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use adit_domain::ProfileId;
use argon2::Argon2;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::config_dir;

/// On-disk format version, so a future change can migrate rather than guess.
const FORMAT_VERSION: u32 = 1;

/// File name inside the config folder.
pub(crate) const CREDENTIALS_FILE: &str = "credentials.json";

/// The built-in KDF input. See the module docs: this is obfuscation, and being in
/// a public binary is inherent to the "no master password" choice — it is not an
/// accident to be "fixed" by hiding it better.
const BUILT_IN_KEY: &[u8] = b"adit.credential.store.v1/built-in-key";

/// Argon2id salt length in bytes.
const SALT_LEN: usize = 16;

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credential store io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("credential store is corrupt: {0}")]
    Json(#[from] serde_json::Error),
    #[error("credential store key derivation failed: {0}")]
    Kdf(String),
    #[error("credential could not be decrypted (wrong key or corrupt entry)")]
    Decrypt,
    #[error("credential could not be encrypted")]
    Encrypt,
    #[error("credential store is not readable: {0}")]
    Corrupt(String),
    #[error("legacy keyring access failed: {0}")]
    Keyring(#[from] keyring::Error),
}

/// One sealed secret. The nonce is random per write, so re-saving the same
/// password never produces the same ciphertext.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sealed {
    nonce: String,
    ciphertext: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CredentialFile {
    version: u32,
    /// Per-file Argon2id salt (hex). Random at creation, then stable.
    salt: String,
    /// Account key (`profile:<id>:password`) → sealed secret.
    #[serde(default)]
    entries: BTreeMap<String, Sealed>,
}

impl CredentialFile {
    fn create() -> Self {
        let mut salt = [0u8; SALT_LEN];
        // `OsRng` here is the AEAD crate's re-export; `generate_nonce` uses the
        // same source, so both the salt and nonces come from the OS CSPRNG.
        use chacha20poly1305::aead::rand_core::RngCore;
        OsRng.fill_bytes(&mut salt);
        Self {
            version: FORMAT_VERSION,
            salt: hex::encode(salt),
            entries: BTreeMap::new(),
        }
    }
}

/// Secrets for connection profiles, encrypted at rest in the config folder.
///
/// Cloneable and cheap: clones share one cached, lazily-loaded file.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
    /// Legacy OS-keyring service name, kept only to migrate old secrets across.
    service: String,
    cache: Arc<Mutex<Option<CredentialFile>>>,
}

impl CredentialStore {
    #[must_use]
    pub fn new(service: impl Into<String>) -> Self {
        Self::with_path(service, config_dir().join(CREDENTIALS_FILE))
    }

    #[must_use]
    pub fn with_path(service: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            service: service.into(),
            cache: Arc::new(Mutex::new(None)),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn default_path() -> PathBuf {
        config_dir().join(CREDENTIALS_FILE)
    }

    pub fn load_profile_password(
        &self,
        profile_id: ProfileId,
    ) -> Result<Option<String>, CredentialError> {
        self.get(&password_account(profile_id))
    }

    pub fn save_profile_password(
        &self,
        profile_id: ProfileId,
        password: &str,
    ) -> Result<(), CredentialError> {
        self.set(&password_account(profile_id), password)
    }

    pub fn delete_profile_password(&self, profile_id: ProfileId) -> Result<(), CredentialError> {
        self.remove(&password_account(profile_id))
    }

    pub fn load_profile_passphrase(
        &self,
        profile_id: ProfileId,
    ) -> Result<Option<String>, CredentialError> {
        self.get(&passphrase_account(profile_id))
    }

    pub fn save_profile_passphrase(
        &self,
        profile_id: ProfileId,
        passphrase: &str,
    ) -> Result<(), CredentialError> {
        self.set(&passphrase_account(profile_id), passphrase)
    }

    pub fn delete_profile_passphrase(&self, profile_id: ProfileId) -> Result<(), CredentialError> {
        self.remove(&passphrase_account(profile_id))
    }

    /// Pull any secrets an older build left in the OS keyring into the encrypted
    /// file, so upgrading doesn't look like every password was lost. Existing file
    /// entries always win (the file is the source of truth once written), and the
    /// keyring copy is left alone rather than deleted — a downgrade should still
    /// work, and deleting is not ours to do silently.
    ///
    /// Returns how many secrets were imported. Keyring errors are ignored: a
    /// machine with no/locked keyring must not block startup.
    pub fn migrate_from_keyring(&self, profile_ids: &[ProfileId]) -> usize {
        let mut imported = 0;
        for &profile_id in profile_ids {
            for (account, legacy) in [
                (password_account(profile_id), format!("profile:{profile_id}:password")),
                (
                    passphrase_account(profile_id),
                    format!("profile:{profile_id}:passphrase"),
                ),
            ] {
                // Never overwrite something already in the file.
                if matches!(self.get(&account), Ok(Some(_))) {
                    continue;
                }
                let Ok(entry) = keyring::Entry::new(&self.service, &legacy) else {
                    continue;
                };
                if let Ok(secret) = entry.get_password() {
                    if self.set(&account, &secret).is_ok() {
                        imported += 1;
                    }
                }
            }
        }
        imported
    }

    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        let mut guard = self.lock()?;
        let file = self.ensure_loaded(&mut guard)?;
        let Some(sealed) = file.entries.get(account).cloned() else {
            return Ok(None);
        };
        let key = derive_key(&file.salt)?;
        open(&key, &sealed).map(Some)
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        let mut guard = self.lock()?;
        let file = self.ensure_loaded(&mut guard)?;
        let key = derive_key(&file.salt)?;
        let sealed = seal(&key, secret)?;
        file.entries.insert(account.to_owned(), sealed);
        self.persist(file)
    }

    fn remove(&self, account: &str) -> Result<(), CredentialError> {
        let mut guard = self.lock()?;
        let file = self.ensure_loaded(&mut guard)?;
        if file.entries.remove(account).is_none() {
            return Ok(());
        }
        self.persist(file)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<CredentialFile>>, CredentialError> {
        self.cache
            .lock()
            .map_err(|_| CredentialError::Corrupt(String::from("credential store lock poisoned")))
    }

    /// Load the file into the cache on first use, creating an empty one when absent.
    fn ensure_loaded<'a>(
        &self,
        guard: &'a mut std::sync::MutexGuard<'_, Option<CredentialFile>>,
    ) -> Result<&'a mut CredentialFile, CredentialError> {
        if guard.is_none() {
            **guard = Some(read_file(&self.path)?);
        }
        Ok(guard
            .as_mut()
            .expect("credential file was just loaded above"))
    }

    /// Write the file atomically (temp + rename) so a crash or a sync client
    /// reading mid-write can never see a truncated store.
    fn persist(&self, file: &CredentialFile) -> Result<(), CredentialError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(file)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        restrict_permissions(&tmp);
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self::new("Adit SSH")
    }
}

fn password_account(profile_id: ProfileId) -> String {
    format!("profile:{profile_id}:password")
}

fn passphrase_account(profile_id: ProfileId) -> String {
    format!("profile:{profile_id}:passphrase")
}

fn read_file(path: &Path) -> Result<CredentialFile, CredentialError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let file: CredentialFile = serde_json::from_str(&text)?;
            if file.version > FORMAT_VERSION {
                return Err(CredentialError::Corrupt(format!(
                    "credential store version {} is newer than this build supports ({FORMAT_VERSION}); \
                     upgrade Adit rather than letting it overwrite the file",
                    file.version
                )));
            }
            Ok(file)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CredentialFile::create()),
        Err(error) => Err(error.into()),
    }
}

/// Derive the file key. `salt` is the file's hex salt.
///
/// The KDF input is [`BUILT_IN_KEY`]; a user passphrase would be appended here
/// (and only here) to turn obfuscation into real secrecy — the file format
/// already carries everything else that change would need.
fn derive_key(salt: &str) -> Result<Zeroizing<[u8; 32]>, CredentialError> {
    let salt = hex::decode(salt)
        .map_err(|error| CredentialError::Corrupt(format!("bad salt: {error}")))?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::default()
        .hash_password_into(BUILT_IN_KEY, &salt, key.as_mut())
        .map_err(|error| CredentialError::Kdf(error.to_string()))?;
    Ok(key)
}

fn seal(key: &[u8; 32], secret: &str) -> Result<Sealed, CredentialError> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, secret.as_bytes())
        .map_err(|_| CredentialError::Encrypt)?;
    Ok(Sealed {
        nonce: hex::encode(nonce),
        ciphertext: hex::encode(ciphertext),
    })
}

fn open(key: &[u8; 32], sealed: &Sealed) -> Result<String, CredentialError> {
    let nonce = hex::decode(&sealed.nonce).map_err(|_| CredentialError::Decrypt)?;
    if nonce.len() != 24 {
        return Err(CredentialError::Decrypt);
    }
    let ciphertext = hex::decode(&sealed.ciphertext).map_err(|_| CredentialError::Decrypt)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let plaintext = cipher
        .decrypt(XNonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| CredentialError::Decrypt)?;
    String::from_utf8(plaintext).map_err(|_| CredentialError::Decrypt)
}

/// Best-effort owner-only permissions. On Windows the file inherits the user's
/// profile ACL, which is already owner-scoped, so this is a no-op there.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store on its own file. `name` must be unique per test — these run in
    /// parallel, and a shared path would let them delete each other's file.
    fn temp_store(name: &str) -> (CredentialStore, PathBuf) {
        let mut path = std::env::temp_dir();
        path.push(format!("adit-cred-test-{}-{name}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        (CredentialStore::with_path("adit-test", &path), path)
    }

    #[test]
    fn round_trips_a_secret_through_the_file() {
        let (store, path) = temp_store("round_trip");
        let id = ProfileId::new();

        assert!(store.load_profile_password(id).unwrap().is_none());
        store.save_profile_password(id, "hunter2").unwrap();
        assert_eq!(
            store.load_profile_password(id).unwrap().as_deref(),
            Some("hunter2")
        );

        // A fresh store (cold cache) must read the same file back — this is the
        // whole point: another machine opens the synced file and gets the password.
        let reopened = CredentialStore::with_path("adit-test", &path);
        assert_eq!(
            reopened.load_profile_password(id).unwrap().as_deref(),
            Some("hunter2")
        );

        store.delete_profile_password(id).unwrap();
        assert!(store.load_profile_password(id).unwrap().is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn password_and_passphrase_do_not_collide() {
        let (store, path) = temp_store("no_collide");
        let id = ProfileId::new();
        store.save_profile_password(id, "the-password").unwrap();
        store.save_profile_passphrase(id, "the-passphrase").unwrap();

        assert_eq!(
            store.load_profile_password(id).unwrap().as_deref(),
            Some("the-password")
        );
        assert_eq!(
            store.load_profile_passphrase(id).unwrap().as_deref(),
            Some("the-passphrase")
        );

        // Deleting one must leave the other intact.
        store.delete_profile_password(id).unwrap();
        assert!(store.load_profile_password(id).unwrap().is_none());
        assert_eq!(
            store.load_profile_passphrase(id).unwrap().as_deref(),
            Some("the-passphrase")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn secrets_are_not_stored_in_plaintext() {
        let (store, path) = temp_store("plaintext");
        let id = ProfileId::new();
        store.save_profile_password(id, "super-secret-value").unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("super-secret-value"),
            "password must never hit the disk in plaintext"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn same_secret_seals_differently_each_write() {
        let key = [7u8; 32];
        let a = seal(&key, "same").unwrap();
        let b = seal(&key, "same").unwrap();
        // Random nonce per write ⇒ ciphertext must differ, so the file never
        // reveals that two profiles share a password.
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
        assert_eq!(open(&key, &a).unwrap(), "same");
        assert_eq!(open(&key, &b).unwrap(), "same");
    }

    #[test]
    fn a_tampered_entry_is_rejected_rather_than_returning_garbage() {
        let key = [9u8; 32];
        let mut sealed = seal(&key, "authentic").unwrap();
        // Flip a byte of ciphertext: Poly1305 must catch it.
        let mut raw = hex::decode(&sealed.ciphertext).unwrap();
        raw[0] ^= 0xff;
        sealed.ciphertext = hex::encode(raw);
        assert!(matches!(
            open(&key, &sealed),
            Err(CredentialError::Decrypt)
        ));
    }

    #[test]
    fn a_different_key_cannot_open_the_entry() {
        let sealed = seal(&[1u8; 32], "secret").unwrap();
        assert!(matches!(
            open(&[2u8; 32], &sealed),
            Err(CredentialError::Decrypt)
        ));
    }
}
