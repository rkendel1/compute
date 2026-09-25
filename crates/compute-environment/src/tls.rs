//! TLS for the Compute API: a configured certificate and key, reloaded
//! from disk when either file changes, without restarting the controller
//! or any workload.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::EnvironmentError;

/// How often a handshake may check the files for a new certificate.
const RELOAD_CHECK: Duration = Duration::from_secs(1);

/// What `/info` and `compute doctor` say about the API's TLS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsStatus {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded_at: Option<DateTime<Utc>>,
    pub reloads: u64,
    /// The last reload that failed; the previous certificate kept serving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl TlsStatus {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            certificate: None,
            fingerprint: None,
            not_after: None,
            names: vec![],
            loaded_at: None,
            reloads: 0,
            last_error: None,
        }
    }
}

struct Loaded {
    key: Arc<CertifiedKey>,
    modified: (Option<SystemTime>, Option<SystemTime>),
    checked: Instant,
    status: TlsStatus,
}

struct Reloading {
    certificate: PathBuf,
    key: PathBuf,
    loaded: Mutex<Loaded>,
}

impl std::fmt::Debug for Reloading {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reloading")
            .field("certificate", &self.certificate)
            .finish_non_exhaustive()
    }
}

fn modified(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn load(certificate: &PathBuf, key: &PathBuf) -> Result<(Arc<CertifiedKey>, TlsStatus), String> {
    let chain = std::fs::read(certificate)
        .map_err(|error| format!("{}: {error}", certificate.display()))?;
    let key_pem = std::fs::read(key).map_err(|error| format!("{}: {error}", key.display()))?;
    let certified =
        compute_network::tls::certified_key(&chain, &key_pem).map_err(|error| error.to_string())?;
    let info = compute_network::tls::info(&chain).map_err(|error| error.to_string())?;
    if info.not_after <= Utc::now() {
        return Err(format!(
            "the certificate in {} expired at {}",
            certificate.display(),
            info.not_after
        ));
    }
    Ok((
        certified,
        TlsStatus {
            enabled: true,
            certificate: Some(certificate.display().to_string()),
            fingerprint: Some(info.fingerprint),
            not_after: Some(info.not_after),
            names: info.names,
            loaded_at: Some(Utc::now()),
            reloads: 0,
            last_error: None,
        },
    ))
}

impl Reloading {
    fn refresh(&self, loaded: &mut Loaded) {
        if loaded.checked.elapsed() < RELOAD_CHECK {
            return;
        }
        loaded.checked = Instant::now();
        let now = (modified(&self.certificate), modified(&self.key));
        if now == loaded.modified {
            return;
        }
        match load(&self.certificate, &self.key) {
            Ok((key, mut status)) => {
                status.reloads = loaded.status.reloads + 1;
                loaded.key = key;
                loaded.status = status;
                loaded.modified = now;
            }
            // Keep serving the certificate that works; say why the new one
            // does not. A half-written pair is retried at the next check.
            Err(error) => loaded.status.last_error = Some(error),
        }
    }
}

impl ResolvesServerCert for Reloading {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let mut loaded = self.loaded.lock().ok()?;
        self.refresh(&mut loaded);
        Some(loaded.key.clone())
    }
}

/// The API's TLS acceptor.
pub struct ApiTls {
    acceptor: TlsAcceptor,
    resolver: Arc<Reloading>,
}

impl ApiTls {
    /// Load the certificate and key. Fails closed: a missing, unreadable,
    /// mismatched, or expired pair is an error, never plaintext.
    pub fn load(certificate: PathBuf, key: PathBuf) -> Result<Arc<Self>, EnvironmentError> {
        let (certified, status) = load(&certificate, &key).map_err(|error| {
            EnvironmentError::Invalid(format!("TLS is required and cannot be loaded: {error}"))
        })?;
        let resolver = Arc::new(Reloading {
            loaded: Mutex::new(Loaded {
                key: certified,
                modified: (modified(&certificate), modified(&key)),
                checked: Instant::now(),
                status,
            }),
            certificate,
            key,
        });
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|error| EnvironmentError::Invalid(error.to_string()))?
        .with_no_client_auth()
        .with_cert_resolver(resolver.clone());
        Ok(Arc::new(Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            resolver,
        }))
    }

    pub async fn accept(&self, stream: TcpStream) -> std::io::Result<TlsStream<TcpStream>> {
        tokio::time::timeout(Duration::from_secs(10), self.acceptor.accept(stream))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "TLS handshake"))?
    }

    pub fn status(&self) -> TlsStatus {
        let mut loaded = self.resolver.loaded.lock().expect("tls");
        self.resolver.refresh(&mut loaded);
        loaded.status.clone()
    }
}
