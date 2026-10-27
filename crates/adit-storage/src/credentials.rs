//! Credential storage: secrets encrypted at rest in Adit's own config folder.
//!
//! Passwords used to live in the OS keyring, which is machine-local — so a config
//! folder synced between machines (Dropbox, etc.) arrived without its passwords
//! and every host had to be re-entered. Secrets now sit next to `profiles.json`
//! in [`crate::config_dir`], which the user can point at a synced folder, so they
//! travel with the profiles (this is what SecureCRT does).
//!
//! Legacy v1 deliberately uses a public built-in key (obfuscation). Optional v2
//! derives a real secret key from a user master password and starts locked. See
//! `docs/credential-security.md` for migration, memory and recovery boundaries.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use adit_domain::ProfileId;
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::config_dir;

/// On-disk format version, so a future change can migrate rather than guess.
const FORMAT_VERSION: u32 = 2;
const MASTER_VERIFIER: &str = "Adit credential vault v2";

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
    #[error("凭据库已锁定，请先在设置中输入主密码解锁")]
    Locked,
    #[error("主密码错误或凭据文件已损坏")]
    WrongMasterPassword,
    #[error("主密码至少需要 12 个字符")]
    WeakMasterPassword,
    #[error("凭据文件已被其他进程或同步修改，请重新解锁后重试")]
    Conflict,
    #[error("凭据库保护模式不适用于此操作")]
    ProtectionMode,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verifier: Option<Sealed>,
}

/// Only ciphertext and a zeroizing derived key are cached; plaintext is returned
/// to the connection caller and is never retained by this store.
#[derive(Default)]
struct VaultState {
    file: Option<CredentialFile>,
    bytes: Option<Vec<u8>>,
    key: Option<Zeroizing<[u8; 32]>>,
}

impl std::fmt::Debug for VaultState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultState")
            .field("unlocked", &self.key.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialProtection {
    Legacy,
    Locked,
    Unlocked,
}

impl CredentialFile {
    fn create() -> Self {
        let mut salt = [0u8; SALT_LEN];
        // `OsRng` here is the AEAD crate's re-export; `generate_nonce` uses the
        // same source, so both the salt and nonces come from the OS CSPRNG.
        use chacha20poly1305::aead::rand_core::RngCore;
        OsRng.fill_bytes(&mut salt);
        Self {
            version: 1,
            salt: hex::encode(salt),
            entries: BTreeMap::new(),
            protection: None,
            verifier: None,
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
    cache: Arc<Mutex<VaultState>>,
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
            cache: Arc::new(Mutex::new(VaultState::default())),
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

    /// A secret that belongs to the application rather than to a profile —
    /// a cloud-sync token, a WebDAV password, an S3 secret key.
    ///
    /// Same sealed file, same Argon2id key: there is no reason for these to be
    /// less protected than a session password, and every reason not to let
    /// them land in `settings.json`, which syncs to the cloud by design.
    pub fn save_secret(&self, name: &str, value: &str) -> Result<(), CredentialError> {
        self.set(&secret_account(name), value)
    }

    pub fn load_secret(&self, name: &str) -> Result<Option<String>, CredentialError> {
        self.get(&secret_account(name))
    }

    pub fn delete_secret(&self, name: &str) -> Result<(), CredentialError> {
        self.remove(&secret_account(name))
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
                (
                    password_account(profile_id),
                    format!("profile:{profile_id}:password"),
                ),
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

    /// This also notices a file replaced by sync and drops the old unlocked key.
    pub fn protection_status(&self) -> Result<CredentialProtection, CredentialError> {
        let mut state = self.state()?;
        self.reload(&mut state)?;
        Ok(if state.file.as_ref().unwrap().version == 1 {
            CredentialProtection::Legacy
        } else if state.key.is_some() {
            CredentialProtection::Unlocked
        } else {
            CredentialProtection::Locked
        })
    }

    pub fn unlock(&self, master: &str) -> Result<(), CredentialError> {
        let mut state = self.state()?;
        self.reload(&mut state)?;
        let file = state.file.as_ref().unwrap();
        if file.version == 1 {
            return Err(CredentialError::ProtectionMode);
        }
        let key = master_key(file, master)?;
        // Validate every entry before reporting success, even when the verifier
        // is intact but a cloud-sync merge damaged one account.
        for (account, entry) in &file.entries {
            let _plaintext = Zeroizing::new(open_entry(&key, entry, file.version, account)?);
        }
        state.key = Some(key);
        Ok(())
    }

    /// Clones share the key, so locking one handle locks all future retrievals.
    /// Existing live sessions own their own authentication copies and continue.
    pub fn lock_vault(&self) -> Result<(), CredentialError> {
        self.state()?.key = None;
        Ok(())
    }

    /// Explicit, reversible migration. The returned backup is encrypted using
    /// the OLD mode; a legacy backup therefore remains weakly protected.
    pub fn enable_master_password(&self, master: &str) -> Result<PathBuf, CredentialError> {
        validate_master(master)?;
        self.rekey(None, Some(master))
    }

    pub fn change_master_password(
        &self,
        current: &str,
        new: &str,
    ) -> Result<PathBuf, CredentialError> {
        validate_master(new)?;
        self.rekey(Some(current), Some(new))
    }

    pub fn disable_master_password(&self, current: &str) -> Result<PathBuf, CredentialError> {
        self.rekey(Some(current), None)
    }

    /// Export ciphertext only, without overwriting an existing chosen file.
    pub fn export_encrypted(&self, destination: &Path) -> Result<(), CredentialError> {
        let mut state = self.state()?;
        self.reload(&mut state)?;
        let bytes = state
            .bytes
            .as_ref()
            .ok_or(CredentialError::ProtectionMode)?;
        write_new_synced(destination, bytes)
    }

    /// Explicit transfer/recovery. Password and every account are verified
    /// before backup/replacement. Import leaves a master vault locked.
    pub fn import_encrypted(
        &self,
        source: &Path,
        master: Option<&str>,
    ) -> Result<PathBuf, CredentialError> {
        let bytes = std::fs::read(source)?;
        let candidate = decode_file(&bytes)?;
        let key = if candidate.version == 2 {
            master_key(&candidate, master.ok_or(CredentialError::Locked)?)?
        } else {
            derive_key(&candidate.salt)?
        };
        for (account, entry) in &candidate.entries {
            let _plaintext = Zeroizing::new(open_entry(&key, entry, candidate.version, account)?);
        }
        let mut state = self.state()?;
        self.reload(&mut state)?;
        // Import must not bypass the current vault lock to destroy its entries.
        let _current_key = state_key(&state)?;
        let backup = backup_verified(&self.path, state.bytes.as_deref())?;
        self.commit(&mut state, candidate)?;
        state.key = None;
        Ok(backup)
    }

    fn rekey(&self, current: Option<&str>, next: Option<&str>) -> Result<PathBuf, CredentialError> {
        let mut state = self.state()?;
        self.reload(&mut state)?;
        let old = state.file.as_ref().unwrap();
        let old_key = match (old.version, current) {
            (1, None) => derive_key(&old.salt)?,
            (2, Some(master)) => master_key(old, master)?,
            _ => return Err(CredentialError::ProtectionMode),
        };
        let mut candidate = CredentialFile::create();
        let new_key = if let Some(master) = next {
            candidate.version = 2;
            candidate.protection = Some("argon2id-64m-t3-p1".to_owned());
            let key = derive_master_key(&candidate.salt, master)?;
            candidate.verifier = Some(seal(&key, MASTER_VERIFIER)?);
            key
        } else {
            derive_key(&candidate.salt)?
        };
        for (account, entry) in &old.entries {
            let plaintext = Zeroizing::new(open_entry(&old_key, entry, old.version, account)?);
            let sealed = seal_entry(&new_key, &plaintext, candidate.version, account)?;
            // Round-trip the actual candidate ciphertext before touching disk.
            if open_entry(&new_key, &sealed, candidate.version, account)? != *plaintext {
                return Err(CredentialError::Encrypt);
            }
            candidate.entries.insert(account.clone(), sealed);
        }
        let backup = backup_verified(&self.path, state.bytes.as_deref())?;
        self.commit(&mut state, candidate)?;
        state.key = next.map(|_| new_key);
        Ok(backup)
    }

    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        let mut state = self.state()?;
        self.reload(&mut state)?;
        let key = state_key(&state)?;
        let file = state.file.as_ref().unwrap();
        file.entries
            .get(account)
            .map(|s| open_entry(&key, s, file.version, account))
            .transpose()
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        let mut state = self.state()?;
        self.reload(&mut state)?;
        let key = state_key(&state)?;
        let mut candidate = state.file.as_ref().unwrap().clone();
        candidate.entries.insert(
            account.to_owned(),
            seal_entry(&key, secret, candidate.version, account)?,
        );
        self.commit(&mut state, candidate)
    }

    fn remove(&self, account: &str) -> Result<(), CredentialError> {
        let mut state = self.state()?;
        self.reload(&mut state)?;
        let _key = state_key(&state)?;
        let mut candidate = state.file.as_ref().unwrap().clone();
        if candidate.entries.remove(account).is_none() {
            return Ok(());
        }
        self.commit(&mut state, candidate)
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, VaultState>, CredentialError> {
        self.cache
            .lock()
            .map_err(|_| CredentialError::Corrupt("credential store lock poisoned".into()))
    }

    fn reload(&self, state: &mut VaultState) -> Result<(), CredentialError> {
        let bytes = read_optional(&self.path)?;
        if state.bytes.is_some() && bytes.is_none() {
            state.key = None;
            return Err(CredentialError::Conflict);
        }
        if state.file.is_none() || state.bytes != bytes {
            state.key = None;
            state.file = Some(match bytes.as_deref() {
                Some(data) => decode_file(data)?,
                None => CredentialFile::create(),
            });
            state.bytes = bytes;
        }
        Ok(())
    }

    fn commit(
        &self,
        state: &mut VaultState,
        candidate: CredentialFile,
    ) -> Result<(), CredentialError> {
        let bytes = serde_json::to_vec_pretty(&candidate)?;
        // Serialize cooperating Adit processes around compare-and-replace. A
        // byte comparison without a cross-process lock still has a TOCTOU race.
        let lock_path = self.path.with_extension("json.lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        lock.lock()?;
        // Detect an external replacement before committing; never silently
        // write cached entries over a different vault or a sync conflict.
        if read_optional(&self.path)? != state.bytes {
            return Err(CredentialError::Conflict);
        }
        atomic_write(&self.path, &bytes)?;
        state.file = Some(candidate);
        state.bytes = Some(bytes);
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

fn secret_account(name: &str) -> String {
    format!("secret:{name}")
}

fn passphrase_account(profile_id: ProfileId) -> String {
    format!("profile:{profile_id}:passphrase")
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, CredentialError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn decode_file(bytes: &[u8]) -> Result<CredentialFile, CredentialError> {
    let file: CredentialFile = serde_json::from_slice(bytes)?;
    if !(1..=FORMAT_VERSION).contains(&file.version) {
        return Err(CredentialError::Corrupt(format!(
            "unsupported credential format {}",
            file.version
        )));
    }
    if hex::decode(&file.salt).map_or(true, |s| s.len() != SALT_LEN) {
        return Err(CredentialError::Corrupt("invalid salt".into()));
    }
    if file.version == 2
        && (file.protection.as_deref() != Some("argon2id-64m-t3-p1") || file.verifier.is_none())
    {
        return Err(CredentialError::Corrupt(
            "invalid master-password protection metadata".into(),
        ));
    }
    if file.version == 1 && (file.protection.is_some() || file.verifier.is_some()) {
        return Err(CredentialError::Corrupt(
            "inconsistent credential protection version".into(),
        ));
    }
    Ok(file)
}

fn state_key(state: &VaultState) -> Result<Zeroizing<[u8; 32]>, CredentialError> {
    let file = state.file.as_ref().unwrap();
    if file.version == 1 {
        derive_key(&file.salt)
    } else {
        state
            .key
            .as_ref()
            .map(|key| Zeroizing::new(**key))
            .ok_or(CredentialError::Locked)
    }
}

fn validate_master(master: &str) -> Result<(), CredentialError> {
    if master.chars().count() < 12 {
        Err(CredentialError::WeakMasterPassword)
    } else {
        Ok(())
    }
}

fn derive_master_key(salt: &str, master: &str) -> Result<Zeroizing<[u8; 32]>, CredentialError> {
    let salt = hex::decode(salt).map_err(|_| CredentialError::Decrypt)?;
    let params =
        Params::new(65536, 3, 1, Some(32)).map_err(|e| CredentialError::Kdf(e.to_string()))?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(master.as_bytes(), &salt, key.as_mut())
        .map_err(|e| CredentialError::Kdf(e.to_string()))?;
    Ok(key)
}

fn master_key(file: &CredentialFile, master: &str) -> Result<Zeroizing<[u8; 32]>, CredentialError> {
    let key = derive_master_key(&file.salt, master)?;
    let check = file
        .verifier
        .as_ref()
        .ok_or(CredentialError::WrongMasterPassword)?;
    let plaintext =
        Zeroizing::new(open(&key, check).map_err(|_| CredentialError::WrongMasterPassword)?);
    if plaintext.as_str() != MASTER_VERIFIER {
        return Err(CredentialError::WrongMasterPassword);
    }
    Ok(key)
}

fn unique_sibling(path: &Path, tag: &str) -> PathBuf {
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{tag}-{}", hex::encode(nonce)));
    PathBuf::from(name)
}

fn backup_verified(path: &Path, previous: Option<&[u8]>) -> Result<PathBuf, CredentialError> {
    let backup = unique_sibling(path, "backup");
    if let Some(bytes) = previous {
        write_new_synced(&backup, bytes)?;
        if std::fs::read(&backup)? != bytes {
            return Err(CredentialError::Corrupt(
                "backup verification failed".into(),
            ));
        }
    } else {
        // Empty initial stores still receive a valid, reversible v1 backup.
        write_new_synced(
            &backup,
            &serde_json::to_vec_pretty(&CredentialFile::create())?,
        )?;
    }
    Ok(backup)
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<(), CredentialError> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    restrict_permissions(path);
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CredentialError> {
    let tmp = unique_sibling(path, "tmp");
    write_new_synced(&tmp, bytes)?;
    if let Err(e) = replace_file(&tmp, path) {
        let _ = std::fs::remove_file(tmp);
        return Err(e.into());
    }
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(tmp: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, destination)
}

#[cfg(windows)]
fn replace_file(tmp: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let source: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // REPLACE_EXISTING | WRITE_THROUGH: never delete the old vault first.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0x1 | 0x8) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Derive the file key. `salt` is the file's hex salt.
///
/// Legacy v1 only. v2 uses the independent user-secret KDF above and must
/// never reach this built-in-key fallback.
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
    seal_entry(key, secret, 1, "")
}

fn seal_entry(
    key: &[u8; 32],
    secret: &str,
    version: u32,
    account: &str,
) -> Result<Sealed, CredentialError> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let aad = if version == 2 {
        account.as_bytes()
    } else {
        &[]
    };
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: secret.as_bytes(),
                aad,
            },
        )
        .map_err(|_| CredentialError::Encrypt)?;
    Ok(Sealed {
        nonce: hex::encode(nonce),
        ciphertext: hex::encode(ciphertext),
    })
}

fn open(key: &[u8; 32], sealed: &Sealed) -> Result<String, CredentialError> {
    open_entry(key, sealed, 1, "")
}

fn open_entry(
    key: &[u8; 32],
    sealed: &Sealed,
    version: u32,
    account: &str,
) -> Result<String, CredentialError> {
    let nonce = hex::decode(&sealed.nonce).map_err(|_| CredentialError::Decrypt)?;
    if nonce.len() != 24 {
        return Err(CredentialError::Decrypt);
    }
    let ciphertext = hex::decode(&sealed.ciphertext).map_err(|_| CredentialError::Decrypt)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let aad = if version == 2 {
        account.as_bytes()
    } else {
        &[]
    };
    let mut plaintext = Zeroizing::new(
        cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad,
                },
            )
            .map_err(|_| CredentialError::Decrypt)?,
    );
    let value = std::str::from_utf8(&plaintext)
        .map_err(|_| CredentialError::Decrypt)?
        .to_owned();
    use zeroize::Zeroize;
    plaintext.zeroize();
    Ok(value)
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
        store
            .save_profile_password(id, "super-secret-value")
            .unwrap();

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
        assert!(matches!(open(&key, &sealed), Err(CredentialError::Decrypt)));
    }

    #[test]
    fn a_different_key_cannot_open_the_entry() {
        let sealed = seal(&[1u8; 32], "secret").unwrap();
        assert!(matches!(
            open(&[2u8; 32], &sealed),
            Err(CredentialError::Decrypt)
        ));
    }

    const MASTER: &str = "synthetic master passphrase";

    #[test]
    fn master_migration_verifies_backup_and_rejects_builtin_key() {
        let (store, path) = temp_store("master_migrate");
        let ids = [ProfileId::new(), ProfileId::new(), ProfileId::new()];
        for (id, secret) in ids
            .iter()
            .zip(["ssh-synthetic", "rdp-synthetic", "vnc-synthetic"])
        {
            store.save_profile_password(*id, secret).unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let backup = store.enable_master_password(MASTER).unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), before);
        assert_eq!(
            store.protection_status().unwrap(),
            CredentialProtection::Unlocked
        );
        let file = decode_file(&std::fs::read(&path).unwrap()).unwrap();
        let old_key = derive_key(&file.salt).unwrap();
        for entry in file.entries.values() {
            assert!(open(&old_key, entry).is_err());
        }
        store.clone().lock_vault().unwrap();
        assert!(matches!(
            store.load_profile_password(ids[0]),
            Err(CredentialError::Locked)
        ));
        let after = std::fs::read(&path).unwrap();
        assert!(matches!(
            store.unlock("wrong secret"),
            Err(CredentialError::WrongMasterPassword)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), after);
        store.unlock(MASTER).unwrap();
        for (id, secret) in ids
            .iter()
            .zip(["ssh-synthetic", "rdp-synthetic", "vnc-synthetic"])
        {
            assert_eq!(
                store.load_profile_password(*id).unwrap().as_deref(),
                Some(secret)
            );
        }
        let reopened = CredentialStore::with_path("test", &path);
        assert_eq!(
            reopened.protection_status().unwrap(),
            CredentialProtection::Locked
        );
        reopened.unlock(MASTER).unwrap();
        assert_eq!(
            reopened.load_profile_password(ids[2]).unwrap().as_deref(),
            Some("vnc-synthetic")
        );
        for file in [backup, path] {
            std::fs::remove_file(file).unwrap();
        }
    }

    #[test]
    fn changing_and_disabling_master_require_current_password() {
        let (store, path) = temp_store("master_change");
        store.save_secret("token", "synthetic token").unwrap();
        let first = store.enable_master_password(MASTER).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(store
            .change_master_password("incorrect", "another synthetic passphrase")
            .is_err());
        assert!(store.disable_master_password("incorrect").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let second = store
            .change_master_password(MASTER, "another synthetic passphrase")
            .unwrap();
        store.lock_vault().unwrap();
        assert!(store.unlock(MASTER).is_err());
        store.unlock("another synthetic passphrase").unwrap();
        let third = store
            .disable_master_password("another synthetic passphrase")
            .unwrap();
        assert_eq!(
            store.protection_status().unwrap(),
            CredentialProtection::Legacy
        );
        assert_eq!(
            store.load_secret("token").unwrap().as_deref(),
            Some("synthetic token")
        );
        for file in [first, second, third, path] {
            std::fs::remove_file(file).unwrap();
        }
    }

    #[test]
    fn failed_migration_does_not_change_legacy_store() {
        let (store, path) = temp_store("master_cancel");
        store.save_secret("token", "synthetic token").unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            store.enable_master_password("short"),
            Err(CredentialError::WeakMasterPassword)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let blocked = unique_sibling(&path, "blocked");
        std::fs::create_dir(&blocked).unwrap();
        assert!(atomic_write(&blocked, b"replacement").is_err());
        assert!(blocked.is_dir());
        assert_eq!(
            store.load_secret("token").unwrap().as_deref(),
            Some("synthetic token")
        );
        std::fs::remove_dir(blocked).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_windows_destination_cannot_destroy_a_legacy_vault_during_migration() {
        use std::os::windows::fs::OpenOptionsExt;
        let (store, path) = temp_store("master_windows_locked");
        store
            .save_secret("token", "synthetic retained token")
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        // Permit reads, but not replacement/deletion, like an external Windows
        // reader that has not opted in to FILE_SHARE_DELETE.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        assert!(store.enable_master_password(MASTER).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            store.load_secret("token").unwrap().as_deref(),
            Some("synthetic retained token")
        );
        drop(held);
        let backup = store.enable_master_password(MASTER).unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), before);
        let cold = CredentialStore::with_path("adit-test", &path);
        assert!(matches!(
            cold.load_secret("token"),
            Err(CredentialError::Locked)
        ));
        cold.unlock(MASTER).unwrap();
        assert_eq!(
            cold.load_secret("token").unwrap().as_deref(),
            Some("synthetic retained token")
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(backup).unwrap();
    }

    #[test]
    fn external_replacement_locks_cache_and_deletion_does_not_downgrade() {
        let (store, path) = temp_store("master_external");
        store.save_secret("token", "before").unwrap();
        let backup = store.enable_master_password(MASTER).unwrap();
        let other = CredentialStore::with_path("test", &path);
        other.unlock(MASTER).unwrap();
        other.save_secret("token", "after").unwrap();
        assert_eq!(
            store.protection_status().unwrap(),
            CredentialProtection::Locked
        );
        assert!(matches!(
            store.save_secret("token", "stale"),
            Err(CredentialError::Locked)
        ));
        store.unlock(MASTER).unwrap();
        assert_eq!(
            store.load_secret("token").unwrap().as_deref(),
            Some("after")
        );
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            store.save_secret("token", "recreate"),
            Err(CredentialError::Conflict)
        ));
        assert!(!path.exists());
        std::fs::remove_file(backup).unwrap();
    }

    #[test]
    fn encrypted_transfer_validates_before_replace_and_rejects_account_swap() {
        let (store, path) = temp_store("master_transfer_src");
        store
            .save_secret("token", "transferred synthetic secret")
            .unwrap();
        let backup = store.enable_master_password(MASTER).unwrap();
        store.lock_vault().unwrap();
        let export = unique_sibling(&path, "export");
        store.export_encrypted(&export).unwrap();
        assert!(store.export_encrypted(&export).is_err());
        let (dest, dest_path) = temp_store("master_transfer_dest");
        dest.save_secret("local", "must survive wrong password")
            .unwrap();
        let before = std::fs::read(&dest_path).unwrap();
        assert!(dest.import_encrypted(&export, Some("wrong")).is_err());
        assert_eq!(std::fs::read(&dest_path).unwrap(), before);
        let dest_backup = dest.import_encrypted(&export, Some(MASTER)).unwrap();
        assert_eq!(std::fs::read(&dest_backup).unwrap(), before);
        assert_eq!(
            dest.protection_status().unwrap(),
            CredentialProtection::Locked
        );
        dest.unlock(MASTER).unwrap();
        assert_eq!(
            dest.load_secret("token").unwrap().as_deref(),
            Some("transferred synthetic secret")
        );
        let mut file = decode_file(&std::fs::read(&dest_path).unwrap()).unwrap();
        let entry = file.entries.remove("secret:token").unwrap();
        file.entries.insert("secret:other".to_owned(), entry);
        atomic_write(&dest_path, &serde_json::to_vec(&file).unwrap()).unwrap();
        assert!(dest.unlock(MASTER).is_err());
        for file in [backup, export, dest_backup, path, dest_path] {
            std::fs::remove_file(file).unwrap();
        }
    }
}
