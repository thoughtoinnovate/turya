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
    api_key_account, oauth_client_id_account, oauth_refresh_account, CredentialStore,
    KeychainStore, MemStore, StoreError,
};
pub use turya_core::ResolvedCreds;
