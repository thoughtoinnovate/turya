//! Plugin manifest (`turya-plugin.toml`) parsing + validation (Rule 3.2).
//!
//! Every plugin — internal native or external Wasm — declares:
//! - `kind`: `internal` (shipped, mandatory) or `external` (community).
//! - capabilities: HTTP hosts, filesystem paths, credential storage.
//! - contributions: providers (with `base_provider` for metadata lookup),
//!   and optional `oauth` flows for provider authentication.

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("invalid manifest: {0}")]
    Invalid(String),
    #[error("parse error: {0}")]
    Parse(String),
}

/// Rule 3.2 taxonomy: shipped natives vs community sandbox guests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    Internal,
    External,
}

impl Default for PluginKind {
    /// Unmarked manifests are guests: least privilege by default.
    fn default() -> Self {
        Self::External
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginMeta {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub kind: PluginKind,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ManifestCapabilities {
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// May persist tokens/credentials (keychain or plugin storage).
    #[serde(default)]
    pub storage: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderContribution {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    /// Canonical key for metadata lookup (e.g. `"google"` for gemini).
    #[serde(default)]
    pub base_provider: String,
}

/// OAuth2 flow type for provider authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum OAuthFlowType {
    Pkce,
    DeviceFlow,
}

/// OAuth2 configuration contributed by a provider plugin.
#[derive(Debug, Clone, Deserialize)]
pub struct OAuthConfig {
    #[serde(default = "default_flow")]
    pub flow: OAuthFlowType,
    pub auth_url: String,
    pub token_url: String,
    /// Env var holding the client id, or a literal `client_id:` value.
    /// Raw secrets are NEVER committed in manifests (Rule 1.2).
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub credentials_path: String,
}

fn default_flow() -> OAuthFlowType {
    OAuthFlowType::Pkce
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Contributions {
    #[serde(default)]
    pub providers: Vec<ProviderContribution>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub plugin: PluginMeta,
    #[serde(default)]
    pub capabilities: ManifestCapabilities,
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
    #[serde(default)]
    pub contributes: Contributions,
}

impl PluginManifest {
    pub fn parse_toml(text: &str) -> Result<Self, ManifestError> {
        toml::from_str(text).map_err(|e| ManifestError::Parse(e.to_string()))
    }

    /// Rule 3.2/3.4 validation. Pure and offline.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let invalid = |msg: String| ManifestError::Invalid(msg);
        if self.plugin.name.trim().is_empty() {
            return Err(invalid("plugin.name must not be empty".to_string()));
        }
        for p in &self.contributes.providers {
            if p.id.trim().is_empty() {
                return Err(invalid(
                    "contributed provider id must not be empty".to_string(),
                ));
            }
        }
        if let Some(oauth) = &self.oauth {
            for (field, url) in [
                ("auth_url", &oauth.auth_url),
                ("token_url", &oauth.token_url),
            ] {
                if !url.starts_with("https://") {
                    return Err(invalid(format!(
                        "oauth.{field} must be https (got '{url}')"
                    )));
                }
            }
            // Loopback only: tokens must never flow to a remote redirect.
            if !oauth.redirect_uri.is_empty()
                && !(oauth.redirect_uri.starts_with("http://127.0.0.1")
                    || oauth.redirect_uri.starts_with("http://localhost"))
            {
                return Err(invalid(format!(
                    "oauth.redirect_uri must be loopback (got '{}')",
                    oauth.redirect_uri
                )));
            }
            if oauth.client_id.trim().is_empty() {
                return Err(invalid(
                    "oauth.client_id must name an env var or client id".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Convert a validated manifest's network scope into the runtime gate.
    pub fn capability(&self) -> super::PluginCapability {
        super::PluginCapability {
            allowed_hosts: self.capabilities.allowed_hosts.clone(),
            allowed_paths: self.capabilities.allowed_paths.clone(),
            storage: self.capabilities.storage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[plugin]
name = "gemini-oauth"
version = "0.2.0"
kind = "internal"

[capabilities]
allowed_hosts = ["oauth2.googleapis.com", "generativelanguage.googleapis.com"]
storage = true

[oauth]
flow = "Pkce"
auth_url = "https://accounts.google.com/o/oauth2/v2/auth"
token_url = "https://oauth2.googleapis.com/token"
client_id = "TURYA_OAUTH_CLIENT_ID"
redirect_uri = "http://127.0.0.1:0/callback"

[[contributes.providers]]
id = "gemini"
display_name = "Gemini"
base_provider = "google"
"#;

    #[test]
    fn valid_manifest_parses_and_validates() {
        let m = PluginManifest::parse_toml(VALID).unwrap();
        assert_eq!(m.plugin.kind, PluginKind::Internal);
        m.validate().unwrap();
        assert_eq!(m.contributes.providers.len(), 1);
        assert_eq!(m.contributes.providers[0].base_provider, "google");
        assert!(m.capabilities.storage);
        let cap = m.capability();
        assert!(cap
            .allowed_hosts
            .contains(&"oauth2.googleapis.com".to_string()));
    }

    #[test]
    fn unmarked_kind_defaults_to_external() {
        let m = PluginManifest::parse_toml("[plugin]\nname = \"x\"\n").unwrap();
        assert_eq!(m.plugin.kind, PluginKind::External);
        m.validate().unwrap();
    }

    #[test]
    fn rejects_empty_name_and_provider_id() {
        let m = PluginManifest::parse_toml("[plugin]\nname = \"\"\n").unwrap();
        assert!(m.validate().is_err());
        let bad = "[plugin]\nname = \"x\"\n[[contributes.providers]]\nid = \"\"\n";
        let m = PluginManifest::parse_toml(bad).unwrap();
        assert!(m.validate().is_err());
    }

    #[test]
    fn oauth_requires_https_loopback_and_client() {
        let base = VALID.to_string();
        for (field, bad) in [
            ("auth_url", "http://evil.test/auth"),
            ("token_url", "http://evil.test/token"),
        ] {
            let text = base.replacen(
                if field == "auth_url" {
                    "auth_url = \"https://accounts.google.com/o/oauth2/v2/auth\""
                } else {
                    "token_url = \"https://oauth2.googleapis.com/token\""
                },
                &format!("{field} = \"{bad}\""),
                1,
            );
            let m = PluginManifest::parse_toml(&text).unwrap();
            assert!(m.validate().is_err(), "{field} must require https");
        }
        let text = base.replace(
            "redirect_uri = \"http://127.0.0.1:0/callback\"",
            "redirect_uri = \"https://evil.test/cb\"",
        );
        let m = PluginManifest::parse_toml(&text).unwrap();
        assert!(m.validate().is_err());
        let text = base.replace("client_id = \"TURYA_OAUTH_CLIENT_ID\"", "client_id = \"\"");
        let m = PluginManifest::parse_toml(&text).unwrap();
        assert!(m.validate().is_err());
    }
}
