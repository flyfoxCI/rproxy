//! HTTP-only management adapter. It never opens server configuration or SQLite.
use reqwest::{redirect::Policy, Method, StatusCode};
use serde_json::Value;
use std::time::{Duration, Instant};
use thiserror::Error;
use url::Url;

const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("management HTTP request failed (status {0}); response body withheld")]
    Http(u16),
    #[error("{0}")]
    Safe(&'static str),
}

/// A root bearer client with explicit management-version/capability negotiation.
/// Debug intentionally is not implemented: the bearer must never appear in errors.
pub struct RootClient {
    client: reqwest::Client,
    endpoint: Url,
    token: String,
    timeout: Duration,
}

impl RootClient {
    pub fn new(endpoint: &str, token_env: &str, timeout: Duration) -> Result<Self, ControlError> {
        let endpoint = Url::parse(endpoint)
            .map_err(|_| ControlError::Safe("invalid management endpoint URL"))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(ControlError::Safe(
                "endpoint must be HTTP(S) without credentials, query or fragment",
            ));
        }
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(ControlError::Safe(
                "timeout must be between 1 and 300 seconds",
            ));
        }
        let token = std::env::var(token_env).map_err(|_| {
            ControlError::Safe("root bearer environment variable is missing or invalid")
        })?;
        if token.is_empty() || !token.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
            return Err(ControlError::Safe(
                "root bearer environment variable is empty or invalid",
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .map_err(|_| ControlError::Safe("could not initialize management HTTP client"))?;
        Ok(Self {
            client,
            endpoint,
            token,
            timeout,
        })
    }

    pub async fn check_management(&self, capability: Option<&str>) -> Result<(), ControlError> {
        let info = self
            .request(Method::GET, "/v1/info", &[], None)
            .await
            .map_err(|_| {
                ControlError::Safe(
                    "management version check failed; server must support /v1/info version 1",
                )
            })?;
        if info.get("management_version").and_then(Value::as_u64) != Some(1) {
            return Err(ControlError::Safe(
                "unsupported management version; version 1 is required",
            ));
        }
        if let Some(capability) = capability {
            let supported = info
                .get("capabilities")
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(capability)));
            if !supported {
                return Err(ControlError::Safe(
                    "server is missing the required management capability",
                ));
            }
        }
        Ok(())
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<Value, ControlError> {
        self.request_before(method, path, query, body, None).await
    }

    pub async fn request_before(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        deadline: Option<Instant>,
    ) -> Result<Value, ControlError> {
        if !path.starts_with('/') || path.starts_with("//") || path.contains(['?', '#']) {
            return Err(ControlError::Safe("invalid management route"));
        }
        let mut url = self.endpoint.clone();
        url.set_path(&format!(
            "{}{}",
            self.endpoint.path().trim_end_matches('/'),
            path
        ));
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (k, v)));
        }
        let timeout = match deadline {
            Some(deadline) => self.timeout.min(
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or(ControlError::Safe(
                        "wait timed out; operation remains pending",
                    ))?,
            ),
            None => self.timeout,
        };
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(&self.token)
            .timeout(timeout);
        if let Some(body) = body {
            let bytes = serde_json::to_vec(body)
                .map_err(|_| ControlError::Safe("could not encode management request"))?;
            if bytes.len() > MAX_REQUEST {
                return Err(ControlError::Safe(
                    "management request exceeds the 64 KiB limit; split the import",
                ));
            }
            request = request
                .header("Content-Type", "application/json")
                .body(bytes);
        }
        let mut response = request.send().await.map_err(|_| {
            ControlError::Safe("management transport failed or timed out; details withheld")
        })?;
        if !response.status().is_success() {
            return Err(ControlError::Http(response.status().as_u16()));
        }
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(serde_json::json!({}));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            ControlError::Safe("could not read management response; details withheld")
        })? {
            if bytes.len() + chunk.len() > MAX_RESPONSE {
                return Err(ControlError::Safe(
                    "management response exceeds client size limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| ControlError::Safe("invalid management JSON response; body withheld"))
    }
}

/// Encode a single resource name, never interpreting it as a route or query.
pub fn resource_path(group: &str, name: &str) -> Result<String, ControlError> {
    if name.is_empty()
        || name.len() > 256
        || name.chars().any(char::is_control)
        || name.contains(['/', '\\', '?', '#'])
        || matches!(name, "." | "..")
    {
        return Err(ControlError::Safe("invalid management resource name"));
    }
    let encoded: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
    Ok(format!("/v1/{group}/{}", encoded.replace('+', "%20")))
}
