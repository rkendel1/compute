//! The agent service the operator runs beside this controller, reached from
//! the Work page.
//!
//! Compute is agent-neutral ([docs/agents.md](../../../docs/agents.md)): the
//! agent (stack-agent) is a host service the operator starts with
//! `compute-configured-stack-agent serve`, and it owns every work record, its
//! events and what they mean. This module only carries a request to that
//! service and its answer back, so a browser, which cannot reach a loopback
//! port or hold the service's trust, can submit a goal, watch it and cancel it.
//!
//! Nothing is stored here and nothing is cached: each call reads the service.
//! The service has no authentication of its own, so it is only ever reached
//! on loopback, and no Compute credential or header is forwarded to it.

use std::time::Duration;

use serde_json::{Value, json};

use crate::EnvironmentError;

/// Where the operator says the service listens.
pub const URL_ENV: &str = "COMPUTE_STACK_AGENT_URL";
/// The port `docs/stack-agent.md` starts the service on.
const DEFAULT_URL: &str = "http://127.0.0.1:8765";
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BYTES: usize = 1024 * 1024;

/// What the service answered: its own status code and body, unchanged.
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The registered service's name and the capability it declares: the same
/// identifiers the distribution profile gives the agent runtime.
pub const SERVICE: &str = "stack-agent";
pub const CAPABILITY: &str = "Agent/loop@1";

/// Where an address came from, in the order it wins.
fn resolve(registered: Option<&str>) -> (String, &'static str) {
    if let Some(url) = registered {
        return (url.to_owned(), "registered");
    }
    match std::env::var(URL_ENV) {
        Ok(url) => (url, "environment"),
        Err(_) => (DEFAULT_URL.to_owned(), "default"),
    }
}

/// An address the service may be reached at: plain http on loopback, because
/// the service has no authentication of its own.
pub fn validate(text: &str) -> Result<reqwest::Url, EnvironmentError> {
    let url = reqwest::Url::parse(text.trim()).map_err(|_| {
        EnvironmentError::Invalid(
            "the agent address is not a URL, e.g. http://127.0.0.1:8765".into(),
        )
    })?;
    let loopback = url.host_str().is_some_and(|host| {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if url.scheme() != "http"
        || !loopback
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(EnvironmentError::Invalid(
            "the agent address must be a plain http loopback URL (127.0.0.1 or localhost): the agent service has no authentication of its own"
                .into(),
        ));
    }
    Ok(url)
}

async fn call(
    registered: Option<&str>,
    method: reqwest::Method,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<Answer, EnvironmentError> {
    let mut url = validate(&resolve(registered).0)?;
    let prefix = url.path().trim_end_matches('/').to_owned();
    url.set_path(&format!("{prefix}{path}"));
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(concat!("compute-environment/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| EnvironmentError::RuntimeUnavailable(error.to_string()))?;
    let mut request = client.request(method, url.clone());
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body);
    }
    let response = request.send().await.map_err(|error| {
        EnvironmentError::RuntimeUnavailable(format!(
            "the agent service at {url} did not answer ({}). Start it with \
             `compute-configured-stack-agent serve --port {}`, or set its address here.",
            if error.is_timeout() {
                "timed out"
            } else {
                "not running"
            },
            url.port_or_known_default().unwrap_or(8765),
        ))
    })?;
    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| EnvironmentError::RuntimeUnavailable(error.to_string()))?;
    if bytes.len() > MAX_BYTES {
        return Err(EnvironmentError::RuntimeUnavailable(
            "the agent service answered with more than Compute will relay".into(),
        ));
    }
    Ok(Answer {
        status,
        body: bytes.to_vec(),
    })
}

impl Answer {
    /// A success passes through unchanged. A refusal becomes Compute's own
    /// error shape carrying the service's reason, so every caller reads it
    /// the way it reads any other Compute error.
    pub fn checked(self) -> Result<Self, EnvironmentError> {
        if self.status < 400 {
            return Ok(self);
        }
        let body = serde_json::from_slice::<Value>(&self.body).ok();
        let reason = body
            .as_ref()
            .and_then(|body| {
                ["message", "error"]
                    .iter()
                    .find_map(|key| body[key].as_str().map(str::to_owned))
            })
            .unwrap_or_else(|| format!("the agent service answered {}", self.status));
        Err(match self.status {
            404 => EnvironmentError::NotFound(reason),
            409 => EnvironmentError::Conflict(reason),
            400..=499 => EnvironmentError::Invalid(reason),
            _ => EnvironmentError::RuntimeUnavailable(reason),
        })
    }
}

/// Whether something answers `GET /health` at this address.
pub async fn status_of(base: &str) -> bool {
    matches!(
        call(Some(base), reqwest::Method::GET, "/health", None).await,
        Ok(answer) if answer.status < 500
    )
}

/// Whether an agent service is reachable, and where its address came from.
/// Never fails: an unreachable service is the answer, with the reason and what
/// to do about it.
pub async fn status(registered: Option<&str>) -> Value {
    status_from(registered, "registered").await
}

/// As [`status`], naming the source of an address the caller resolved.
pub async fn status_from(address: Option<&str>, given: &'static str) -> Value {
    let registered = address;
    let (url, mut source) = resolve(registered);
    if registered.is_some() {
        source = given;
    }
    // `GET /health` is in the route list stack-agent's profile declares. Any
    // answer below 500 means a service is listening.
    match call(registered, reqwest::Method::GET, "/health", None).await {
        Ok(answer) if answer.status < 500 => {
            json!({ "url": url, "source": source, "reachable": true })
        }
        Ok(answer) => {
            json!({ "url": url, "source": source, "reachable": false, "error": format!("the agent service answered {}", answer.status) })
        }
        Err(error) => {
            json!({ "url": url, "source": source, "reachable": false, "error": error.message() })
        }
    }
}

/// Submit a goal. Only `goal` (and an optional `kind`) is passed on.
pub async fn submit(registered: Option<&str>, request: Value) -> Result<Answer, EnvironmentError> {
    let goal = request["goal"]
        .as_str()
        .map(str::trim)
        .filter(|goal| !goal.is_empty())
        .ok_or_else(|| EnvironmentError::Invalid("a goal is required".into()))?;
    let mut forwarded = json!({ "goal": goal });
    if let Some(kind) = request["kind"].as_str() {
        forwarded["kind"] = json!(kind);
    }
    call(
        registered,
        reqwest::Method::POST,
        "/v1/work",
        Some(serde_json::to_vec(&forwarded)?),
    )
    .await
}

fn work_path(work: &str, suffix: &str) -> Result<String, EnvironmentError> {
    if work.is_empty()
        || !work
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(EnvironmentError::Invalid("not a work identifier".into()));
    }
    Ok(format!("/v1/work/{work}{suffix}"))
}

pub async fn work(registered: Option<&str>, work: &str) -> Result<Answer, EnvironmentError> {
    call(
        registered,
        reqwest::Method::GET,
        &work_path(work, "")?,
        None,
    )
    .await
}

pub async fn events(registered: Option<&str>, work: &str) -> Result<Answer, EnvironmentError> {
    call(
        registered,
        reqwest::Method::GET,
        &work_path(work, "/events")?,
        None,
    )
    .await
}

pub async fn cancel(registered: Option<&str>, work: &str) -> Result<Answer, EnvironmentError> {
    call(
        registered,
        reqwest::Method::POST,
        &work_path(work, "/cancel")?,
        None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_work_identifier_reaches_the_service() {
        assert!(work_path("work_1-a", "/events").is_ok());
        for bad in ["", "../x", "a/b", "a?b", "a b"] {
            assert!(work_path(bad, "").is_err(), "{bad}");
        }
    }
}
