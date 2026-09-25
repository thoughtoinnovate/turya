//! `turya auth` subcommands: login/logout/status for provider plugins.
//!
//! Rule 3.2: thin host wrappers over shared `turya-auth` functions — the TUI
//! `/auth` flow calls these same fns, so CLI and TUI can never diverge.
//! No provider logic here: everything resolves through `ProviderRegistry`.

use turya_auth::{
    api_key_account, auth_status, methods_for, oauth_client_id_account, oauth_refresh_account,
    CredentialStore, KeychainStore, SlotState,
};
use turya_core::ProviderRegistry;

/// Known provider ids come from the registry, never a hardcoded list here.
pub fn registry_ids(registry: &ProviderRegistry) -> Vec<String> {
    registry.ids()
}

fn badge(state: &SlotState) -> &'static str {
    state.badge()
}

fn slot_label(state: &SlotState) -> String {
    match state {
        SlotState::Env => "● env".to_string(),
        SlotState::Stored => "● stored".to_string(),
        SlotState::Connected { account } if account.is_empty() => "● connected".to_string(),
        SlotState::Connected { account } => format!("● {account}"),
        SlotState::Missing => "○ missing".to_string(),
        SlotState::Unsupported => "— n/a".to_string(),
    }
}

/// `turya auth status`: one table, dual slots per provider.
pub fn status(registry: &ProviderRegistry, store: &dyn CredentialStore) -> Result<(), String> {
    println!("{:<12} {:<12} oauth", "provider", "api-key");
    let mut any_filled = false;
    for id in registry_ids(registry) {
        let st = auth_status(&id, &methods_for(&id), store);
        any_filled |= st.is_authenticated();
        println!(
            "{:<12} {:<12} {}",
            id,
            slot_label(&st.api_key),
            slot_label(&st.oauth)
        );
        let _ = badge(&st.api_key);
    }
    // Everything missing can mean "never logged in" OR "backend silently
    // broken" — disambiguate with a live probe instead of guessing.
    if !any_filled {
        if let Some(warning) = backend_health(store) {
            println!("⚠ {warning}");
        }
    }
    Ok(())
}

/// Probe whether the credential backend actually persists across processes.
/// Returns `None` when healthy, else a human-readable warning. Uses a
/// sentinel account (written + deleted) so no user data is touched.
fn backend_health(store: &dyn CredentialStore) -> Option<String> {
    const SENTINEL: &str = "__turya-healthcheck__";
    if store.set(SENTINEL, "ok").is_err() {
        return Some(
            "credential backend is not writable here (keychain unavailable?). \
             Export provider API keys as environment variables instead."
                .to_string(),
        );
    }
    let persisted = store.get(SENTINEL).as_deref() == Some("ok");
    let _ = store.delete(SENTINEL);
    if !persisted {
        return Some(
            "credential backend accepted a write but the entry is not readable \
             back — logins will NOT stick in this environment (per-command dbus \
             sessions, sudo/HOME mismatch, or transient keyring). Export provider \
             API keys as environment variables instead."
                .to_string(),
        );
    }
    None
}

/// Confirm a just-written secret reads back identically. Catches backends
/// that accept writes but don't persist (the silent-login bug).
fn confirm_persisted(
    store: &dyn CredentialStore,
    account: &str,
    secret: &str,
) -> Result<(), String> {
    if store.get(account).as_deref() == Some(secret) {
        return Ok(());
    }
    Err(format!(
        "keychain accepted the write but '{account}' is not readable back — \
         this backend does not persist across processes here. Nothing was \
         relied upon: export the key as an environment variable instead."
    ))
}

/// `turya auth logout`: forget one slot, or both when `method` is None.
pub fn logout(
    provider: &str,
    method: Option<&str>,
    store: &dyn CredentialStore,
) -> Result<(), String> {
    let drop_key = method.map(|m| m == "api-key").unwrap_or(true);
    let drop_oauth = method.map(|m| m == "oauth").unwrap_or(true);
    if let Some(m) = method {
        if m != "api-key" && m != "oauth" {
            return Err(format!("unknown method '{m}': expected api-key|oauth"));
        }
    }
    if drop_key {
        store
            .delete(&api_key_account(provider))
            .map_err(|e| e.to_string())?;
    }
    if drop_oauth {
        store
            .delete(&oauth_refresh_account(provider))
            .map_err(|e| e.to_string())?;
        let _ = store.delete(&oauth_client_id_account(provider));
    }
    println!("Forgot {provider} credentials.");
    Ok(())
}

/// `turya auth login`: interactive per-method flow. API key path masks input,
/// verifies with a cheap call, then stores. OAuth path runs browser PKCE.
pub async fn login(
    registry: &ProviderRegistry,
    provider: &str,
    method: Option<&str>,
    client_id: Option<&str>,
    store: &dyn CredentialStore,
) -> Result<(), String> {
    if registry.get(provider).is_none() {
        return Err(format!(
            "unknown provider '{provider}' (known: {})",
            registry_ids(registry).join(", ")
        ));
    }
    let methods = methods_for(provider);
    match method.unwrap_or("api-key") {
        "api-key" => {
            let key = rpassword::prompt_password(format!("API key for {provider}: "))
                .map_err(|e| format!("input error: {e}"))?;
            let key = key.trim().to_string();
            if key.is_empty() {
                return Err("empty key — nothing stored".to_string());
            }
            verify_key(registry, provider, &key).await?;
            store
                .set(&api_key_account(provider), &key)
                .map_err(|e| e.to_string())?;
            confirm_persisted(store, &api_key_account(provider), &key)?;
            println!("Stored {provider} API key.");
            Ok(())
        }
        "oauth" => {
            if !methods.has_oauth {
                return Err(format!("provider '{provider}' offers no OAuth flow"));
            }
            login_oauth(provider, client_id, store).await
        }
        other => Err(format!("unknown method '{other}': expected api-key|oauth")),
    }
}

/// Cheap verification before persisting: list models with the candidate key
/// through the registry trait (no vendor imports here).
async fn verify_key(
    registry: &turya_core::ProviderRegistry,
    provider: &str,
    key: &str,
) -> Result<(), String> {
    let plugin = registry
        .get(provider)
        .ok_or_else(|| format!("unknown provider '{provider}'"))?;
    let creds = turya_core::ResolvedCreds {
        token: key.to_string(),
        expires_at: None,
        via: "login",
    };
    if plugin.list_models(&creds).await.is_empty() {
        return Err("key rejected (no models listed) — not stored".to_string());
    }
    Ok(())
}

async fn login_oauth(
    provider: &str,
    client_id: Option<&str>,
    store: &dyn CredentialStore,
) -> Result<(), String> {
    let client_id = client_id
        .map(|s| s.to_string())
        .or_else(|| std::env::var("TURYA_OAUTH_CLIENT_ID").ok())
        .or_else(|| store.get(&oauth_client_id_account(provider)))
        .ok_or_else(|| {
            "OAuth needs your own Google Cloud client ID (--client-id or TURYA_OAUTH_CLIENT_ID). \
             Consumer Google accounts are not accepted by Google for this flow; \
             use an API key instead."
                .to_string()
        })?;
    let cfg = turya_auth::OAuthConfig::google(&client_id);
    let (verifier, challenge) = turya_auth::oauth::pkce_pair().map_err(|e| e.to_string())?;
    let state = format!("turya-{}", std::process::id());

    // Bind first so the redirect URI is known before opening the browser.
    let probe = tokio::net::TcpListener::bind(("127.0.0.1", cfg.redirect_port))
        .await
        .map_err(|e| format!("cannot bind localhost callback: {e}"))?;
    let port = probe.local_addr().map_err(|e| e.to_string())?.port();
    drop(probe);
    let redirect = turya_auth::oauth::redirect_uri(port);
    let url = turya_auth::oauth::build_auth_url(&cfg, &redirect, &challenge, &state)
        .map_err(|e| e.to_string())?;

    println!("Open this URL to connect {provider} (OAuth):\n{url}");
    println!(
        "Workspace/Enterprise Google accounts only — consumer accounts are rejected by Google."
    );
    if !turya_auth::oauth::open_browser(&url) {
        println!("(Could not open a browser automatically — copy the URL above.)");
    }
    // Headless fallback: user pastes the full callback URL or just the code.
    println!("Waiting up to 5 minutes (or paste the ?code= value and press Enter):");

    let callback = tokio::spawn(async move {
        turya_auth::oauth::wait_for_callback(port, &state, std::time::Duration::from_secs(300))
            .await
    });
    // Race the browser callback against one pasted line (headless fallback).
    // Empty line cancels; a full callback URL or bare code is accepted.
    let code = tokio::select! {
        res = callback => res
            .map_err(|e| e.to_string())?
            .map(|(c, _)| c)
            .map_err(|e| e.to_string())?,
        line = read_line_once() => extract_code(&line)?,
    };
    let tokens = turya_auth::oauth::exchange_code(&cfg, &redirect, code.trim(), &verifier)
        .await
        .map_err(|e| e.to_string())?;
    let refresh = tokens.refresh_token.ok_or_else(|| {
        "provider did not return a refresh token (re-run and consent offline access)".to_string()
    })?;
    store
        .set(&oauth_refresh_account(provider), &refresh)
        .map_err(|e| e.to_string())?;
    confirm_persisted(store, &oauth_refresh_account(provider), &refresh)?;
    let _ = store.set(&oauth_client_id_account(provider), &client_id);
    println!("Connected {provider} via OAuth.");
    Ok(())
}

async fn read_line_once() -> String {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    let _ = tokio::io::BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await;
    line
}

/// Accept a full callback URL or a bare code; empty input cancels.
fn extract_code(line: &str) -> Result<String, String> {
    let line = line.trim();
    if line.is_empty() {
        return Err("empty input — login cancelled".to_string());
    }
    if let Some((_, q)) = line.split_once('?') {
        for pair in q.split('&') {
            if let Some(code) = pair.strip_prefix("code=") {
                let code = code.split('&').next().unwrap_or("");
                if !code.is_empty() {
                    return Ok(code.to_string());
                }
            }
        }
    }
    Ok(line.to_string())
}

/// Default keychain store for CLI use.
pub fn default_store() -> KeychainStore {
    KeychainStore::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use turya_auth::MemStore;

    /// Reproduces the silent-login bug: accepts writes, reads back nothing
    /// (e.g. transient dbus session collections, sudo/HOME mismatch).
    struct WriteOnlyStore;

    impl CredentialStore for WriteOnlyStore {
        fn get(&self, _account: &str) -> Option<String> {
            None
        }
        fn set(&self, _account: &str, _secret: &str) -> Result<(), turya_auth::StoreError> {
            Ok(())
        }
        fn delete(&self, _account: &str) -> Result<(), turya_auth::StoreError> {
            Ok(())
        }
    }

    struct BrokenStore;

    impl CredentialStore for BrokenStore {
        fn get(&self, _account: &str) -> Option<String> {
            None
        }
        fn set(&self, _account: &str, _secret: &str) -> Result<(), turya_auth::StoreError> {
            Err(turya_auth::StoreError::Backend("no backend".to_string()))
        }
        fn delete(&self, _account: &str) -> Result<(), turya_auth::StoreError> {
            Err(turya_auth::StoreError::Backend("no backend".to_string()))
        }
    }

    #[test]
    fn confirm_persisted_catches_silent_backend() {
        let healthy = MemStore::new();
        assert!(confirm_persisted(&healthy, "gemini-api-key", "k").is_err());
        healthy.set("gemini-api-key", "k").unwrap();
        assert!(confirm_persisted(&healthy, "gemini-api-key", "k").is_ok());
        // Wrong value also fails (not just missing).
        assert!(confirm_persisted(&healthy, "gemini-api-key", "other").is_err());

        // The reported bug: write "succeeds", read finds nothing.
        let silent = WriteOnlyStore;
        silent.set("gemini-api-key", "k").unwrap();
        assert!(confirm_persisted(&silent, "gemini-api-key", "k").is_err());
    }

    #[test]
    fn backend_health_distinguishes_failure_modes() {
        assert!(backend_health(&MemStore::new()).is_none());
        let broken = backend_health(&BrokenStore).expect("broken backend must warn");
        assert!(broken.contains("not writable"));
        let silent = backend_health(&WriteOnlyStore).expect("silent backend must warn");
        assert!(silent.contains("not readable"));
    }
}
