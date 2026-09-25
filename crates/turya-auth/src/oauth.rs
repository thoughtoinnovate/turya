//! OAuth 2.0 PKCE flow (browser + localhost callback) plus token refresh.
//!
//! Targets Google-style endpoints by default but carries no hardcoded
//! provider credentials: the caller supplies `client_id` (the user's own
//! Cloud project). Consumer Google accounts are NOT expected to work here
//! (Google removed consumer Code Assist OAuth); the CLI surfaces that.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("network: {0}")]
    Network(String),
    #[error("provider rejected the request: {0}")]
    Rejected(String),
    #[error("bad callback (missing or mismatched code/state)")]
    BadCallback,
    #[error("timed out waiting for the browser callback")]
    Timeout,
    #[error("io: {0}")]
    Io(String),
}

/// OAuth endpoints + client identity. `client_id` is the user's own
/// Google Cloud OAuth client (never baked into the binary).
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub auth_url: String,
    pub token_url: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    /// Loopback redirect, e.g. `http://127.0.0.1:PORT/callback`.
    /// `redirect_port: 0` means "pick an ephemeral port at runtime".
    pub redirect_port: u16,
}

impl OAuthConfig {
    /// Google endpoints with caller-supplied client id.
    pub fn google(client_id: &str) -> Self {
        Self {
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
            token_url: "https://oauth2.googleapis.com/token".to_string(),
            client_id: client_id.to_string(),
            scopes: vec!["https://www.googleapis.com/auth/cloud-platform".to_string()],
            redirect_port: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: u64,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
}

/// Generate a PKCE code verifier (64 random chars) + S256 challenge.
pub fn pkce_pair() -> Result<(String, String), OAuthError> {
    let mut buf = [0u8; 48];
    getrandom::getrandom(&mut buf).map_err(|e| OAuthError::Io(e.to_string()))?;
    let verifier = URL_SAFE_NO_PAD.encode(buf);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok((verifier, challenge))
}

/// Build the user-facing authorization URL.
pub fn build_auth_url(
    cfg: &OAuthConfig,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> Result<String, OAuthError> {
    let mut url = url::Url::parse(&cfg.auth_url).map_err(|e| OAuthError::Io(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", &cfg.scopes.join(" "))
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    Ok(url.to_string())
}

/// Exchange an authorization code for tokens.
pub async fn exchange_code(
    cfg: &OAuthConfig,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<TokenResponse, OAuthError> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", cfg.client_id.as_str()),
        ("code_verifier", verifier),
    ];
    post_token(&cfg.token_url, &params).await
}

/// Refresh an access token. Returns the new token set (rotation-aware:
/// callers persist a returned `refresh_token` when present).
pub async fn refresh(cfg: &OAuthConfig, refresh_token: &str) -> Result<TokenResponse, OAuthError> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", cfg.client_id.as_str()),
    ];
    post_token(&cfg.token_url, &params).await
}

async fn post_token(token_url: &str, params: &[(&str, &str)]) -> Result<TokenResponse, OAuthError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("turya-oauth")
        .build()
        .map_err(|e| OAuthError::Network(e.to_string()))?;
    let resp = client
        .post(token_url)
        .form(params)
        .send()
        .await
        .map_err(|e| OAuthError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        let short: String = body.chars().take(300).collect();
        return Err(OAuthError::Rejected(short));
    }
    resp.json::<TokenResponse>()
        .await
        .map_err(|e| OAuthError::Rejected(format!("bad token response: {e}")))
}

/// Wait for the browser callback on 127.0.0.1 (ephemeral or fixed port).
/// Returns `(code, redirect_uri_used)`. Validates `state` when expected.
pub async fn wait_for_callback(
    port: u16,
    expected_state: &str,
    timeout: Duration,
) -> Result<(String, String), OAuthError> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| OAuthError::Io(format!("cannot bind localhost callback: {e}")))?;
    let actual_port = listener
        .local_addr()
        .map_err(|e| OAuthError::Io(e.to_string()))?
        .port();
    let _ = actual_port; // reported via redirect_uri() helper below instead
    let (mut socket, _) = tokio::time::timeout(timeout, listener.accept())
        .await
        .map_err(|_| OAuthError::Timeout)?
        .map_err(|e| OAuthError::Io(e.to_string()))?;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut reader = tokio::io::BufReader::new(&mut socket);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .map_err(|e| OAuthError::Io(e.to_string()))?;
    // Drain headers so the browser isn't left hanging.
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| OAuthError::Io(e.to_string()))?;
        if line.trim().is_empty() {
            break;
        }
    }
    let body = "<html><body><h3>Turya connected — you can close this tab.</h3></body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    reader
        .get_mut()
        .write_all(response.as_bytes())
        .await
        .map_err(|e| OAuthError::Io(e.to_string()))?;

    // GET /callback?code=...&state=...
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or(OAuthError::BadCallback)?;
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "code" => code = Some(v.to_string()),
            "state" => state = Some(v.to_string()),
            _ => {}
        }
    }
    match (code, state) {
        (Some(c), Some(s)) if s == expected_state => Ok((c, String::new())),
        _ => Err(OAuthError::BadCallback),
    }
}

/// Canonical redirect URI for a bound port.
pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

/// Open a URL in the user's browser. Returns false when no opener exists
/// (headless) — callers then fall back to showing the URL for manual copy.
pub fn open_browser(url: &str) -> bool {
    open::that(url).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_rfc7636_vector() {
        // RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn pkce_pair_is_url_safe() {
        let (v, c) = pkce_pair().unwrap();
        assert!(v.len() >= 43);
        assert!(!v.contains(['+', '/', '=']));
        assert!(!c.contains(['+', '/', '=']));
    }

    #[test]
    fn auth_url_carries_pkce_and_offline() {
        let cfg = OAuthConfig::google("my-client-id");
        let url = build_auth_url(&cfg, &redirect_uri(43721), "CHAL", "ST").unwrap();
        assert!(url.contains("code_challenge=CHAL"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("client_id=my-client-id"));
        assert!(url.contains("127.0.0.1%3A43721") || url.contains("127.0.0.1:43721"));
    }

    #[tokio::test]
    async fn callback_roundtrip_and_state_check() {
        // Bind an ephemeral port first so the test knows the redirect URI.
        let probe = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let server = tokio::spawn(async move {
            wait_for_callback(port, "s3cr3t", Duration::from_secs(10)).await
        });
        // Give the listener a moment to bind (retry the client side).
        let mut last = String::new();
        for _ in 0..50 {
            match reqwest::Client::new()
                .get(format!(
                    "http://127.0.0.1:{port}/callback?code=AUTHCODE&state=s3cr3t"
                ))
                .send()
                .await
            {
                Ok(r) => {
                    last = r.text().await.unwrap_or_default();
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        assert!(last.contains("Turya connected"));
        let (code, _) = server.await.unwrap().unwrap();
        assert_eq!(code, "AUTHCODE");
    }

    #[tokio::test]
    async fn callback_rejects_state_mismatch() {
        let probe = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let server = tokio::spawn(async move {
            wait_for_callback(port, "expected", Duration::from_secs(10)).await
        });
        for _ in 0..50 {
            if reqwest::Client::new()
                .get(format!(
                    "http://127.0.0.1:{port}/callback?code=X&state=wrong"
                ))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(matches!(
            server.await.unwrap().unwrap_err(),
            OAuthError::BadCallback
        ));
    }
}
