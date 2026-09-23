//! ORCID Authorization Code flow (ADR-0010). The access token is used once to read the
//! authenticated iD and name from the token response and is then discarded.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use zeroize::Zeroizing;

use crate::config::OrcidConfig;
use crate::error::ApiError;

/// Authenticated identity returned by ORCID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrcidIdentity {
    /// ORCID iD, e.g. `0000-0002-1825-0097`.
    pub orcid: String,
    /// Display name reported by ORCID (may be empty).
    pub name: String,
}

/// Scope requested: authentication only.
pub const SCOPE: &str = "/authenticate";

/// Provider endpoints.
#[derive(Clone, Debug)]
pub struct Endpoints {
    /// Authorization endpoint.
    pub authorize_url: String,
    /// Token endpoint.
    pub token_url: String,
}

impl Endpoints {
    /// ORCID sandbox.
    pub fn sandbox() -> Self {
        Self {
            authorize_url: "https://sandbox.orcid.org/oauth/authorize".into(),
            token_url: "https://sandbox.orcid.org/oauth/token".into(),
        }
    }
    /// ORCID production.
    pub fn production() -> Self {
        Self {
            authorize_url: "https://orcid.org/oauth/authorize".into(),
            token_url: "https://orcid.org/oauth/token".into(),
        }
    }
}

/// Provider implementation.
pub enum OrcidProvider {
    /// No ORCID sign-in configured: the portal explains it; admin-issued API tokens still work.
    Disabled,
    /// Real ORCID (sandbox or production), untested until credentials exist (docs/STATUS.md).
    Real {
        /// Endpoints.
        endpoints: Endpoints,
        /// Client id.
        client_id: String,
        /// Client secret (zeroized on drop).
        client_secret: Zeroizing<String>,
        /// Registered redirect URI.
        redirect_uri: String,
        /// HTTP client with verified TLS.
        http: reqwest::Client,
    },
    /// In-process simulator for development and tests. Never available on public deployments.
    Simulated {
        /// Issued codes.
        codes: Mutex<HashMap<String, (OrcidIdentity, Instant)>>,
        /// Redirect URI.
        redirect_uri: String,
    },
}

impl std::fmt::Debug for OrcidProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrcidProvider::Real {
                endpoints,
                client_id,
                ..
            } => write!(
                f,
                "OrcidProvider::Real({}, client {client_id})",
                endpoints.authorize_url
            ),
            OrcidProvider::Simulated { .. } => write!(f, "OrcidProvider::Simulated"),
            OrcidProvider::Disabled => write!(f, "OrcidProvider::Disabled"),
        }
    }
}

/// Only the identity fields are deserialized; `access_token`/`refresh_token` are ignored on purpose.
#[derive(Deserialize)]
struct TokenResponse {
    scope: Option<String>,
    orcid: Option<String>,
    name: Option<String>,
}

/// Validate an ORCID iD: four groups of four characters, ISO 7064 MOD 11-2 checksum.
pub fn is_valid_orcid(s: &str) -> bool {
    let bytes: Vec<u8> = s.bytes().collect();
    if bytes.len() != 19 {
        return false;
    }
    let mut digits = Vec::with_capacity(16);
    for (i, b) in bytes.iter().enumerate() {
        if i == 4 || i == 9 || i == 14 {
            if *b != b'-' {
                return false;
            }
            continue;
        }
        digits.push(*b);
    }
    let mut total: u32 = 0;
    for d in &digits[..15] {
        if !d.is_ascii_digit() {
            return false;
        }
        total = (total + (d - b'0') as u32) * 2;
    }
    let remainder = total % 11;
    let result = (12 - remainder) % 11;
    let expected = if result == 10 {
        b'X'
    } else {
        b'0' + result as u8
    };
    digits[15] == expected
}

impl OrcidProvider {
    /// Build from configuration. The client secret is read from the configured environment variable.
    pub fn from_config(cfg: &OrcidConfig) -> Result<Self, String> {
        match cfg.provider.as_str() {
            "disabled" => Ok(OrcidProvider::Disabled),
            "simulated" => Ok(OrcidProvider::Simulated {
                codes: Mutex::new(HashMap::new()),
                redirect_uri: cfg.redirect_uri.clone(),
            }),
            "sandbox" | "production" => {
                let endpoints = if cfg.provider == "sandbox" {
                    Endpoints::sandbox()
                } else {
                    Endpoints::production()
                };
                let secret = std::env::var(&cfg.client_secret_env).map_err(|_| {
                    format!(
                        "environment variable {} (ORCID client secret) is not set",
                        cfg.client_secret_env
                    )
                })?;
                Self::real(
                    endpoints,
                    cfg.client_id.clone(),
                    Zeroizing::new(secret),
                    cfg.redirect_uri.clone(),
                )
            }
            other => Err(format!("unknown orcid.provider '{other}'")),
        }
    }

    /// Real provider with explicit endpoints (tests use a local mock token endpoint).
    pub fn real(
        endpoints: Endpoints,
        client_id: String,
        client_secret: Zeroizing<String>,
        redirect_uri: String,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        Ok(OrcidProvider::Real {
            endpoints,
            client_id,
            client_secret,
            redirect_uri,
            http,
        })
    }

    /// Whether this is the simulator.
    pub fn is_simulated(&self) -> bool {
        matches!(self, OrcidProvider::Simulated { .. })
    }

    /// Whether sign-in is available.
    pub fn is_enabled(&self) -> bool {
        !matches!(self, OrcidProvider::Disabled)
    }

    /// Registered redirect URI.
    pub fn redirect_uri(&self) -> &str {
        match self {
            OrcidProvider::Real { redirect_uri, .. }
            | OrcidProvider::Simulated { redirect_uri, .. } => redirect_uri,
            OrcidProvider::Disabled => "",
        }
    }

    /// URL the browser is sent to for `state`.
    pub fn authorize_url(&self, state: &str) -> String {
        match self {
            OrcidProvider::Real {
                endpoints,
                client_id,
                redirect_uri,
                ..
            } => format!(
                "{}?client_id={}&response_type=code&scope={}&redirect_uri={}&state={}",
                endpoints.authorize_url,
                urlencode(client_id),
                urlencode(SCOPE),
                urlencode(redirect_uri),
                urlencode(state)
            ),
            OrcidProvider::Simulated { .. } => {
                format!("/portal/dev/orcid/authorize?state={}", urlencode(state))
            }
            OrcidProvider::Disabled => "/portal".to_owned(),
        }
    }

    /// Simulator: issue a one-time code for an identity.
    pub fn simulated_issue_code(&self, identity: OrcidIdentity) -> Result<String, ApiError> {
        let OrcidProvider::Simulated { codes, .. } = self else {
            return Err(ApiError::NotFound("simulator".into()));
        };
        if !is_valid_orcid(&identity.orcid) {
            return Err(ApiError::Unprocessable(
                "invalid ORCID iD (checksum)".into(),
            ));
        }
        let code = crate::auth::new_token("simcode_")?;
        let mut map = codes
            .lock()
            .map_err(|_| ApiError::Internal("simulator lock".into()))?;
        map.retain(|_, (_, t)| t.elapsed() < Duration::from_secs(600));
        map.insert(code.clone(), (identity, Instant::now()));
        Ok(code)
    }

    /// Exchange an authorization code for the authenticated identity. The access token is dropped.
    pub async fn exchange(&self, code: &str) -> Result<OrcidIdentity, ApiError> {
        if code.is_empty() || code.len() > 256 {
            return Err(ApiError::BadRequest("invalid code".into()));
        }
        match self {
            OrcidProvider::Disabled => {
                Err(ApiError::NotFound("ORCID sign-in is not configured".into()))
            }
            OrcidProvider::Simulated { codes, .. } => {
                let mut map = codes
                    .lock()
                    .map_err(|_| ApiError::Internal("simulator lock".into()))?;
                match map.remove(code) {
                    Some((identity, issued)) if issued.elapsed() < Duration::from_secs(600) => {
                        Ok(identity)
                    }
                    _ => Err(ApiError::Forbidden(
                        "unknown or expired simulated code".into(),
                    )),
                }
            }
            OrcidProvider::Real {
                endpoints,
                client_id,
                client_secret,
                redirect_uri,
                http,
            } => {
                let form = [
                    ("client_id", client_id.as_str()),
                    ("client_secret", client_secret.as_str()),
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", redirect_uri.as_str()),
                ];
                let resp = http
                    .post(&endpoints.token_url)
                    .header("Accept", "application/json")
                    .form(&form)
                    .send()
                    .await
                    .map_err(|e| ApiError::Internal(format!("orcid token request: {e}")))?;
                if !resp.status().is_success() {
                    tracing::warn!(status = %resp.status(), "orcid token endpoint refused the code");
                    return Err(ApiError::Forbidden(
                        "ORCID refused the authorization code".into(),
                    ));
                }
                let token: TokenResponse = resp
                    .json()
                    .await
                    .map_err(|e| ApiError::Internal(format!("orcid token response: {e}")))?;
                let orcid = token.orcid.unwrap_or_default();
                if !is_valid_orcid(&orcid) {
                    return Err(ApiError::Internal(
                        "orcid token response without a valid iD".into(),
                    ));
                }
                if let Some(scope) = &token.scope
                    && !scope.contains(SCOPE)
                    && !scope.contains("openid")
                {
                    tracing::warn!(%scope, "unexpected ORCID scope");
                }
                Ok(OrcidIdentity {
                    orcid,
                    name: token.name.unwrap_or_default().chars().take(128).collect(),
                })
            }
        }
    }
}

/// Minimal percent-encoding for query values.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orcid_checksum() {
        assert!(is_valid_orcid("0000-0002-1825-0097")); // Josiah Carberry
        assert!(is_valid_orcid("0000-0001-5109-3700"));
        assert!(is_valid_orcid("0000-0002-1694-233X"));
        assert!(!is_valid_orcid("0000-0002-1825-0098"));
        assert!(!is_valid_orcid("0000-0002-1825-009"));
        assert!(!is_valid_orcid("0000_0002_1825_0097"));
    }

    #[test]
    fn authorize_url_contains_required_parameters() {
        let p = OrcidProvider::real(
            Endpoints::sandbox(),
            "APP-1".into(),
            Zeroizing::new("SECRET-VALUE".into()),
            "https://example.org/portal/callback".into(),
        )
        .unwrap();
        let u = p.authorize_url("abc");
        assert!(u.starts_with("https://sandbox.orcid.org/oauth/authorize?"));
        for part in [
            "client_id=APP-1",
            "response_type=code",
            "scope=%2Fauthenticate",
            "redirect_uri=https%3A%2F%2Fexample.org%2Fportal%2Fcallback",
            "state=abc",
        ] {
            assert!(u.contains(part), "{u} lacks {part}");
        }
        assert!(
            !format!("{p:?}").contains("SECRET-VALUE"),
            "secret must not appear in Debug: {p:?}"
        );
    }
}
