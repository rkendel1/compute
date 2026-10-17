//! Cloudflare's v4 API and workers.dev invocations, translated into
//! Compute's provider error model. Cloudflare's own codes and messages are
//! kept in each error; the credential never is.

use std::time::Duration;

use compute_provider::{ProviderError, ProviderErrorKind};
use reqwest::{Method, StatusCode};
use serde_json::Value;

const API_TIMEOUT: Duration = Duration::from_secs(60);

/// An API token. It is never displayed, serialized, or logged.
#[derive(Clone)]
pub struct Credential(String);

impl Credential {
    pub fn new(token: impl Into<String>) -> Option<Self> {
        let token = token.into();
        let token = token.trim();
        (!token.is_empty()).then(|| Self(token.to_owned()))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Credential(<redacted>)")
    }
}

/// Where the credential comes from, for messages: never the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    Environment(String),
    File(String),
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Environment(name) => write!(formatter, "environment variable {name}"),
            Self::File(path) => write!(formatter, "token file {path}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: String,
    account_id: String,
    credential: Option<Credential>,
    source: CredentialSource,
}

/// A raw workers.dev response: the harness's JSON, or Cloudflare's own
/// error page when the request never reached the harness.
#[derive(Debug, Clone)]
pub struct InvocationResponse {
    pub status: u16,
    pub cf_ray: Option<String>,
    pub json: Option<Value>,
    pub text: String,
}

impl ApiClient {
    pub fn new(
        base: &str,
        account_id: &str,
        credential: Option<Credential>,
        source: CredentialSource,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("compute/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("HTTP client"),
            base: base.trim_end_matches('/').to_owned(),
            account_id: account_id.to_owned(),
            credential,
            source,
        }
    }

    pub fn has_credential(&self) -> bool {
        self.credential.is_some()
    }

    pub fn source(&self) -> &CredentialSource {
        &self.source
    }

    pub(crate) fn credential(&self) -> Result<&Credential, ProviderError> {
        self.credential.as_ref().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Unauthorized,
                format!(
                    "no Cloudflare API token: set {} to an API token with the Workers Scripts Edit permission for account {}",
                    self.source, self.account_id
                ),
            )
        })
    }

    fn redact(&self, text: &str) -> String {
        match &self.credential {
            Some(credential) => text.replace(credential.expose(), "<redacted>"),
            None => text.to_owned(),
        }
    }

    fn account_path(&self, path: &str) -> String {
        format!("{}/accounts/{}{path}", self.base, self.account_id)
    }

    async fn call(
        &self,
        operation: &str,
        method: Method,
        path: &str,
        body: Option<(String, Vec<u8>)>,
        failure: ProviderErrorKind,
    ) -> Result<(StatusCode, Value), ProviderError> {
        let credential = self.credential()?;
        let mut request = self
            .http
            .request(method, self.account_path(path))
            .bearer_auth(credential.expose())
            .timeout(API_TIMEOUT);
        if let Some((content_type, bytes)) = body {
            request = request.header("content-type", content_type).body(bytes);
        }
        let response = request
            .send()
            .await
            .map_err(|error| self.transport(operation, error))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| self.transport(operation, error))?;
        let envelope = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
        let success = envelope.get("success").and_then(Value::as_bool);
        if status.is_success() && success != Some(false) {
            return Ok((status, envelope));
        }
        Err(self.api_error(operation, status, &envelope, &text, failure))
    }

    fn transport(&self, operation: &str, error: reqwest::Error) -> ProviderError {
        let reason = if error.is_timeout() {
            "timed out"
        } else if error.is_connect() {
            "could not connect"
        } else {
            "transport failed"
        };
        ProviderError::new(
            if error.is_timeout() || error.is_connect() {
                ProviderErrorKind::ProviderUnavailable
            } else {
                ProviderErrorKind::TransportFailure
            },
            self.redact(&format!(
                "cloudflare {operation}: {reason}: {}",
                error.without_url()
            )),
        )
    }

    fn api_error(
        &self,
        operation: &str,
        status: StatusCode,
        envelope: &Value,
        text: &str,
        failure: ProviderErrorKind,
    ) -> ProviderError {
        let errors = envelope
            .get("errors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let codes = errors
            .iter()
            .filter_map(|error| error.get("code").and_then(Value::as_u64))
            .collect::<Vec<_>>();
        let mut details = errors
            .iter()
            .map(|error| {
                let code = error.get("code").and_then(Value::as_u64);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)");
                match code {
                    Some(code) => format!("code {code}: {message}"),
                    None => message.to_owned(),
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        if details.is_empty() {
            details = text.chars().take(200).collect();
        }
        let lower = details.to_ascii_lowercase();
        let (kind, hint) = if matches!(status.as_u16(), 401 | 403)
            || codes
                .iter()
                .any(|code| matches!(code, 9106 | 9109 | 10000 | 10001))
        {
            (
                ProviderErrorKind::Unauthorized,
                format!(
                    "Cloudflare refused the API token from {} for account {}; it needs Workers Scripts Edit (and Read) on that account",
                    self.source, self.account_id
                ),
            )
        } else if codes.iter().any(|code| matches!(code, 7000 | 7003)) {
            (
                ProviderErrorKind::Unauthorized,
                format!(
                    "Cloudflare does not know account {} or the token cannot reach it; check account_id",
                    self.account_id
                ),
            )
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            (
                ProviderErrorKind::QuotaExhausted,
                "Cloudflare's API rate limit is exhausted; try again later".to_owned(),
            )
        } else if lower.contains("exceeded") && lower.contains("limit") {
            (
                ProviderErrorKind::QuotaExhausted,
                "a Cloudflare plan limit is exhausted".to_owned(),
            )
        } else if status.is_server_error() {
            (
                ProviderErrorKind::ProviderUnavailable,
                "Cloudflare's API is unavailable".to_owned(),
            )
        } else {
            (failure, format!("Cloudflare refused the {operation}"))
        };
        ProviderError::new(
            kind,
            self.redact(&format!(
                "cloudflare {operation}: {hint} (HTTP {}; {details})",
                status.as_u16()
            )),
        )
    }

    /// The account's workers.dev subdomain. Also the cheapest proof that
    /// the token is valid for the account.
    pub async fn subdomain(&self) -> Result<String, ProviderError> {
        let (_, envelope) = self
            .call(
                "subdomain lookup",
                Method::GET,
                "/workers/subdomain",
                None,
                ProviderErrorKind::ProviderUnavailable,
            )
            .await?;
        envelope
            .pointer("/result/subdomain")
            .and_then(Value::as_str)
            .filter(|subdomain| !subdomain.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::OperationUnsupported,
                    format!(
                        "Cloudflare account {} has no workers.dev subdomain; choose one under Workers & Pages in the Cloudflare dashboard",
                        self.account_id
                    ),
                )
            })
    }

    /// A Worker's settings (its bindings), or `None` when it does not exist.
    pub async fn script_settings(&self, worker: &str) -> Result<Option<Value>, ProviderError> {
        match self
            .call(
                "deployment status",
                Method::GET,
                &format!("/workers/scripts/{worker}/settings"),
                None,
                ProviderErrorKind::RemoteExecutionFailure,
            )
            .await
        {
            Ok((_, envelope)) => Ok(Some(envelope.get("result").cloned().unwrap_or(Value::Null))),
            Err(error) if error.message.contains("HTTP 404") => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn upload(
        &self,
        worker: &str,
        content_type: String,
        body: Vec<u8>,
    ) -> Result<Value, ProviderError> {
        let (_, envelope) = self
            .call(
                "deployment",
                Method::PUT,
                &format!("/workers/scripts/{worker}"),
                Some((content_type, body)),
                ProviderErrorKind::ArtifactInvalid,
            )
            .await?;
        Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn enable_workers_dev(&self, worker: &str) -> Result<(), ProviderError> {
        self.call(
            "workers.dev route",
            Method::POST,
            &format!("/workers/scripts/{worker}/subdomain"),
            Some((
                "application/json".into(),
                br#"{"enabled":true,"previews_enabled":false}"#.to_vec(),
            )),
            ProviderErrorKind::RemoteExecutionFailure,
        )
        .await
        .map(|_| ())
    }

    /// Delete a Worker. `false` when it was already absent.
    pub async fn delete(&self, worker: &str) -> Result<bool, ProviderError> {
        match self
            .call(
                "deletion",
                Method::DELETE,
                &format!("/workers/scripts/{worker}?force=true"),
                None,
                ProviderErrorKind::RemoteExecutionFailure,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(error) if error.message.contains("HTTP 404") => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Every Worker on the account: `(name, modified_on)`.
    pub async fn list(&self) -> Result<Vec<(String, Option<String>)>, ProviderError> {
        let (_, envelope) = self
            .call(
                "listing",
                Method::GET,
                "/workers/scripts",
                None,
                ProviderErrorKind::RemoteExecutionFailure,
            )
            .await?;
        Ok(envelope
            .get("result")
            .and_then(Value::as_array)
            .map(|scripts| {
                scripts
                    .iter()
                    .filter_map(|script| {
                        let name = script.get("id").and_then(Value::as_str)?;
                        Some((
                            name.to_owned(),
                            script
                                .get("modified_on")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// POST one invocation to a Worker. Returns whatever came back; the
    /// caller decides what it means. Transport failures are errors.
    pub async fn invoke(
        &self,
        url: &str,
        token: &str,
        body: Vec<u8>,
    ) -> Result<InvocationResponse, ProviderError> {
        let response = self
            .http
            .post(url)
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::ProviderUnavailable,
                    self.redact(&format!(
                        "cloudflare invocation: the request did not complete, so nothing is known about its outcome: {}",
                        error.without_url()
                    )),
                )
            })?;
        let status = response.status().as_u16();
        let cf_ray = response
            .headers()
            .get("cf-ray")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let text = response.text().await.map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::ProviderInterrupted,
                format!(
                    "cloudflare invocation: the response was cut off, so its outcome is unknown: {}",
                    error.without_url()
                ),
            )
        })?;
        let json = serde_json::from_str::<Value>(&text)
            .ok()
            .filter(|value| value.get("protocol").is_some() || value.get("error").is_some());
        Ok(InvocationResponse {
            status,
            cf_ray,
            json,
            text: text.chars().take(2000).collect(),
        })
    }
}
