//! The node's public entry: HTTP and HTTPS.
//!
//! ```text
//! :80   /.well-known/acme-challenge/<token>  → the key authorization
//!       Host with a certificate               → 308 to https
//!       Host with a route                     → its endpoint
//! :443  SNI with a certificate and a route    → TLS terminated → its endpoint
//! ```
//!
//! A host routes only where its domain record says; an unknown host gets
//! nothing. Ingress never decides routing: the daemon sets it.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::endpoints::connect_local;
use crate::tls::{TlsError, certified_key};

const HEAD_LIMIT: usize = 16 * 1024;

/// Where a domain's traffic goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressRoute {
    /// The endpoint's host port on this node.
    pub endpoint_port: u16,
}

#[derive(Default)]
struct Table {
    routes: RwLock<BTreeMap<String, IngressRoute>>,
    challenges: RwLock<BTreeMap<String, String>>,
    certificates: RwLock<BTreeMap<String, Arc<CertifiedKey>>>,
}

#[derive(Debug)]
struct Resolver(Arc<Table>);

impl std::fmt::Debug for Table {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ingress table")
    }
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_ascii_lowercase();
        self.0.certificates.read().ok()?.get(&name).cloned()
    }
}

pub struct Ingress {
    table: Arc<Table>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Default for Ingress {
    fn default() -> Self {
        Self::new()
    }
}

impl Ingress {
    pub fn new() -> Self {
        Self {
            table: Arc::default(),
            tasks: Mutex::default(),
        }
    }

    /// Serve HTTP on `address`. Returns the bound address.
    pub async fn listen_http(&self, address: SocketAddr) -> io::Result<SocketAddr> {
        let listener = TcpListener::bind(address).await?;
        let bound = listener.local_addr()?;
        let table = self.table.clone();
        self.tasks
            .lock()
            .expect("tasks")
            .push(tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    };
                    let table = table.clone();
                    tokio::spawn(async move {
                        let _ = http(stream, &table).await;
                    });
                }
            }));
        Ok(bound)
    }

    /// Serve HTTPS on `address`, terminating TLS with the certificate of
    /// the requested name. Returns the bound address.
    pub async fn listen_https(&self, address: SocketAddr) -> io::Result<SocketAddr> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Resolver(self.table.clone())));
        let mut config = config;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(address).await?;
        let bound = listener.local_addr()?;
        let table = self.table.clone();
        self.tasks
            .lock()
            .expect("tasks")
            .push(tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    };
                    let acceptor = acceptor.clone();
                    let table = table.clone();
                    tokio::spawn(async move {
                        let _ = https(stream, acceptor, &table).await;
                    });
                }
            }));
        Ok(bound)
    }

    /// Replace the routing table: domain → endpoint.
    pub fn set_routes(&self, routes: BTreeMap<String, IngressRoute>) {
        let routes = routes
            .into_iter()
            .map(|(domain, route)| (domain.to_ascii_lowercase(), route))
            .collect();
        *self.table.routes.write().expect("routes") = routes;
    }

    pub fn route(&self, domain: &str) -> Option<IngressRoute> {
        self.table
            .routes
            .read()
            .expect("routes")
            .get(&domain.to_ascii_lowercase())
            .cloned()
    }

    /// Serve a certificate for `domain`.
    pub fn set_certificate(
        &self,
        domain: &str,
        chain_pem: &[u8],
        key_pem: &[u8],
    ) -> Result<(), TlsError> {
        let key = certified_key(chain_pem, key_pem)?;
        self.table
            .certificates
            .write()
            .expect("certificates")
            .insert(domain.to_ascii_lowercase(), key);
        Ok(())
    }

    pub fn remove_certificate(&self, domain: &str) {
        self.table
            .certificates
            .write()
            .expect("certificates")
            .remove(&domain.to_ascii_lowercase());
    }

    /// The fingerprint of the certificate served for `domain`.
    pub fn certificate(&self, domain: &str) -> Option<String> {
        self.table
            .certificates
            .read()
            .expect("certificates")
            .get(&domain.to_ascii_lowercase())
            .and_then(|key| key.cert.first().map(|leaf| crate::tls::fingerprint(leaf)))
    }
}

impl crate::acme::ChallengeResponder for Ingress {
    fn present(&self, token: &str, key_authorization: &str) {
        self.table
            .challenges
            .write()
            .expect("challenges")
            .insert(token.into(), key_authorization.into());
    }

    fn clear(&self, token: &str) {
        self.table
            .challenges
            .write()
            .expect("challenges")
            .remove(token);
    }
}

impl Drop for Ingress {
    fn drop(&mut self) {
        for task in self.tasks.lock().expect("tasks").iter() {
            task.abort();
        }
    }
}

/// The request head: bytes read so far, the path, and the host.
async fn read_head(stream: &mut TcpStream) -> io::Result<(Vec<u8>, String, Option<String>)> {
    let mut head = Vec::with_capacity(1024);
    let mut buffer = [0u8; 2048];
    let read = async {
        loop {
            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                return Ok(());
            }
            if head.len() > HEAD_LIMIT {
                return Err(io::Error::other("request head too large"));
            }
            let count = stream.read(&mut buffer).await?;
            if count == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            head.extend_from_slice(&buffer[..count]);
        }
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.split("\r\n");
    let request = lines.next().unwrap_or_default();
    let path = request.split(' ').nth(1).unwrap_or("/").to_string();
    let host = lines
        .take_while(|line| !line.is_empty())
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("host")
                .then(|| value.trim().to_string())
        })
        .map(|host| strip_port(&host).to_ascii_lowercase());
    Ok((head, path, host))
}

fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host;
    }
    host.rsplit_once(':').map_or(host, |(name, _)| name)
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    headers: &str,
    body: &str,
) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\ncontent-type: text/plain\r\nconnection: close\r\n{headers}\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

async fn http(mut stream: TcpStream, table: &Table) -> io::Result<()> {
    let (head, path, host) = read_head(&mut stream).await?;
    if let Some(token) = path.strip_prefix("/.well-known/acme-challenge/") {
        let answer = table
            .challenges
            .read()
            .expect("challenges")
            .get(token)
            .cloned();
        return match answer {
            Some(answer) => respond(&mut stream, "200 OK", "", &answer).await,
            None => respond(&mut stream, "404 Not Found", "", "no such challenge\n").await,
        };
    }
    let Some(host) = host else {
        return respond(
            &mut stream,
            "400 Bad Request",
            "",
            "a Host header is required\n",
        )
        .await;
    };
    let route = table.routes.read().expect("routes").get(&host).cloned();
    let Some(route) = route else {
        return respond(&mut stream, "404 Not Found", "", "no route for this host\n").await;
    };
    let secured = table
        .certificates
        .read()
        .expect("certificates")
        .contains_key(&host);
    if secured {
        let location = format!("location: https://{host}{path}\r\n");
        return respond(&mut stream, "308 Permanent Redirect", &location, "").await;
    }
    let mut upstream = connect_local(route.endpoint_port).await?;
    upstream.write_all(&head).await?;
    tokio::io::copy_bidirectional(&mut stream, &mut upstream).await?;
    Ok(())
}

async fn https(
    stream: TcpStream,
    acceptor: tokio_rustls::TlsAcceptor,
    table: &Table,
) -> io::Result<()> {
    let mut tls = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    let name = tls
        .get_ref()
        .1
        .server_name()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let route = table.routes.read().expect("routes").get(&name).cloned();
    let Some(route) = route else {
        tls.shutdown().await?;
        return Ok(());
    };
    let mut upstream = connect_local(route.endpoint_port).await?;
    tokio::io::copy_bidirectional(&mut tls, &mut upstream).await?;
    Ok(())
}
