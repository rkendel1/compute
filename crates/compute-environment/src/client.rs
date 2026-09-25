//! A client of the Compute API. The CLI uses it; so can anything else.

use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::EnvironmentError;

pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8787";

#[derive(Clone)]
pub struct DaemonClient {
    host: String,
    port: u16,
    authorization: Option<String>,
    /// For `https://` endpoints: whom to trust.
    tls: Option<Arc<rustls::ClientConfig>>,
}

impl std::fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the authorization header.
        f.debug_struct("DaemonClient")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls", &self.tls.is_some())
            .finish_non_exhaustive()
    }
}

trait Connection: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Connection for T {}

/// A readable stream from the daemon.
pub trait AsyncReadSend: AsyncRead + Unpin + Send {}
impl<T: AsyncRead + Unpin + Send> AsyncReadSend for T {}

/// Trust for `https://` endpoints: the PEM certificates in
/// `$COMPUTE_CA_CERT` when it is set (a private CA or a self-signed
/// certificate), otherwise the public web PKI.
fn client_tls() -> Result<Arc<rustls::ClientConfig>, EnvironmentError> {
    match std::env::var_os("COMPUTE_CA_CERT") {
        Some(path) => {
            let pem = std::fs::read(&path).map_err(|error| {
                EnvironmentError::Invalid(format!(
                    "COMPUTE_CA_CERT {}: {error}",
                    std::path::Path::new(&path).display()
                ))
            })?;
            trusting(Some(&pem))
        }
        None => trusting(None),
    }
}

/// A client configuration trusting these PEM certificates, or the public
/// web PKI.
fn trusting(pem: Option<&[u8]>) -> Result<Arc<rustls::ClientConfig>, EnvironmentError> {
    let mut roots = rustls::RootCertStore::empty();
    match pem {
        Some(pem) => {
            for certificate in compute_network::tls::certificates(pem).map_err(|error| {
                EnvironmentError::Invalid(format!("trusted certificate: {error}"))
            })? {
                roots.add(certificate).map_err(|error| {
                    EnvironmentError::Invalid(format!("trusted certificate: {error}"))
                })?;
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| EnvironmentError::Invalid(error.to_string()))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

impl DaemonClient {
    /// `endpoint` is `http://host:port` or `https://host:port`.
    pub fn new(endpoint: &str) -> Result<Self, EnvironmentError> {
        let (rest, tls) = if let Some(rest) = endpoint.strip_prefix("https://") {
            (rest, Some(client_tls()?))
        } else if let Some(rest) = endpoint.strip_prefix("http://") {
            (rest, None)
        } else {
            return Err(EnvironmentError::Invalid(
                "the daemon endpoint must be http://host:port or https://host:port".into(),
            ));
        };
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
            tls,
        })
    }

    /// Trust exactly these PEM certificates for an `https://` endpoint.
    pub fn trusting_pem(mut self, pem: &[u8]) -> Result<Self, EnvironmentError> {
        if self.tls.is_some() {
            self.tls = Some(trusting(Some(pem))?);
        }
        Ok(self)
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

    async fn connect(&self) -> Result<Box<dyn Connection>, EnvironmentError> {
        let tcp = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|error| {
                EnvironmentError::ControllerUnavailable(format!(
                    "cannot reach the Compute daemon at {}:{} ({error}); start it with `compute start`",
                    self.host, self.port
                ))
            })?;
        let stream: Box<dyn Connection> = match &self.tls {
            Some(config) => {
                let name = rustls::pki_types::ServerName::try_from(self.host.clone()).map_err(
                    |error| EnvironmentError::Invalid(format!("{}: {error}", self.host)),
                )?;
                let connector = tokio_rustls::TlsConnector::from(config.clone());
                Box::new(connector.connect(name, tcp).await.map_err(|error| {
                    EnvironmentError::ControllerUnavailable(format!(
                        "TLS with the Compute daemon at {}:{} failed: {error}",
                        self.host, self.port
                    ))
                })?)
            }
            None => Box::new(tcp),
        };
        Ok(stream)
    }

    /// Open a streaming GET, such as `/events/stream`, and read it line by
    /// line: the response headers, then the body as it arrives.
    pub async fn stream_lines(
        &self,
        path: &str,
    ) -> Result<tokio::io::Lines<tokio::io::BufReader<Box<dyn AsyncReadSend>>>, EnvironmentError>
    {
        use tokio::io::AsyncBufReadExt;
        let mut stream = self.connect().await?;
        let authorization = self
            .authorization
            .as_ref()
            .map(|value| format!("Authorization: {value}\r\n"))
            .unwrap_or_default();
        stream
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n{authorization}\r\n",
                    self.host
                )
                .as_bytes(),
            )
            .await?;
        stream.flush().await?;
        let reader: Box<dyn AsyncReadSend> = Box::new(stream);
        Ok(tokio::io::BufReader::new(reader).lines())
    }

    /// A GET whose response body is returned exactly as the daemon sent
    /// it, such as a canonical receipt.
    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>, EnvironmentError> {
        self.send_raw::<()>("GET", path, None).await
    }

    async fn send<B: Serialize, T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, EnvironmentError> {
        let body = self.send_raw(method, path, body).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    async fn send_raw<B: Serialize>(
        &self,
        method: &str,
        path: &str,
        body: Option<&B>,
    ) -> Result<Vec<u8>, EnvironmentError> {
        let payload = match body {
            Some(value) => serde_json::to_vec(value)?,
            None => vec![],
        };
        let mut stream = self.connect().await?;
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
        stream.flush().await?;
        let mut response = Vec::new();
        match stream.read_to_end(&mut response).await {
            Ok(_) => {}
            // A TLS peer that closes without close_notify after a complete
            // response is still a complete response.
            Err(error)
                if error.kind() == std::io::ErrorKind::UnexpectedEof && !response.is_empty() => {}
            Err(error) => return Err(error.into()),
        }
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
                Some("unauthorized" | "authentication_failed") => {
                    EnvironmentError::Unauthorized(message)
                }
                Some("authorization_denied") => EnvironmentError::Forbidden(message),
                Some("runtime_unavailable") => EnvironmentError::RuntimeUnavailable(message),
                Some("cancelled") => EnvironmentError::Cancelled(message),
                Some("controller_unavailable") => EnvironmentError::ControllerUnavailable(message),
                Some("upgrade_failed") => EnvironmentError::UpgradeFailed(message),
                _ => EnvironmentError::Invalid(message),
            });
        }
        Ok(body.to_vec())
    }
}
