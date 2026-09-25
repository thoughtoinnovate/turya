//! Credential storage: keychain-backed with an in-memory fallback for tests.
//!
//! Rule 3.1: this is the ONLY place secrets at rest are touched. Account
//! naming: `<provider>-api-key`, `<provider>-oauth-refresh`,
//! `<provider>-oauth-client-id`. Access tokens are never persisted.

use std::collections::HashMap;
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("keychain unavailable (needs OS credential service, e.g. Secret Service on Linux); export {0} instead")]
    Unavailable(String),
    #[error("keychain error: {0}")]
    Backend(String),
}

/// Abstract credential storage. Production impl is the OS keychain;
/// `MemStore` is for tests and headless environments without a backend.
pub trait CredentialStore: Send + Sync {
    fn get(&self, account: &str) -> Option<String>;
    fn set(&self, account: &str, secret: &str) -> Result<(), StoreError>;
    fn delete(&self, account: &str) -> Result<(), StoreError>;
    /// Downcast hook so hosts can report backend-specific details
    /// (e.g. where a chained write landed) without breaking the abstraction.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// OS keychain via the `keyring` crate (service name: `turya`).
pub struct KeychainStore {
    service: String,
}

impl KeychainStore {
    pub fn new() -> Self {
        Self {
            service: "turya".to_string(),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(&self.service, account).map_err(|e| StoreError::Backend(e.to_string()))
    }

    /// Env var a user should export when the keychain backend is missing,
    /// e.g. `GEMINI_API_KEY`. Purely informational for error messages.
    pub fn env_hint(account: &str) -> String {
        if let Some(provider) = account.strip_suffix("-api-key") {
            format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"))
        } else {
            "provider API key".to_string()
        }
    }
}

impl Default for KeychainStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialStore for KeychainStore {
    fn get(&self, account: &str) -> Option<String> {
        self.entry(account).ok()?.get_password().ok()
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), StoreError> {
        self.entry(account)
            .and_then(|e| {
                e.set_password(secret)
                    .map_err(|err| StoreError::Backend(err.to_string()))
            })
            .map_err(|e| match e {
                StoreError::Backend(msg) if is_missing_backend(&msg) => {
                    StoreError::Unavailable(Self::env_hint(account))
                }
                other => other,
            })
    }

    fn delete(&self, account: &str) -> Result<(), StoreError> {
        match self.entry(account).and_then(|e| {
            e.delete_credential()
                .map_err(|err| StoreError::Backend(err.to_string()))
        }) {
            // Deleting a missing entry is success (idempotent logout).
            Err(StoreError::Backend(msg)) if is_not_found(&msg) => Ok(()),
            other => other,
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Heuristic: `keyring` surfaces platform/backend absence as error text.
/// Anything else propagates as a real backend failure.
fn is_missing_backend(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("no secret service")
        || m.contains("not available")
        || m.contains("dbus")
        || m.contains("platform secure storage failure")
}

fn is_not_found(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("not found") || m.contains("no matching")
}

/// In-memory store: tests, and a documented fallback where no keychain exists.
pub struct MemStore {
    inner: Mutex<HashMap<String, String>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for MemStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialStore for MemStore {
    fn get(&self, account: &str) -> Option<String> {
        self.inner.lock().ok()?.get(account).cloned()
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), StoreError> {
        self.inner
            .lock()
            .map_err(|e| StoreError::Backend(e.to_string()))?
            .insert(account.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), StoreError> {
        self.inner
            .lock()
            .map_err(|e| StoreError::Backend(e.to_string()))?
            .remove(account);
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// File-backed store (`credentials.json`, mode 0600): persistent fallback
/// for environments without a working OS keychain (containers, headless
/// Linux without Secret Service, per-command dbus sessions).
///
/// Security posture: filesystem permissions are the only protection at rest
/// (same trade-off as the wider ecosystem). Permissions are tightened on
/// every load; parent dirs are created as needed. Never commit this file.
pub struct FileStore {
    path: std::path::PathBuf,
}

impl FileStore {
    pub fn new(path: impl AsRef<std::path::Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    fn load_map(&self) -> HashMap<String, String> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(r) => r,
            Err(_) => return HashMap::new(),
        };
        // Tighten permissions on every load (repairs chmod drift).
        Self::tighten(&self.path);
        serde_json::from_str(&raw).unwrap_or_default()
    }

    fn save_map(&self, map: &HashMap<String, String>) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::Backend(format!("cannot create store dir: {e}")))?;
        }
        let raw = serde_json::to_string(map).map_err(|e| StoreError::Backend(e.to_string()))?;
        std::fs::write(&self.path, raw)
            .map_err(|e| StoreError::Backend(format!("cannot write store: {e}")))?;
        Self::tighten(&self.path);
        Ok(())
    }

    fn tighten(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

impl CredentialStore for FileStore {
    fn get(&self, account: &str) -> Option<String> {
        self.load_map().get(account).cloned()
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), StoreError> {
        let mut map = self.load_map();
        map.insert(account.to_string(), secret.to_string());
        self.save_map(&map)
    }

    fn delete(&self, account: &str) -> Result<(), StoreError> {
        let mut map = self.load_map();
        map.remove(account);
        self.save_map(&map)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Chained store: tries backends in order. Reads hit the first backend
/// holding the account; writes go to the first backend that confirms
/// persistence via read-back (this is what defeats silent-login backends:
/// a keychain that accepts writes into the void is skipped in favor of
/// the file). Deletes fan out to all backends.
pub struct ChainStore {
    stores: Vec<(&'static str, Box<dyn CredentialStore>)>,
    last_write: Mutex<Option<usize>>,
}

impl ChainStore {
    pub fn new(stores: Vec<(&'static str, Box<dyn CredentialStore>)>) -> Self {
        Self {
            stores,
            last_write: Mutex::new(None),
        }
    }

    /// Production chain: OS keychain first, persistent file fallback.
    /// `file_path` should live under `~/.turya` (never the repo).
    pub fn with_file_fallback(file_path: impl AsRef<std::path::Path>) -> Self {
        Self::new(vec![
            ("OS keychain", Box::new(KeychainStore::new())),
            (
                "credentials file (~/.turya, mode 0600)",
                Box::new(FileStore::new(file_path)),
            ),
        ])
    }

    /// Human-readable label of the backend that confirmed the last write
    /// (for login messaging), e.g. `Some("OS keychain")`.
    pub fn last_write_backend(&self) -> Option<String> {
        self.last_write.lock().ok().and_then(|guard| {
            guard.and_then(|i| self.stores.get(i).map(|(label, _)| label.to_string()))
        })
    }
}

impl CredentialStore for ChainStore {
    fn get(&self, account: &str) -> Option<String> {
        self.stores.iter().find_map(|(_, s)| s.get(account))
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), StoreError> {
        let mut errors = Vec::new();
        for (i, (_, store)) in self.stores.iter().enumerate() {
            match store.set(account, secret) {
                Ok(()) => {
                    // Read-back confirmation: skip backends that swallow writes.
                    if store.get(account).as_deref() == Some(secret) {
                        if let Ok(mut guard) = self.last_write.lock() {
                            *guard = Some(i);
                        }
                        return Ok(());
                    }
                    errors.push(format!(
                        "backend accepted write but read-back failed for '{account}'"
                    ));
                }
                Err(e) => errors.push(e.to_string()),
            }
        }
        Err(StoreError::Backend(format!(
            "no persistent backend available: {}",
            errors.join("; ")
        )))
    }

    fn delete(&self, account: &str) -> Result<(), StoreError> {
        let mut errors = Vec::new();
        for (_, store) in &self.stores {
            if let Err(e) = store.delete(account) {
                errors.push(e.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(StoreError::Backend(errors.join("; ")))
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Canonical keychain account names for a provider id.
pub fn api_key_account(provider: &str) -> String {
    format!("{provider}-api-key")
}

pub fn oauth_refresh_account(provider: &str) -> String {
    format!("{provider}-oauth-refresh")
}

pub fn oauth_client_id_account(provider: &str) -> String {
    format!("{provider}-oauth-client-id")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_store_roundtrip_and_idempotent_delete() {
        let s = MemStore::new();
        assert!(s.get("a").is_none());
        s.set("a", "secret").unwrap();
        assert_eq!(s.get("a").as_deref(), Some("secret"));
        s.delete("a").unwrap();
        assert!(s.get("a").is_none());
        // Deleting twice is fine (logout idempotency).
        s.delete("a").unwrap();
    }

    #[test]
    fn account_naming_is_stable() {
        assert_eq!(api_key_account("gemini"), "gemini-api-key");
        assert_eq!(oauth_refresh_account("gemini"), "gemini-oauth-refresh");
        assert_eq!(
            oauth_client_id_account("anthropic"),
            "anthropic-oauth-client-id"
        );
    }

    #[test]
    fn env_hint_derives_from_account() {
        assert_eq!(KeychainStore::env_hint("gemini-api-key"), "GEMINI_API_KEY");
        assert_eq!(
            KeychainStore::env_hint("my-provider-api-key"),
            "MY_PROVIDER_API_KEY"
        );
    }

    fn temp_path(dir_name: &str, file_name: &str) -> std::path::PathBuf {
        // Unique dir per test: parallel tests must never share a directory
        // (one test's setup wipe would delete another's files).
        let dir = std::env::temp_dir().join(format!("turya-filestore-{dir_name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(file_name)
    }

    #[test]
    fn file_store_roundtrip_and_survives_reopen() {
        let path = temp_path("roundtrip", "creds.json");
        let s = FileStore::new(&path);
        assert!(s.get("gemini-api-key").is_none());
        s.set("gemini-api-key", "sk-test").unwrap();
        assert_eq!(s.get("gemini-api-key").as_deref(), Some("sk-test"));
        // Persistence across instances (the keychain property being replaced).
        drop(s);
        let reopened = FileStore::new(&path);
        assert_eq!(reopened.get("gemini-api-key").as_deref(), Some("sk-test"));
        reopened.delete("gemini-api-key").unwrap();
        assert!(reopened.get("gemini-api-key").is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn file_store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("perms", "perms.json");
        let s = FileStore::new(&path);
        s.set("a", "b").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials file must be owner-only");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn file_store_tolerates_corrupt_file() {
        let path = temp_path("corrupt", "corrupt.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        let s = FileStore::new(&path);
        // Corrupt file reads as empty (logout/login can proceed), never panics.
        assert!(s.get("a").is_none());
        s.set("a", "b").unwrap();
        assert_eq!(s.get("a").as_deref(), Some("b"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Write-only backend (the reported silent-login bug).
    struct WriteOnlyStore;

    impl CredentialStore for WriteOnlyStore {
        fn get(&self, _account: &str) -> Option<String> {
            None
        }
        fn set(&self, _account: &str, _secret: &str) -> Result<(), StoreError> {
            Ok(())
        }
        fn delete(&self, _account: &str) -> Result<(), StoreError> {
            Ok(())
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[test]
    fn chain_skips_silent_backend_for_file() {
        let path = temp_path("chain", "chain.json");
        let chain = ChainStore::new(vec![
            ("silent-keychain", Box::new(WriteOnlyStore)),
            ("file", Box::new(FileStore::new(&path))),
        ]);
        chain.set("gemini-api-key", "k").unwrap();
        // Read-back lands on the file, and the chain reports where.
        assert_eq!(chain.get("gemini-api-key").as_deref(), Some("k"));
        assert_eq!(chain.last_write_backend().as_deref(), Some("file"));
        chain.delete("gemini-api-key").unwrap();
        assert!(chain.get("gemini-api-key").is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn chain_reads_first_hit_and_deletes_everywhere() {
        let path = temp_path("chain2", "chain2.json");
        let primary = MemStore::new();
        primary.set("a", "from-mem").unwrap();
        let chain = ChainStore::new(vec![
            ("mem", Box::new(primary)),
            ("file", Box::new(FileStore::new(&path))),
        ]);
        assert_eq!(chain.get("a").as_deref(), Some("from-mem"));
        chain.delete("a").unwrap();
        assert!(chain.get("a").is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
