//! A client of the Compute API. The CLI uses it; so can anything else.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::EnvironmentError;

pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8787";

#[derive(Debug, Clone)]
pub struct DaemonClient {
    host: String,
    port: u16,
    authorization: Option<String>,
}

impl DaemonClient {
    /// `endpoint` is `http://host:port`.
    pub fn new(endpoint: &str) -> Result<Self, EnvironmentError> {
        let rest = endpoint.strip_prefix("http://").ok_or_else(|| {
            EnvironmentError::Invalid("the daemon endpoint must be http://host:port".into())
        })?;
        let authority = rest.split('/').next().unwrap_or_default();
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or_else(|| EnvironmentError::Invalid("the daemon endpoint needs a port".into()))?;
        Ok(Self {
            host: host.to_string(),
            port: port
                .parse()
                .map_err(|_| EnvironmentError::Invalid("invalid daemon port".into()))?,
            authorization: None,
        })
    }

    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.authorization = Some(format!("Bearer {}", token.into()));
        self
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, EnvironmentError> {
        self.send::<(), T>("GET", path, None).await
    }

    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, EnvironmentError> {
        self.send("POST", path, body).await
    }

    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T, EnvironmentError> {
        self.send::<(), T>("DELETE", path, None).await
    }

    async fn send<B: Serialize, T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, EnvironmentError> {
        let payload = match body {
            Some(value) => serde_json::to_vec(value)?,
            None => vec![],
        };
        let mut stream = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|error| {
                EnvironmentError::Io(std::io::Error::new(
                    error.kind(),
                    format!(
                        "cannot reach the Compute daemon at {}:{} ({error}); start it with `compute start`",
                        self.host, self.port
                    ),
                ))
            })?;
        let authorization = self
            .authorization
            .as_ref()
            .map(|value| format!("Authorization: {value}\r\n"))
            .unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json\r\n{authorization}Content-Length: {}\r\n\r\n",
            self.host,
            payload.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&payload).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        let header_end = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or_else(|| EnvironmentError::Invalid("malformed daemon response".into()))?;
        let status = String::from_utf8_lossy(&response[..header_end])
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or_else(|| EnvironmentError::Invalid("malformed daemon status".into()))?;
        let body = &response[header_end + 4..];
        if !(200..300).contains(&status) {
            let value: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            let message = value["message"]
                .as_str()
                .unwrap_or("daemon request failed")
                .to_string();
            return Err(match value["kind"].as_str() {
                Some("not_found") => EnvironmentError::NotFound(message),
                Some("no_route") => EnvironmentError::NoRoute(message),
                Some("state_unavailable") => EnvironmentError::Unavailable(message),
                Some("conflict") => EnvironmentError::Conflict(message),
                Some("admission_denied") => EnvironmentError::Denied(message),
                Some("unauthorized") => EnvironmentError::Unauthorized(message),
                _ => EnvironmentError::Invalid(message),
            });
        }
        Ok(serde_json::from_slice(body)?)
    }
}
