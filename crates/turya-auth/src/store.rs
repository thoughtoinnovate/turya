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
}
