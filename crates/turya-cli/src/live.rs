//! Live-model test harness (Tier 2 of the verification plan).
//!
//! Deliberately *not* the production provider-selection path: this module
//! talks to one provider directly so a live test is a straight line from
//! "our wire format" to "a real API call". Credential resolution uses the
//! same `turya-auth` resolver and the same store chain the binary uses, so a
//! test can never pass on a credential the real app could not obtain.

use std::sync::Arc;

use turya_core::{LlmProvider, ResolvedCreds};
use turya_provider_gemini::GeminiProvider;

/// Default live target. Overridable with `TURYA_PROVIDER`/`TURYA_MODEL`.
const DEFAULT_PROVIDER: &str = "gemini";
const DEFAULT_MODEL: &str = "gemini-flash-lite-latest";

/// Resolve a live credential, or `None` when this machine has none.
///
/// Order: explicit env var, then the stored chain (keychain -> file). Mirrors
/// the binary's precedence so a green live test means the app can really run.
pub async fn live_credentials() -> Option<ResolvedCreds> {
    let provider = std::env::var("TURYA_PROVIDER").unwrap_or_else(|_| DEFAULT_PROVIDER.to_string());
    if provider != DEFAULT_PROVIDER {
        // Anthropic needs ANTHROPIC_API_KEY and its own wire tests; a live
        // Gemini harness must not silently swap vendors.
        eprintln!("skip: live harness is gemini-only (TURYA_PROVIDER={provider})");
        return None;
    }
    let env_key = "TURYA_GEMINI_API_KEY";
    if let Ok(key) = std::env::var(env_key) {
        if !key.trim().is_empty() {
            return Some(ResolvedCreds {
                token: key,
                via: "env",
                expires_at: None,
            });
        }
    }
    let store = crate::auth_cmd::default_store();
    match turya_auth::resolver::resolve(DEFAULT_PROVIDER, None, None, &store).await {
        Ok(creds) => Some(creds),
        Err(e) => {
            eprintln!("skip: no live credential ({e})");
            None
        }
    }
}

/// Build the live provider for a resolved credential.
pub struct LiveProvider;

impl LiveProvider {
    pub fn build(creds: &ResolvedCreds) -> Arc<dyn LlmProvider> {
        let model = std::env::var("TURYA_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Arc::new(GeminiProvider::connect_with(creds, &model))
    }
}
