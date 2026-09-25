//! Credential resolution: env → stored key → OAuth token (with refresh).
//!
//! Output is `ResolvedCreds`: a short-lived token plus provenance. Refresh
//! tokens and keychain handles never leave this module.

use crate::oauth::{self, OAuthConfig};
use crate::status::{methods_for, SlotState};
use crate::store::{api_key_account, oauth_refresh_account, CredentialStore};
use std::time::{Duration, SystemTime};
use thiserror::Error;
use turya_core::ResolvedCreds;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("no credential for provider '{0}' (run: turya auth login {0})")]
    Missing(String),
    #[error("OAuth refresh failed: {0}")]
    RefreshFailed(String),
    #[error("store: {0}")]
    Store(#[from] crate::store::StoreError),
}

/// Where the resolved credential came from (precedence order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredSource {
    Env,
    StoredKey,
    OAuth,
}

impl CredSource {
    fn label(self) -> &'static str {
        match self {
            CredSource::Env => "env",
            CredSource::StoredKey => "stored-key",
            CredSource::OAuth => "oauth",
        }
    }
}

/// Pure method choice for a given status + optional user preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodChoice {
    ApiKey,
    OAuth,
    None,
}

pub fn pick_method(api_key: &SlotState, oauth: &SlotState, prefer: Option<&str>) -> MethodChoice {
    if let Some("oauth") = prefer {
        if oauth.is_filled() {
            return MethodChoice::OAuth;
        }
    }
    if api_key.is_filled() {
        return MethodChoice::ApiKey;
    }
    if oauth.is_filled() {
        return MethodChoice::OAuth;
    }
    MethodChoice::None
}

/// Resolve credentials for `provider`. `oauth_cfg` is required only when
/// falling back to OAuth (carries the user-supplied client id for refresh).
/// `prefer` is `Some("oauth")` to override the default api-key-first order.
pub async fn resolve(
    provider: &str,
    prefer: Option<&str>,
    oauth_cfg: Option<&OAuthConfig>,
    store: &dyn CredentialStore,
) -> Result<ResolvedCreds, ResolveError> {
    let methods = methods_for(provider);
    let status = crate::status::auth_status(provider, &methods, store);
    match pick_method(&status.api_key, &status.oauth, prefer) {
        MethodChoice::ApiKey => {
            if let Some(env) = methods.env_var {
                if let Ok(v) = std::env::var(env) {
                    if !v.trim().is_empty() {
                        return Ok(ResolvedCreds {
                            token: v,
                            expires_at: None,
                            via: CredSource::Env.label(),
                        });
                    }
                }
            }
            match store.get(&api_key_account(provider)) {
                Some(k) => Ok(ResolvedCreds {
                    token: k,
                    expires_at: None,
                    via: CredSource::StoredKey.label(),
                }),
                None => Err(ResolveError::Missing(provider.to_string())),
            }
        }
        MethodChoice::OAuth => {
            let cfg = oauth_cfg.ok_or_else(|| ResolveError::Missing(provider.to_string()))?;
            let refresh_token = store
                .get(&oauth_refresh_account(provider))
                .ok_or_else(|| ResolveError::Missing(provider.to_string()))?;
            let tokens = oauth::refresh(cfg, &refresh_token)
                .await
                .map_err(|e| ResolveError::RefreshFailed(e.to_string()))?;
            // Rotation: persist a rotated refresh token when the server issues one.
            if let Some(rotated) = tokens.refresh_token {
                if rotated != refresh_token {
                    let _ = store.set(&oauth_refresh_account(provider), &rotated);
                }
            }
            Ok(ResolvedCreds {
                token: tokens.access_token,
                expires_at: Some(SystemTime::now() + Duration::from_secs(tokens.expires_in)),
                via: CredSource::OAuth.label(),
            })
        }
        MethodChoice::None => Err(ResolveError::Missing(provider.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    #[test]
    fn precedence_matrix() {
        use MethodChoice::{ApiKey, OAuth};
        // (api, oauth, prefer) -> choice
        assert_eq!(
            pick_method(&SlotState::Env, &SlotState::Missing, None),
            ApiKey
        );
        assert_eq!(
            pick_method(
                &SlotState::Stored,
                &SlotState::Connected {
                    account: "a".into()
                },
                None
            ),
            ApiKey // api-key wins by default
        );
        assert_eq!(
            pick_method(
                &SlotState::Missing,
                &SlotState::Connected {
                    account: "a".into()
                },
                None
            ),
            OAuth
        );
        assert_eq!(
            pick_method(
                &SlotState::Stored,
                &SlotState::Connected {
                    account: "a".into()
                },
                Some("oauth")
            ),
            OAuth // explicit preference wins
        );
        assert_eq!(
            pick_method(&SlotState::Missing, &SlotState::Missing, None),
            MethodChoice::None
        );
        assert_eq!(
            pick_method(&SlotState::Missing, &SlotState::Missing, Some("oauth")),
            MethodChoice::None
        );
    }

    #[tokio::test]
    async fn resolve_prefers_env_then_store() {
        let store = MemStore::new();
        store.set("gemini-api-key", "stored-key").unwrap();
        // Without env: stored key.
        let c = resolve("gemini", None, None, &store).await.unwrap();
        assert_eq!(c.token, "stored-key");
        assert_eq!(c.via, "stored-key");

        // With env: env wins.
        std::env::set_var("GEMINI_API_KEY", "env-key");
        let c = resolve("gemini", None, None, &store).await.unwrap();
        assert_eq!(c.token, "env-key");
        assert_eq!(c.via, "env");
        std::env::remove_var("GEMINI_API_KEY");
    }

    #[tokio::test]
    async fn resolve_missing_without_network() {
        let store = MemStore::new();
        // Unknown provider: immediate Missing, no HTTP attempted.
        let err = resolve("nope", None, None, &store).await.unwrap_err();
        assert!(matches!(err, ResolveError::Missing(_)));
    }
}
