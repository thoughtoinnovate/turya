//! Authentication services for provider plugins (internal plugin).
//!
//! Rule 3.1/3.2: keychain access, OAuth HTTP, and token refresh live ONLY
//! here. The microkernel never imports this crate; the host resolves
//! credentials and hands the engine short-lived `ResolvedCreds`.

pub mod oauth;
pub mod resolver;
pub mod status;
pub mod store;

pub use oauth::{OAuthConfig, OAuthError, TokenResponse};
pub use resolver::{CredSource, MethodChoice, ResolveError};
pub use status::{auth_status, methods_for, ProviderAuthMethods, ProviderAuthStatus, SlotState};
pub use store::{
    api_key_account, oauth_client_id_account, oauth_refresh_account, ChainStore, CredentialStore,
    FileStore, KeychainStore, MemStore, StoreError,
};
pub use turya_core::ResolvedCreds;

/// Serialises every test in this crate that mutates process environment.
///
/// Environment is global, so two tests setting and clearing the same var
/// interleave and produce failures that look like logic bugs and are not. A
/// fast workstation hides this; a 2-core CI runner surfaces it.
#[cfg(test)]
pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
