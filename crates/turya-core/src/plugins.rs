//! Plugin traits + registries (Rule 3.1).
//!
//! The kernel owns these abstractions and NOTHING behind them: no provider
//! implementations, no vendor strings, no credentials, no UI code. Built-in
//! capabilities (`turya-provider-*`, `turya-plugin-tui`, …) implement these
//! traits in their own crates and register at host bootstrap — the same path
//! an external Wasm plugin uses, so any of them can be replaced or
//! hot-reloaded without touching core.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use async_trait::async_trait;

use crate::provider::LlmProvider;

/// Short-lived credential handed to provider plugins.
///
/// NEVER a refresh token, API key at rest, or keychain handle — only the
/// token for the next request(s) plus its expiry and provenance label.
/// Minted by the host via `turya-auth`; the engine merely carries it.
#[derive(Debug, Clone)]
pub struct ResolvedCreds {
    pub token: String,
    pub expires_at: Option<SystemTime>,
    /// e.g. `"env"`, `"stored-key"`, `"oauth"`.
    pub via: &'static str,
}

/// One model offered by a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
}

/// Authentication methods a provider offers (declaration only — the flows
/// live in `turya-auth` and the host).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMethodKind {
    ApiKey { env_var: &'static str },
    OAuth,
}

/// A language-model provider as a plugin (Rule 3.2: built-ins implement
/// this in their own crates; community ones over the Wasm boundary later).
#[async_trait]
pub trait ProviderPlugin: Send + Sync {
    /// Registry id, e.g. `"anthropic"`, `"gemini"`.
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    /// Curated model list (live discovery unions over this in STEP 4).
    fn models(&self) -> Vec<ModelInfo>;
    fn auth_methods(&self) -> Vec<AuthMethodKind>;
    /// Build a ready-to-run provider from host-resolved credentials.
    fn connect(&self, creds: ResolvedCreds, model: &str) -> Result<Arc<dyn LlmProvider>, String>;
    /// Live model ids for these credentials. Empty on ANY failure —
    /// callers fall through to cached/static lists, never an error.
    async fn list_models(&self, creds: &ResolvedCreds) -> Vec<String> {
        let _ = creds;
        vec![]
    }
}

/// Runtime provider registry (Rule 3.3: register/unregister, never load-time-only).
#[derive(Default)]
pub struct ProviderRegistry {
    plugins: RwLock<HashMap<String, Arc<dyn ProviderPlugin>>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, plugin: Arc<dyn ProviderPlugin>) {
        self.plugins
            .write()
            .unwrap()
            .insert(plugin.id().to_string(), plugin);
    }

    pub fn unregister(&self, id: &str) -> bool {
        self.plugins.write().unwrap().remove(id).is_some()
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn ProviderPlugin>> {
        self.plugins.read().unwrap().get(id).cloned()
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.read().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// A UI/client frontend as a plugin (Rule 3.2: the TUI is `turya-plugin-tui`,
/// an internal client plugin; WebUI/IDE adapters follow the same trait).
///
/// The ONLY engine interaction is `handle_event` in / `drain_commands` out —
/// both speak `turya-protocol` types exclusively.
pub trait UiPlugin: Send {
    fn id(&self) -> &str;
    fn handle_event(&mut self, event: &turya_protocol::TuryaEvent);
    fn drain_commands(&mut self) -> Vec<turya_protocol::TuryaCommand>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{MockProvider, ProviderStep};

    struct FakeProviderPlugin {
        id: &'static str,
    }

    impl ProviderPlugin for FakeProviderPlugin {
        fn id(&self) -> &str {
            self.id
        }
        fn display_name(&self) -> &str {
            "Fake"
        }
        fn models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: "fake-1".to_string(),
                display_name: "Fake 1".to_string(),
            }]
        }
        fn auth_methods(&self) -> Vec<AuthMethodKind> {
            vec![AuthMethodKind::ApiKey {
                env_var: "FAKE_KEY",
            }]
        }
        fn connect(
            &self,
            _creds: ResolvedCreds,
            _model: &str,
        ) -> Result<Arc<dyn LlmProvider>, String> {
            Ok(Arc::new(MockProvider {
                responses: vec![
                    ProviderStep::Token("fake".to_string()),
                    ProviderStep::Finish,
                ],
            }))
        }
    }

    #[test]
    fn registry_registers_lists_unregisters() {
        let reg = ProviderRegistry::new();
        assert!(reg.ids().is_empty());
        reg.register(Arc::new(FakeProviderPlugin { id: "b" }));
        reg.register(Arc::new(FakeProviderPlugin { id: "a" }));
        assert_eq!(reg.ids(), vec!["a".to_string(), "b".to_string()]);
        let p = reg.get("a").unwrap();
        assert_eq!(p.models().len(), 1);
        // Replacement (hot-reload path) overwrites in place.
        reg.register(Arc::new(FakeProviderPlugin { id: "a" }));
        assert_eq!(reg.ids().len(), 2);
        assert!(reg.unregister("a"));
        assert!(!reg.unregister("a"));
        assert!(reg.get("a").is_none());
    }

    #[test]
    fn resolved_creds_carry_no_refresh_token_shape() {
        let c = ResolvedCreds {
            token: "tok".to_string(),
            expires_at: None,
            via: "env",
        };
        // Compile-time shape check: only short-lived fields exist.
        assert_eq!(c.via, "env");
    }
}
