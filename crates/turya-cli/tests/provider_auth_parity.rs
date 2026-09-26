//! The provider↔auth-table contract, enforced.
//!
//! Two hand-maintained facts describe the same thing and nothing cross-checks
//! them:
//!
//! 1. each plugin declares its own `ProviderPlugin::auth_methods()`, and
//! 2. `turya_auth::status::methods_for` is the table the credential resolver
//!    and the host's auth status actually consult.
//!
//! When they drift, the failure is silent and confusing: the browser promises
//! an API key the resolver will not hand over, or a local server keeps
//! demanding a login it does not need. This file is the cross-check.

use std::sync::Arc;

use turya_core::{AuthMethodKind, LlmProvider, ModelInfo, ProviderPlugin, ResolvedCreds};

/// Mirrors `build_provider_registry()` in `src/main.rs`.
///
/// It is duplicated rather than imported because that function lives in the
/// `turya` *binary* target and is private to it, so an integration test cannot
/// reach it. A new built-in provider must be added to BOTH places in the same
/// change: registering it without listing it here silently drops it from this
/// guard, which is the failure mode this file exists to prevent.
fn registered_providers() -> Vec<(String, Arc<dyn ProviderPlugin>)> {
    vec![
        (
            "anthropic".to_string(),
            Arc::new(turya_provider_anthropic::AnthropicPlugin) as Arc<dyn ProviderPlugin>,
        ),
        (
            "gemini".to_string(),
            Arc::new(turya_provider_gemini::GeminiPlugin) as Arc<dyn ProviderPlugin>,
        ),
        (
            "ollama".to_string(),
            Arc::new(turya_provider_ollama::OllamaPlugin) as Arc<dyn ProviderPlugin>,
        ),
    ]
}

/// Every way a provider's declaration can contradict the auth table, as
/// sentences that name the provider. Empty means they agree.
fn parity_problems(providers: &[(String, Arc<dyn ProviderPlugin>)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (id, plugin) in providers {
        assert_eq!(
            plugin.id(),
            id,
            "the mirror in `registered_providers()` drifted from the plugin's own id"
        );
        let declared = plugin.auth_methods();
        let table = turya_auth::status::methods_for(id);

        let key = declared.iter().find_map(|m| match m {
            AuthMethodKind::ApiKey { env_var } => Some(*env_var),
            _ => None,
        });
        let declares_none = declared.iter().any(|m| matches!(m, AuthMethodKind::None));
        // 1. The env var, whenever a key is declared. Checked independently of
        //    the credential demand below, because a local provider may declare
        //    an *optional* key (for a reverse proxy) alongside `None`.
        if let Some(env_var) = key {
            if table.env_var != Some(env_var) {
                problems.push(format!(
                    "provider '{id}' declares ApiKey {{ env_var: {env_var:?} }}, but \
                     turya_auth::status::methods_for(\"{id}\") says env_var: {:?} — the host \
                     reads credentials from the table, not from the plugin, so these must be \
                     the same variable.",
                    table.env_var
                ));
            }
        } else if !declares_none && !declared.is_empty() {
            problems.push(format!(
                "provider '{id}' declares {declared:?}: neither an ApiKey env var nor None. \
                 Say which variable turya-auth should read, or that none is needed."
            ));
        }

        // 2. Whether a credential is demanded at all. `methods_for`'s
        //    unknown-provider arm fails safe to `needs_credential: true`, so a
        //    provider that was never added to the table surfaces here as a
        //    named failure instead of a provider that silently cannot be used.
        if declares_none {
            if table.needs_credential {
                problems.push(format!(
                    "provider '{id}' declares no credential (auth_methods: {declared:?}), but \
                     turya_auth::status::methods_for(\"{id}\") still demands one \
                     (needs_credential: true) — the resolver will refuse to use it with nothing \
                     configured. Add a `methods_for(\"{id}\")` arm with needs_credential: false."
                ));
            }
        } else if declared.is_empty() {
            problems.push(format!(
                "provider '{id}' declares no auth methods at all, which says nothing about \
                 whether it needs a credential; declare AuthMethodKind::ApiKey or \
                 AuthMethodKind::None."
            ));
        } else if !table.needs_credential {
            problems.push(format!(
                "provider '{id}' declares an API key but turya_auth::status::methods_for(\"{id}\") \
                 says needs_credential: false — the resolver would skip asking for a key the \
                 user was told to provide."
            ));
        }
    }
    problems
}

#[test]
fn every_registered_provider_agrees_with_the_auth_methods_table() {
    let providers = registered_providers();
    assert!(
        !providers.is_empty(),
        "the mirror of `build_provider_registry()` is empty, so this test proves nothing"
    );
    let problems = parity_problems(&providers);
    assert!(
        problems.is_empty(),
        "provider declarations and turya_auth::status::methods_for disagree:\n  - {}",
        problems.join("\n  - ")
    );
}

/// A local provider the auth table has never heard of. Exactly the shape of
/// the bug this file exists for, so the guard is proven to bite rather than
/// merely to be present: the report must name the provider, not shrug.
struct UnregisteredLocal;

impl ProviderPlugin for UnregisteredLocal {
    fn id(&self) -> &str {
        "local-without-a-table-entry"
    }
    fn display_name(&self) -> &str {
        "Local Without A Table Entry"
    }
    fn models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
    fn auth_methods(&self) -> Vec<AuthMethodKind> {
        vec![AuthMethodKind::None]
    }
    fn connect(&self, _creds: ResolvedCreds, _model: &str) -> Result<Arc<dyn LlmProvider>, String> {
        Err("not a real provider".to_string())
    }
}

#[test]
fn a_provider_missing_from_the_auth_table_is_reported_by_name() {
    let problems = parity_problems(&[(
        "local-without-a-table-entry".to_string(),
        Arc::new(UnregisteredLocal) as Arc<dyn ProviderPlugin>,
    )]);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("local-without-a-table-entry"),
        "{problems:?}"
    );
    assert!(
        problems[0].contains("needs_credential: false"),
        "{problems:?}"
    );
}
