//! Dual-slot auth status: API key ∥ OAuth, shown separately everywhere.
//!
//! Each provider id maps to two independent slots. A slot is `Env` when the
//! corresponding environment variable is set, `Stored`/`Connected` when the
//! keychain holds the secret, `Missing` otherwise, `Unsupported` when the
//! provider offers no such method (e.g. no OAuth flow).

use crate::store::CredentialStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    /// Present in the environment (takes precedence, never persisted by us).
    Env,
    /// API key persisted in the credential store.
    Stored,
    /// OAuth refresh token persisted; holds the connected account label.
    Connected { account: String },
    /// No credential for this slot.
    Missing,
    /// The provider does not offer this method at all.
    Unsupported,
}

impl SlotState {
    /// One-glyph badge for pickers and status tables.
    pub fn badge(&self) -> &'static str {
        match self {
            SlotState::Env | SlotState::Stored | SlotState::Connected { .. } => "●",
            SlotState::Missing | SlotState::Unsupported => "○",
        }
    }

    pub fn is_filled(&self) -> bool {
        matches!(
            self,
            SlotState::Env | SlotState::Stored | SlotState::Connected { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAuthStatus {
    pub provider: String,
    pub api_key: SlotState,
    pub oauth: SlotState,
}

impl ProviderAuthStatus {
    /// Effective authentication: either slot filled.
    pub fn is_authenticated(&self) -> bool {
        self.api_key.is_filled() || self.oauth.is_filled()
    }

    /// Which credential backs requests (see resolver precedence).
    pub fn active_method(&self) -> Option<&'static str> {
        if self.api_key.is_filled() {
            Some("api-key")
        } else if self.oauth.is_filled() {
            Some("oauth")
        } else {
            None
        }
    }
}

/// Which auth methods a provider offers. The registry (STEP 2) answers this;
/// until then, callers pass it explicitly so this fn stays provider-blind.
pub struct ProviderAuthMethods {
    pub env_var: Option<&'static str>,
    pub has_oauth: bool,
}

/// Well-known method sets (mirrored by provider plugin manifests in STEP 8).
pub fn methods_for(provider: &str) -> ProviderAuthMethods {
    match provider {
        "gemini" => ProviderAuthMethods {
            env_var: Some("GEMINI_API_KEY"),
            has_oauth: true,
        },
        "anthropic" => ProviderAuthMethods {
            env_var: Some("ANTHROPIC_API_KEY"),
            has_oauth: false,
        },
        _ => ProviderAuthMethods {
            env_var: None,
            has_oauth: false,
        },
    }
}

/// Compute dual-slot status from env + store. Never touches the network.
pub fn auth_status(
    provider: &str,
    methods: &ProviderAuthMethods,
    store: &dyn CredentialStore,
) -> ProviderAuthStatus {
    let api_key = match methods.env_var {
        None => {
            if store
                .get(&crate::store::api_key_account(provider))
                .is_some()
            {
                SlotState::Stored
            } else {
                SlotState::Missing
            }
        }
        Some(env) => {
            if std::env::var(env)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            {
                SlotState::Env
            } else if store
                .get(&crate::store::api_key_account(provider))
                .is_some()
            {
                SlotState::Stored
            } else {
                SlotState::Missing
            }
        }
    };
    let oauth = if !methods.has_oauth {
        SlotState::Unsupported
    } else if let Some(_refresh) = store.get(&crate::store::oauth_refresh_account(provider)) {
        SlotState::Connected {
            account: String::new(), // account label resolved lazily at login; absence of label ≠ absent token
        }
    } else {
        SlotState::Missing
    };
    ProviderAuthStatus {
        provider: provider.to_string(),
        api_key,
        oauth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    fn gemini() -> ProviderAuthMethods {
        methods_for("gemini")
    }

    #[test]
    fn badges_and_effective_auth() {
        let s = ProviderAuthStatus {
            provider: "x".into(),
            api_key: SlotState::Missing,
            oauth: SlotState::Missing,
        };
        assert!(!s.is_authenticated());
        assert_eq!(s.active_method(), None);
        assert_eq!(SlotState::Missing.badge(), "○");
        assert_eq!(SlotState::Env.badge(), "●");
    }

    #[test]
    fn status_matrix() {
        let store = MemStore::new();
        // Nothing configured.
        let s = auth_status("gemini", &gemini(), &store);
        assert_eq!(s.api_key, SlotState::Missing);
        assert_eq!(s.oauth, SlotState::Missing);
        assert!(!s.is_authenticated());

        // Stored key, no OAuth.
        store.set("gemini-api-key", "k").unwrap();
        let s = auth_status("gemini", &gemini(), &store);
        assert_eq!(s.api_key, SlotState::Stored);
        assert_eq!(s.active_method(), Some("api-key"));

        // OAuth connected.
        store.set("gemini-oauth-refresh", "r").unwrap();
        let s = auth_status("gemini", &gemini(), &store);
        assert!(matches!(s.oauth, SlotState::Connected { .. }));
        assert!(s.is_authenticated());
    }

    #[test]
    fn env_beats_stored() {
        let store = MemStore::new();
        store.set("gemini-api-key", "stored").unwrap();
        std::env::set_var("TURYA_TEST_GEMINI_KEY", "env");
        let methods = ProviderAuthMethods {
            env_var: Some("TURYA_TEST_GEMINI_KEY"),
            has_oauth: false,
        };
        let s = auth_status("gemini", &methods, &store);
        assert_eq!(s.api_key, SlotState::Env);
        std::env::remove_var("TURYA_TEST_GEMINI_KEY");
    }

    #[test]
    fn unknown_provider_has_no_methods() {
        let store = MemStore::new();
        let m = methods_for("nope");
        assert!(m.env_var.is_none() && !m.has_oauth);
        let s = auth_status("nope", &m, &store);
        assert!(!s.is_authenticated());
    }
}
