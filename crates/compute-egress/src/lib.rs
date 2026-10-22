//! The egress gateway: the one way out of a session that is allowed to
//! reach the network, and the place where credentials are added so that
//! they never enter the session.
//!
//! ```text
//!   session ──HTTPS_PROXY──▶ gateway ──allowlist──▶ DNS (pinned) ──▶ upstream
//!                               │
//!                               └─ bound host: TLS terminated with a leaf from
//!                                  this gateway's own CA (name-constrained to
//!                                  the bound hosts), credential header set,
//!                                  re-encrypted to the upstream it resolved
//! ```
//!
//! **Allowlisted.** A destination is reached only when a rule names its host
//! (exactly, or `*.suffix` for subdomains) and port. Everything else is
//! refused with `403`, and every decision is recorded.
//!
//! **Pinned.** The gateway resolves a host once per connection and connects
//! to the address it checked, never to one the session names. An address
//! that is private, loopback, link-local, or otherwise not public is refused
//! unless the policy allows private destinations, so an allowed name cannot
//! be pointed at the node's own services.
//!
//! **Brokered.** A [`CredentialBinding`] names a host, a header, and a
//! value. For that host only, the gateway terminates the session's TLS with
//! a certificate from its own CA, checks the request's `Host` is that host,
//! replaces the header (a value the session sent is discarded), forces one
//! request per connection, and forwards over TLS it verifies itself. The
//! value is never written anywhere the session can read. Other allowed hosts
//! are tunnelled untouched. Plain HTTP is forwarded, but a credential is
//! never sent in cleartext.
//!
//! **Fails closed.** A malformed request, an unresolvable host, an upstream
//! that fails verification, or anything the gateway does not understand
//! ends the connection; nothing is forwarded on a guess.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, GeneralSubtree, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, NameConstraints,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The largest request head the gateway reads.
const MAX_HEAD: usize = 32 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    #[error("invalid egress configuration: {0}")]
    Invalid(String),
    #[error("egress gateway: {0}")]
    Io(#[from] std::io::Error),
}

/// Where a session may connect.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressPolicy {
    #[serde(default)]
    pub allow: Vec<HostRule>,
    /// Whether an allowed name may resolve to a private, loopback, or
    /// link-local address. Off unless a policy needs it.
    #[serde(default)]
    pub allow_private: bool,
}

/// `host` is a name (`api.example.com`), `*.example.com` for any subdomain
/// (not the apex), or an IP address. `ports` defaults to 443.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostRule {
    pub host: String,
    #[serde(default)]
    pub ports: Vec<u16>,
}

impl HostRule {
    fn matches(&self, host: &str, port: u16) -> bool {
        let ports_match = if self.ports.is_empty() {
            port == 443
        } else {
            self.ports.contains(&port)
        };
        let pattern = self.host.to_ascii_lowercase();
        let host_match = match pattern.strip_prefix("*.") {
            Some(suffix) => host
                .strip_suffix(suffix)
                .is_some_and(|label| label.len() > 1 && label.ends_with('.')),
            None => host == pattern,
        };
        ports_match && host_match
    }
}

/// A credential the gateway adds to requests for one host. The value lives
/// in the gateway only.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialBinding {
    /// The exact host, as the session connects to it.
    pub host: String,
    /// The header that carries it, such as `authorization` or `x-api-key`.
    pub header: String,
    pub value: String,
}

impl std::fmt::Debug for CredentialBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialBinding")
            .field("host", &self.host)
            .field("header", &self.header)
            .field("value", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct GatewayConfig {
    pub policy: EgressPolicy,
    pub bindings: Vec<CredentialBinding>,
    /// Fixed answers for names, used instead of DNS (still checked against
    /// the policy's private-address rule).
    pub resolve: BTreeMap<String, Vec<SocketAddr>>,
    /// Roots the gateway trusts for upstreams it re-encrypts to. The public
    /// web roots when `None`.
    pub upstream_roots: Option<Vec<CertificateDer<'static>>>,
}

/// One connection's outcome, as the gateway decided it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub host: String,
    pub port: u16,
    pub allowed: bool,
    /// Whether a credential was added.
    pub brokered: bool,
    pub reason: String,
}

struct State {
    policy: EgressPolicy,
    bindings: HashMap<String, CredentialBinding>,
    resolve: BTreeMap<String, Vec<SocketAddr>>,
    issuer: Issuer<'static, KeyPair>,
    ca: CertificateDer<'static>,
    leaves: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
    upstream: Arc<rustls::ClientConfig>,
    decisions: Mutex<Vec<Decision>>,
}

/// A running gateway. Dropping it stops it.
pub struct Gateway {
    address: SocketAddr,
    ca_pem: String,
    state: Arc<State>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn invalid(what: impl std::fmt::Display) -> EgressError {
    EgressError::Invalid(what.to_string())
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

impl Gateway {
    /// Start a gateway on `listen` with its own, freshly made CA.
    pub async fn start(listen: SocketAddr, config: GatewayConfig) -> Result<Self, EgressError> {
        for rule in &config.policy.allow {
            let host = rule.host.strip_prefix("*.").unwrap_or(&rule.host);
            if !valid_host(host) && host.parse::<IpAddr>().is_err() {
                return Err(invalid(format!("unsupported host rule {:?}", rule.host)));
            }
        }
        let mut bindings = HashMap::new();
        for binding in config.bindings {
            let host = binding.host.to_ascii_lowercase();
            if !valid_host(&host) || host.parse::<IpAddr>().is_ok() {
                return Err(invalid(format!(
                    "a credential is bound to a host name, not {:?}",
                    binding.host
                )));
            }
            if binding.header.is_empty()
                || !binding
                    .header
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                || binding.value.contains(['\r', '\n'])
            {
                return Err(invalid(format!("invalid credential binding for {host}")));
            }
            if !config
                .policy
                .allow
                .iter()
                .any(|rule| rule.matches(&host, 443))
            {
                return Err(invalid(format!(
                    "a credential is bound to {host}, which the policy does not allow on 443"
                )));
            }
            if bindings.insert(host.clone(), binding).is_some() {
                return Err(invalid(format!("{host} has two credential bindings")));
            }
        }

        // The CA can only ever vouch for the bound hosts.
        let key = KeyPair::generate().map_err(invalid)?;
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(invalid)?;
        params
            .distinguished_name
            .push(DnType::CommonName, "Compute egress gateway");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let mut permitted = bindings
            .keys()
            .map(|host| GeneralSubtree::DnsName(host.clone()))
            .collect::<Vec<_>>();
        if permitted.is_empty() {
            permitted.push(GeneralSubtree::DnsName("invalid".into()));
        }
        params.name_constraints = Some(NameConstraints {
            permitted_subtrees: permitted,
            excluded_subtrees: vec![],
        });
        let certificate = params.self_signed(&key).map_err(invalid)?;
        let ca_pem = certificate.pem();
        let ca = certificate.der().clone();
        let issuer = Issuer::new(params, key);

        let mut roots = rustls::RootCertStore::empty();
        match config.upstream_roots {
            Some(certificates) => {
                for certificate in certificates {
                    roots.add(certificate).map_err(invalid)?;
                }
            }
            None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        let mut upstream = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(invalid)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        upstream.alpn_protocols = vec![b"http/1.1".to_vec()];

        let state = Arc::new(State {
            policy: config.policy,
            bindings,
            resolve: config
                .resolve
                .into_iter()
                .map(|(host, addresses)| (host.to_ascii_lowercase(), addresses))
                .collect(),
            issuer,
            ca,
            leaves: Mutex::new(HashMap::new()),
            upstream: Arc::new(upstream),
            decisions: Mutex::new(vec![]),
        });
        let listener = TcpListener::bind(listen).await?;
        let address = listener.local_addr()?;
        let serving = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = serving.clone();
                tokio::spawn(async move {
                    let _ = connection(stream, &state).await;
                });
            }
        });
        Ok(Self {
            address,
            ca_pem,
            state,
            task,
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The CA a session trusts so brokered hosts verify. It can sign only
    /// for the bound hosts.
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    pub fn ca_der(&self) -> CertificateDer<'static> {
        self.state.ca.clone()
    }

    /// Every decision so far, oldest first.
    pub fn decisions(&self) -> Vec<Decision> {
        self.state.decisions.lock().expect("decisions").clone()
    }
}

/// Whether an address is one the public internet routes to.
pub fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || first == 0x2001 && v6.segments()[1] == 0x0db8)
        }
    }
}

struct Head {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.split("\r\n");
        let mut request = lines.next()?.split(' ');
        let (method, target, version) = (request.next()?, request.next()?, request.next()?);
        if request.next().is_some() || !version.starts_with("HTTP/1.") {
            return None;
        }
        let mut headers = vec![];
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':')?;
            if name.is_empty() || name.contains(char::is_whitespace) {
                return None;
            }
            headers.push((name.to_owned(), value.trim().to_owned()));
        }
        Some(Self {
            method: method.to_owned(),
            target: target.to_owned(),
            version: version.to_owned(),
            headers,
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The head as forwarded: hop-by-hop and proxy headers gone, `remove`
    /// gone, `add` added, and one request per connection.
    fn forwarded(&self, target: &str, remove: Option<&str>, add: Option<(&str, &str)>) -> Vec<u8> {
        let mut out = format!("{} {target} {}\r\n", self.method, self.version);
        for (name, value) in &self.headers {
            let hop = [
                "connection",
                "proxy-connection",
                "keep-alive",
                "proxy-authorization",
            ]
            .iter()
            .any(|hop| name.eq_ignore_ascii_case(hop));
            if hop || remove.is_some_and(|remove| name.eq_ignore_ascii_case(remove)) {
                continue;
            }
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some((name, value)) = add {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str("Connection: close\r\n\r\n");
        out.into_bytes()
    }
}

/// Read a request head; returns it and whatever followed it.
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 4096];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            buffer.truncate(end);
            return Ok((buffer, rest));
        }
        if buffer.len() > MAX_HEAD {
            return Err(std::io::Error::other("request head too large"));
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

async fn refuse<S: AsyncWrite + Unpin>(stream: &mut S, status: &str, reason: &str) {
    let body = format!("compute egress: {reason}\n");
    let _ = stream
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;
    let _ = stream.shutdown().await;
}

fn split_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') || host.starts_with('[') => {
            (host, port.parse().ok()?)
        }
        _ => (authority, default_port),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = host.to_ascii_lowercase();
    (valid_host(&host) || host.parse::<IpAddr>().is_ok()).then_some((host, port))
}

impl State {
    fn record(&self, host: &str, port: u16, allowed: bool, brokered: bool, reason: &str) {
        self.decisions.lock().expect("decisions").push(Decision {
            host: host.to_owned(),
            port,
            allowed,
            brokered,
            reason: reason.to_owned(),
        });
    }

    /// The address to connect to, or why there is none.
    async fn admit(&self, host: &str, port: u16) -> Result<SocketAddr, String> {
        if !self
            .policy
            .allow
            .iter()
            .any(|rule| rule.matches(host, port))
        {
            return Err(format!("{host}:{port} is not allowed"));
        }
        let addresses = match self.resolve.get(host) {
            Some(fixed) => fixed.clone(),
            None => match host.parse::<IpAddr>() {
                Ok(address) => vec![SocketAddr::new(address, port)],
                Err(_) => tokio::net::lookup_host((host, port))
                    .await
                    .map_err(|error| format!("{host} does not resolve: {error}"))?
                    .collect(),
            },
        };
        let mut addresses = addresses.into_iter();
        let first = addresses
            .next()
            .ok_or_else(|| format!("{host} does not resolve"))?;
        // Every answer must be acceptable: one private answer among public
        // ones is a rebinding attempt, not a choice.
        for address in std::iter::once(first).chain(addresses) {
            if !self.policy.allow_private && !is_public(address.ip()) {
                return Err(format!(
                    "{host} resolves to {}, which is not a public address",
                    address.ip()
                ));
            }
        }
        Ok(first)
    }

    fn leaf(&self, host: &str) -> Result<Arc<rustls::ServerConfig>, String> {
        if let Some(config) = self.leaves.lock().expect("leaves").get(host) {
            return Ok(config.clone());
        }
        let key = KeyPair::generate().map_err(|error| error.to_string())?;
        let mut params =
            CertificateParams::new(vec![host.to_owned()]).map_err(|error| error.to_string())?;
        params.distinguished_name.push(DnType::CommonName, host);
        let certificate = params
            .signed_by(&key, &self.issuer)
            .map_err(|error| error.to_string())?;
        let mut config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|error| error.to_string())?
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.der().clone(), self.ca.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
            .map_err(|error| error.to_string())?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        self.leaves
            .lock()
            .expect("leaves")
            .insert(host.to_owned(), config.clone());
        Ok(config)
    }
}

async fn connection(mut client: TcpStream, state: &State) -> std::io::Result<()> {
    let (head, rest) = read_head(&mut client).await?;
    let Some(head) = Head::parse(&head) else {
        refuse(&mut client, "400 Bad Request", "unreadable request").await;
        return Ok(());
    };
    if head.method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = split_authority(&head.target, 443) else {
            refuse(&mut client, "400 Bad Request", "unreadable destination").await;
            return Ok(());
        };
        let address = match state.admit(&host, port).await {
            Ok(address) => address,
            Err(reason) => {
                state.record(&host, port, false, false, &reason);
                refuse(&mut client, "403 Forbidden", &reason).await;
                return Ok(());
            }
        };
        let binding = (port == 443).then(|| state.bindings.get(&host)).flatten();
        let Ok(mut upstream) = TcpStream::connect(address).await else {
            state.record(&host, port, false, false, "the upstream did not answer");
            refuse(
                &mut client,
                "502 Bad Gateway",
                "the upstream did not answer",
            )
            .await;
            return Ok(());
        };
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        match binding {
            None => {
                state.record(&host, port, true, false, "tunnelled");
                upstream.write_all(&rest).await?;
                tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
            }
            Some(binding) => {
                if !rest.is_empty() {
                    // TLS must begin after the tunnel is established.
                    return Ok(());
                }
                brokered(client, upstream, &host, binding, state).await?;
            }
        }
        return Ok(());
    }
    // Plain HTTP, in absolute form.
    let Some(authority_path) = head.target.strip_prefix("http://") else {
        refuse(
            &mut client,
            "400 Bad Request",
            "use CONNECT or an http:// URL",
        )
        .await;
        return Ok(());
    };
    let (authority, path) = match authority_path.find('/') {
        Some(at) => authority_path.split_at(at),
        None => (authority_path, "/"),
    };
    let Some((host, port)) = split_authority(authority, 80) else {
        refuse(&mut client, "400 Bad Request", "unreadable destination").await;
        return Ok(());
    };
    let address = match state.admit(&host, port).await {
        Ok(address) => address,
        Err(reason) => {
            state.record(&host, port, false, false, &reason);
            refuse(&mut client, "403 Forbidden", &reason).await;
            return Ok(());
        }
    };
    let Ok(mut upstream) = TcpStream::connect(address).await else {
        refuse(
            &mut client,
            "502 Bad Gateway",
            "the upstream did not answer",
        )
        .await;
        return Ok(());
    };
    // A credential is never sent in cleartext, so none is added here.
    state.record(&host, port, true, false, "forwarded (plain HTTP)");
    upstream
        .write_all(&head.forwarded(path, None, None))
        .await?;
    upstream.write_all(&rest).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Terminate the session's TLS for a bound host, add its credential, and
/// forward one request over TLS the gateway verifies.
async fn brokered(
    client: TcpStream,
    upstream: TcpStream,
    host: &str,
    binding: &CredentialBinding,
    state: &State,
) -> std::io::Result<()> {
    let config = state.leaf(host).map_err(std::io::Error::other)?;
    let mut inner = tokio_rustls::TlsAcceptor::from(config)
        .accept(client)
        .await?;
    let (head, rest) = read_head(&mut inner).await?;
    let Some(head) = Head::parse(&head) else {
        refuse(&mut inner, "400 Bad Request", "unreadable request").await;
        return Ok(());
    };
    // The request must be for the host the tunnel was opened to.
    let named = head
        .header("host")
        .and_then(|value| split_authority(value, 443))
        .map(|(named, _)| named);
    if named.as_deref() != Some(host) || !head.target.starts_with('/') {
        let reason = format!("a request in the tunnel to {host} names another host");
        state.record(host, 443, false, false, &reason);
        refuse(&mut inner, "403 Forbidden", &reason).await;
        return Ok(());
    }
    let name = ServerName::try_from(host.to_owned()).map_err(std::io::Error::other)?;
    let mut outer = tokio_rustls::TlsConnector::from(state.upstream.clone())
        .connect(name, upstream)
        .await?;
    state.record(host, 443, true, true, "credential added");
    outer
        .write_all(&head.forwarded(
            &head.target,
            Some(&binding.header),
            Some((&binding.header, &binding.value)),
        ))
        .await?;
    outer.write_all(&rest).await?;
    tokio::io::copy_bidirectional(&mut inner, &mut outer).await?;
    Ok(())
}
