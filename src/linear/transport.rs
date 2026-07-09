//! The sync HTTP seam for the Linear adapter: a [`LinearTransport`] trait (tests
//! inject fakes; nothing else in the crate touches HTTP) and the production
//! [`UreqTransport`]. The single [`linear_to_lane`] translation maps every transport
//! failure to `LaneError::Network` — closed classifications only, never response-body
//! dumps, never the auth header.

use std::time::Duration;

use serde_json::Value;

use crate::error::LaneError;
use crate::secrets::SecretValue;

/// Default bounded wait for one HTTP round trip (mirrors the git adapter's 10s).
pub const DEFAULT_LINEAR_TIMEOUT: Duration = Duration::from_secs(10);

/// Transport-layer failure classes. Messages never contain response bodies.
#[derive(Debug)]
pub enum TransportError {
    /// Non-2xx HTTP status; `hint` is a closed classification of the status.
    Http { status: u16, hint: String },
    /// Connect / TLS / timeout / IO-level failure (client's own message; no bodies).
    Network(String),
    /// The response body was not valid JSON.
    Malformed(String),
    /// The `api_url` violates transport policy (https required; loopback exempt).
    UrlPolicy(String),
}

/// The injectable HTTP seam. `auth` is exposed ONLY into the `Authorization` header
/// inside implementations (Linear personal keys ride raw — no `Bearer` prefix).
pub trait LinearTransport {
    /// POST the GraphQL envelope to `url`; return the parsed top-level response JSON.
    fn post_json(
        &self,
        url: &str,
        auth: &SecretValue,
        body: &Value,
    ) -> Result<Value, TransportError>;
}

/// The production transport: blocking `ureq` agent with a global timeout; HTTP
/// statuses are classified here (not raised as client errors).
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_LINEAR_TIMEOUT)
    }
    /// Custom timeout (tests pin the bounded-wait path).
    pub fn with_timeout(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// Transport policy: a resolved secret is only ever SENT over https — except to a
/// loopback host, the hermetic-test exemption. Closes the cleartext-key misconfig
/// (an operator-edited `api_url` of `http://…` would otherwise ship the key raw).
fn check_url_policy(url: &str) -> Result<(), TransportError> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        // Reject ANY userinfo (`user[:pass]@host`) outright. ureq/`http` derive the
        // connect host as the substring AFTER the last `@`, so a value like
        // `http://127.0.0.1:1@evil.com/` would otherwise let a loopback token in the
        // USERINFO pass this check while the key is shipped cleartext to `evil.com`.
        // A genuine loopback authority never carries userinfo, so `@` ⇒ fail closed.
        if !authority.contains('@') {
            let host = if let Some(v6) = authority.strip_prefix('[') {
                // Bracketed IPv6 authority: compare the bracketed host.
                match v6.split(']').next() {
                    Some(h) => format!("[{h}]"),
                    None => authority.to_string(),
                }
            } else {
                authority.split(':').next().unwrap_or("").to_string()
            };
            if host == "127.0.0.1" || host == "localhost" || host == "[::1]" {
                return Ok(());
            }
        }
    }
    Err(TransportError::UrlPolicy(
        "api_url must be https:// (plain http is allowed only for loopback test fixtures)"
            .to_string(),
    ))
}

/// Closed hint per HTTP status class — actionable, body-free.
fn hint_for(status: u16) -> String {
    match status {
        400 | 401 => {
            "authentication failed; check the linear_api role mapping and `op signin`".to_string()
        }
        403 => "the API key lacks permission for this operation".to_string(),
        404 => "endpoint not found; check [linear] api_url".to_string(),
        429 => "Linear rate limit hit; retry later".to_string(),
        s if s >= 500 => "Linear server error; retry later".to_string(),
        _ => "unexpected HTTP status".to_string(),
    }
}

impl LinearTransport for UreqTransport {
    fn post_json(
        &self,
        url: &str,
        auth: &SecretValue,
        body: &Value,
    ) -> Result<Value, TransportError> {
        check_url_policy(url)?;
        let payload = body.to_string();
        let mut resp = self
            .agent
            .post(url)
            // Linear personal API keys ride raw — deliberately NOT `Bearer <key>`.
            .header("Authorization", auth.expose())
            .header("Content-Type", "application/json")
            .send(payload.as_str())
            .map_err(|e| TransportError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        // Classify the status BEFORE touching the body: a non-2xx response with a
        // non-UTF-8 or over-limit body must still surface its actionable per-status
        // hint, not get misreported as a generic transport/network error. Error
        // bodies are never read, classified from, stored, or surfaced.
        if !(200..300).contains(&status) {
            return Err(TransportError::Http {
                status,
                hint: hint_for(status),
            });
        }
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| TransportError::Network(format!("reading response: {e}")))?;
        serde_json::from_str(&text)
            .map_err(|e| TransportError::Malformed(format!("response is not JSON: {e}")))
    }
}

/// The single transport→lane translation (the `git_to_lane` precedent).
pub fn linear_to_lane(e: TransportError) -> LaneError {
    LaneError::Network(match e {
        TransportError::Http { status, hint } => {
            format!("linear API returned HTTP {status}: {hint}")
        }
        TransportError::Network(m) => format!("linear API unreachable: {m}"),
        TransportError::Malformed(m) => format!("linear API response malformed: {m}"),
        TransportError::UrlPolicy(m) => m,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_policy_https_and_loopback_only() {
        assert!(check_url_policy("https://api.linear.app/graphql").is_ok());
        assert!(check_url_policy("http://127.0.0.1:8080/graphql").is_ok());
        assert!(check_url_policy("http://localhost:9/x").is_ok());
        assert!(check_url_policy("http://[::1]:4000/graphql").is_ok());
        assert!(matches!(
            check_url_policy("http://10.0.0.5/graphql"),
            Err(TransportError::UrlPolicy(_))
        ));
        assert!(matches!(
            check_url_policy("http://evil.example/graphql"),
            Err(TransportError::UrlPolicy(_))
        ));
        assert!(matches!(
            check_url_policy("ftp://127.0.0.1/x"),
            Err(TransportError::UrlPolicy(_))
        ));
        // localhost-prefixed non-loopback hosts must not pass.
        assert!(matches!(
            check_url_policy("http://localhost.evil.example/x"),
            Err(TransportError::UrlPolicy(_))
        ));
        // Userinfo tricks: a loopback token in the USERINFO must NOT pass — ureq
        // connects to the post-`@` host, so these would ship the key to `evil.com`.
        for evil in [
            "http://127.0.0.1@evil.com/graphql",
            "http://127.0.0.1:1@evil.com/graphql",
            "http://localhost:1@evil.com/graphql",
            "http://[::1]@evil.com/graphql",
            "http://[::1]:5@evil.com/graphql",
        ] {
            assert!(
                matches!(check_url_policy(evil), Err(TransportError::UrlPolicy(_))),
                "userinfo bypass not rejected: {evil}"
            );
        }
    }

    #[test]
    fn hints_are_closed_and_actionable() {
        assert!(hint_for(401).contains("linear_api"));
        assert!(hint_for(429).contains("rate limit"));
        assert!(hint_for(500).contains("server error"));
    }
}
