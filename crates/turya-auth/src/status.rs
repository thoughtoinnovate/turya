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
    /// The provider needs no credential: a server the user already runs.
    ///
    /// Distinct from `Missing`, which means "you need one and do not have
    /// it" and renders as a locked badge. Reporting a local server as
    /// `Missing` is what makes turya print a misleading "run turya auth
    /// login" for something with no login.
    NotRequired,
}

impl SlotState {
    /// One-glyph badge for pickers and status tables.
    pub fn badge(&self) -> &'static str {
        match self {
            SlotState::Env | SlotState::Stored | SlotState::Connected { .. } => "●",
            // Filled, deliberately: there is nothing to unlock.
            SlotState::NotRequired => "●",
            SlotState::Missing | SlotState::Unsupported => "○",
        }
    }

    pub fn is_filled(&self) -> bool {
        matches!(
            self,
            SlotState::Env
                | SlotState::Stored
                | SlotState::Connected { .. }
                | SlotState::NotRequired
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
        // `NotRequired` is filled, so the `is_filled()` test below would claim
        // an api key is in play. Report the truth — a local server is reached
        // with no credential — and keep the label in step with the `via`
        // provenance the resolver mints for it.
        if matches!(self.api_key, SlotState::NotRequired) {
            return Some("local");
        }
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
    /// False when the provider can be used with no credential at all. The
    /// env var is then *optional*: set it for a proxied or remote server,
    /// leave it unset for a local one.
    pub needs_credential: bool,
}

/// Well-known method sets (mirrored by provider plugin manifests in STEP 8).
pub fn methods_for(provider: &str) -> ProviderAuthMethods {
    match provider {
        "gemini" => ProviderAuthMethods {
            env_var: Some("GEMINI_API_KEY"),
            has_oauth: true,
            needs_credential: true,
        },
        "anthropic" => ProviderAuthMethods {
            env_var: Some("ANTHROPIC_API_KEY"),
            has_oauth: false,
            needs_credential: true,
        },
        "ollama" => ProviderAuthMethods {
            env_var: Some("OLLAMA_API_KEY"),
            has_oauth: false,
            // A local Ollama needs nothing. The key is for a reverse proxy
            // or ollama.com, so it is offered but not demanded.
            needs_credential: false,
        },
        // Unknown providers are assumed to need a key: demanding a credential
        // we cannot obtain fails loudly, silently trusting an unknown endpoint
        // would not.
        _ => ProviderAuthMethods {
            env_var: None,
            has_oauth: false,
            needs_credential: true,
        },
    }
}

/// Compute dual-slot status from env + store. Never touches the network.
pub fn auth_status(
    provider: &str,
    methods: &ProviderAuthMethods,
    store: &dyn CredentialStore,
) -> ProviderAuthStatus {
    // env > stored > (nothing needed) > missing. Kept flat instead of nested on
    // `env_var`: a provider may offer an *optional* env var, so "no env var
    // configured" and "env var configured but unset" must reach the same verdict.
    let env_set = methods
        .env_var
        .and_then(|env| std::env::var(env).ok())
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let stored = store
        .get(&crate::store::api_key_account(provider))
        .is_some();
    let api_key = if env_set {
        SlotState::Env
    } else if stored {
        SlotState::Stored
    } else if !methods.needs_credential {
        // "Needs nothing" is not "missing": reporting a local server as Missing
        // renders a locked badge and tells the user to run a login flow that
        // does not exist. An optional env var that *is* set still wins above.
        SlotState::NotRequired
    } else {
        SlotState::Missing
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
    fn not_required_is_filled_and_never_locked() {
        // A provider that needs no credential must not look like one awaiting
        // a login: filled badge, filled semantics, and a truthful active
        // method (never "api-key", which would claim a key exists).
        let nr = SlotState::NotRequired;
        assert!(nr.is_filled());
        assert_eq!(nr.badge(), "●");
        assert_ne!(nr.badge(), "○");
        let s = ProviderAuthStatus {
            provider: "ollama".into(),
            api_key: SlotState::NotRequired,
            oauth: SlotState::Unsupported,
        };
        assert!(s.is_authenticated());
        assert_eq!(s.active_method(), Some("local"));
    }

    #[test]
    fn local_provider_needs_no_credential() {
        assert!(!methods_for("ollama").needs_credential);
        assert!(methods_for("anthropic").needs_credential);
        assert!(methods_for("gemini").needs_credential);
    }

    #[test]
    fn no_credential_provider_is_not_reported_missing() {
        let store = MemStore::new();
        let m = ProviderAuthMethods {
            env_var: None,
            has_oauth: false,
            needs_credential: false,
        };
        let s = auth_status("local-x", &m, &store);
        assert_eq!(s.api_key, SlotState::NotRequired);
        // No OAuth flow on such a provider: still "n/a", not "missing".
        assert_eq!(s.oauth, SlotState::Unsupported);

        // A stored key, if the user has one, is still honoured.
        store.set("local-x-api-key", "k").unwrap();
        let s = auth_status("local-x", &m, &store);
        assert_eq!(s.api_key, SlotState::Stored);
    }

    #[test]
    fn optional_env_var_upgrades_not_required_to_env() {
        let store = MemStore::new();
        let m = ProviderAuthMethods {
            env_var: Some("TURYA_TEST_OPTIONAL_KEY"),
            has_oauth: false,
            needs_credential: false,
        };
        // Unset optional var: still nothing required.
        let s = auth_status("local-x", &m, &store);
        assert_eq!(s.api_key, SlotState::NotRequired);

        // Set (proxy / ollama.com case): reported as an ordinary env key.
        std::env::set_var("TURYA_TEST_OPTIONAL_KEY", "env");
        let s = auth_status("local-x", &m, &store);
        assert_eq!(s.api_key, SlotState::Env);
        std::env::remove_var("TURYA_TEST_OPTIONAL_KEY");
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
            needs_credential: true,
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
