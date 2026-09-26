//! Operator authentication and authorization for the Compute API.
//!
//! Every request resolves to a [`Principal`]: an operator, the credential
//! that proved it, its scopes, and a request ID. Every route declares the
//! scope it needs ([`required_scope`]); a request without it is refused.
//! Credentials are durable control state, keyed by a generated ID; the
//! secret is shown once and only its SHA-256 verifier is kept.
//!
//! ```text
//! token = cmpt_<credential_id>_<secret>
//!   credential_id  cred_<16 hex>   (public; names the credential)
//!   secret         64 hex          (256 random bits; never stored)
//! ```
//!
//! Development mode (explicit `--insecure`, or a loopback listener without
//! TLS) accepts requests without credentials as the `development`
//! operator. Production mode requires TLS and a credential on every
//! request, reads included, and fails closed.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use compute_state::OperatorCredentialRecord;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::EnvironmentError;

/// What an operator may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Scope {
    /// Read environments, projects, deployments, logs, evidence.
    #[serde(rename = "compute.read")]
    Read,
    /// Run tasks.
    #[serde(rename = "compute.execute")]
    Execute,
    /// Register revisions; deploy, promote, roll back; add projects.
    #[serde(rename = "compute.deploy")]
    Deploy,
    /// Start, stop, restart; environments, domains, DNS, certificates.
    #[serde(rename = "compute.operate")]
    Operate,
    /// Credentials, node lifecycle, shutdown, audit. Implies every scope.
    #[serde(rename = "compute.admin")]
    Admin,
}

impl Scope {
    pub const ALL: [Scope; 5] = [
        Scope::Read,
        Scope::Execute,
        Scope::Deploy,
        Scope::Operate,
        Scope::Admin,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "compute.read",
            Self::Execute => "compute.execute",
            Self::Deploy => "compute.deploy",
            Self::Operate => "compute.operate",
            Self::Admin => "compute.admin",
        }
    }

    pub fn parse(value: &str) -> Result<Self, EnvironmentError> {
        Self::ALL
            .into_iter()
            .find(|scope| scope.as_str() == value)
            .ok_or_else(|| {
                EnvironmentError::Invalid(format!(
                    "unknown scope {value}; scopes are {}",
                    Self::ALL.map(Scope::as_str).join(", ")
                ))
            })
    }
}

/// The scope a route needs. Anything unlisted needs `compute.admin`: new
/// routes fail closed until they declare otherwise.
pub fn required_scope(method: &str, segments: &[&str]) -> Scope {
    match (method, segments) {
        ("GET", ["audit", ..]) | ("GET", ["auth", "credentials", ..]) => Scope::Admin,
        ("GET", _) => Scope::Read,
        ("POST", ["environments", _, "projects", _, "workloads", _, "run"]) => Scope::Execute,
        // Runs, jobs, admission, and runtime preparation on this node as a
        // provider.
        ("POST", ["compute", ..]) => Scope::Execute,
        ("POST", ["projects", _, "revisions"])
        | ("POST", ["applications", _, "deployments"])
        | ("POST", ["applications", _, "rollback"])
        | ("POST", ["deployments"])
        | ("POST", ["deployments", "promote"])
        | ("POST", ["deployments", _, "rollback"])
        | ("POST", ["environments", _, "projects"])
        | ("DELETE", ["environments", _, "projects", _]) => Scope::Deploy,
        ("POST", ["environments"])
        | ("DELETE", ["environments", _])
        | ("POST", ["environments", _, "start" | "stop" | "restart"])
        | (
            "POST",
            [
                "environments",
                _,
                "projects",
                _,
                "start" | "stop" | "restart",
            ],
        )
        | (
            "POST",
            [
                "environments",
                _,
                "projects",
                _,
                "workloads",
                _,
                "start" | "stop" | "restart",
            ],
        )
        | ("POST", ["domains"])
        | ("DELETE", ["domains", _])
        | ("POST", ["dns", "reconcile"])
        | ("POST", ["certificates", _, "renew"])
        | ("POST", ["services"])
        | ("DELETE", ["services", _])
        | ("POST", ["applications", _, "stop"])
        | ("POST", ["node", "reconcile"]) => Scope::Operate,
        _ => Scope::Admin,
    }
}

/// Who is asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub operator_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    pub scopes: BTreeSet<Scope>,
    /// Admitted without a credential by development mode.
    #[serde(default)]
    pub development: bool,
}

impl Principal {
    pub fn allows(&self, scope: Scope) -> bool {
        self.scopes.contains(&Scope::Admin) || self.scopes.contains(&scope)
    }

    fn development() -> Self {
        Self {
            operator_id: "development".into(),
            credential_id: None,
            scopes: Scope::ALL.into_iter().collect(),
            development: true,
        }
    }
}

/// How the API is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityMode {
    /// Plaintext is allowed and requests without credentials are admitted
    /// as the `development` operator. Never for a reachable node.
    Development,
    /// TLS and a credential on every request.
    Production,
}

#[derive(Debug, Clone)]
pub struct SecurityConfig {
    pub mode: SecurityMode,
    /// Why this mode: shown by `compute doctor` and `/info`.
    pub reason: String,
    /// Whether the API listener terminates TLS.
    pub tls: bool,
    /// A pre-shared bearer token accepted as an admin credential. Only in
    /// development mode; production refuses to start with one.
    pub legacy_token: Option<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            mode: SecurityMode::Development,
            reason: "embedded daemon".into(),
            tls: false,
            legacy_token: None,
        }
    }
}

/// A credential as the API shows it: never the verifier or the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialView {
    pub credential_id: String,
    pub operator_id: String,
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// `active`, `expired`, or `revoked`.
    pub status: String,
}

impl CredentialView {
    pub fn of(record: &OperatorCredentialRecord, now: DateTime<Utc>) -> Self {
        Self {
            credential_id: record.credential_id.clone(),
            operator_id: record.operator_id.clone(),
            scopes: record.scopes.clone(),
            description: record.description.clone(),
            created_at: record.created_at,
            expires_at: record.expires_at,
            revoked_at: record.revoked_at,
            rotated_from: record.rotated_from.clone(),
            created_by: record.created_by.clone(),
            status: status(record, now).into(),
        }
    }
}

fn status(record: &OperatorCredentialRecord, now: DateTime<Utc>) -> &'static str {
    if record.revoked_at.is_some_and(|at| at <= now) {
        "revoked"
    } else if record.expires_at.is_some_and(|at| at <= now) {
        "expired"
    } else {
        "active"
    }
}

/// A new credential: the one response that carries its token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuedCredential {
    pub credential: CredentialView,
    /// Shown once. Compute keeps only its verifier.
    pub token: String,
}

/// A request to create a credential.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CredentialRequest {
    pub operator_id: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Lifetime in seconds; none means it lasts until revoked.
    #[serde(default)]
    pub expires_in_seconds: Option<u64>,
}

/// A request to rotate a credential.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RotateRequest {
    /// How long the old credential keeps working, so clients can switch.
    /// Zero (the default) revokes it immediately.
    #[serde(default)]
    pub grace_seconds: u64,
}

const TOKEN_PREFIX: &str = "cmpt_";

/// A fresh credential: its record and the token to hand out once.
pub(crate) fn mint(
    request: &CredentialRequest,
    created_by: Option<&str>,
    rotated_from: Option<String>,
) -> Result<(OperatorCredentialRecord, String), EnvironmentError> {
    let operator_id = request.operator_id.trim();
    if operator_id.is_empty()
        || operator_id.len() > 128
        || !operator_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c))
    {
        return Err(EnvironmentError::Invalid(
            "operator_id must be 1-128 characters of letters, digits, - _ . @".into(),
        ));
    }
    if request.scopes.is_empty() {
        return Err(EnvironmentError::Invalid(
            "a credential needs at least one scope".into(),
        ));
    }
    let scopes = request
        .scopes
        .iter()
        .map(|scope| Scope::parse(scope).map(|scope| scope.as_str().to_string()))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let credential_id = format!("cred_{}", hex(&random::<8>()?));
    let secret = hex(&random::<32>()?);
    let now = Utc::now();
    let record = OperatorCredentialRecord {
        credential_id: credential_id.clone(),
        operator_id: operator_id.into(),
        scopes: scopes.into_iter().collect(),
        verifier: verifier(&secret),
        description: request.description.clone(),
        created_at: now,
        expires_at: request
            .expires_in_seconds
            .map(|seconds| now + chrono::TimeDelta::seconds(seconds.min(i64::MAX as u64) as i64)),
        revoked_at: None,
        rotated_from,
        created_by: created_by.map(str::to_owned),
    };
    Ok((record, format!("{TOKEN_PREFIX}{credential_id}_{secret}")))
}

fn verifier(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random<const N: usize>() -> Result<[u8; N], EnvironmentError> {
    use std::io::Read;
    let mut bytes = [0_u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|error| {
            EnvironmentError::Io(std::io::Error::new(
                error.kind(),
                format!("no secure randomness for credentials: {error}"),
            ))
        })?;
    Ok(bytes)
}

/// Comparison that takes the same time wherever the inputs differ.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// A request ID: unique per API request, returned in `X-Request-Id`.
pub fn request_id() -> String {
    match random::<8>() {
        Ok(bytes) => format!("req_{}", hex(&bytes)),
        Err(_) => format!(
            "req_{:016x}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ),
    }
}

/// The node's view of operator credentials: verifiers only. Durable
/// control state is the authority; this is its cache, refreshed from it,
/// with a node-local snapshot so a node that starts while control state is
/// unreachable can still authenticate reads.
pub(crate) struct Authority {
    pub config: SecurityConfig,
    credentials: RwLock<HashMap<String, OperatorCredentialRecord>>,
    loaded_at: RwLock<Option<DateTime<Utc>>>,
    snapshot: PathBuf,
}

impl Authority {
    pub fn new(config: SecurityConfig, snapshot: PathBuf) -> Self {
        Self {
            config,
            credentials: RwLock::new(HashMap::new()),
            loaded_at: RwLock::new(None),
            snapshot,
        }
    }

    /// Resolve an `Authorization` header to a principal.
    pub fn authenticate(&self, authorization: Option<&str>) -> Result<Principal, EnvironmentError> {
        let Some(header) = authorization else {
            return match self.config.mode {
                SecurityMode::Development if self.config.legacy_token.is_none() => {
                    Ok(Principal::development())
                }
                _ => Err(EnvironmentError::Unauthorized(
                    "this Compute API requires a credential: Authorization: Bearer <token>".into(),
                )),
            };
        };
        let token = header
            .strip_prefix("Bearer ")
            .map(str::trim)
            .ok_or_else(|| {
                EnvironmentError::Unauthorized("expected Authorization: Bearer <token>".into())
            })?;
        if let Some(legacy) = &self.config.legacy_token
            && constant_time_eq(verifier(token).as_bytes(), verifier(legacy).as_bytes())
        {
            return Ok(Principal {
                operator_id: "legacy-token".into(),
                credential_id: None,
                scopes: Scope::ALL.into_iter().collect(),
                development: false,
            });
        }
        let rejected = || EnvironmentError::Unauthorized("the credential is not valid".into());
        let (credential_id, secret) = token
            .strip_prefix(TOKEN_PREFIX)
            .and_then(|rest| rest.rsplit_once('_'))
            .ok_or_else(rejected)?;
        let record = self
            .credentials
            .read()
            .expect("credentials")
            .get(credential_id)
            .cloned()
            .ok_or_else(rejected)?;
        if !constant_time_eq(verifier(secret).as_bytes(), record.verifier.as_bytes()) {
            return Err(rejected());
        }
        match status(&record, Utc::now()) {
            "revoked" => Err(EnvironmentError::Unauthorized(format!(
                "credential {credential_id} was revoked"
            ))),
            "expired" => Err(EnvironmentError::Unauthorized(format!(
                "credential {credential_id} expired"
            ))),
            _ => Ok(Principal {
                operator_id: record.operator_id.clone(),
                credential_id: Some(record.credential_id.clone()),
                scopes: record
                    .scopes
                    .iter()
                    .filter_map(|scope| Scope::parse(scope).ok())
                    .collect(),
                development: false,
            }),
        }
    }

    /// Replace the cache with what control state holds, and snapshot it.
    pub fn replace(&self, records: Vec<OperatorCredentialRecord>) {
        let map = records
            .into_iter()
            .map(|record| (record.credential_id.clone(), record))
            .collect::<HashMap<_, _>>();
        self.write_snapshot(&map);
        *self.credentials.write().expect("credentials") = map;
        *self.loaded_at.write().expect("loaded") = Some(Utc::now());
    }

    /// Apply one credential change this node made.
    pub fn upsert(&self, record: OperatorCredentialRecord) {
        let mut credentials = self.credentials.write().expect("credentials");
        credentials.insert(record.credential_id.clone(), record);
        self.write_snapshot(&credentials);
    }

    /// Load the node-local snapshot: used only while control state is
    /// unreachable at start.
    pub fn load_snapshot(&self) -> bool {
        let Ok(bytes) = std::fs::read(&self.snapshot) else {
            return false;
        };
        let Ok(records) = serde_json::from_slice::<Vec<OperatorCredentialRecord>>(&bytes) else {
            return false;
        };
        *self.credentials.write().expect("credentials") = records
            .into_iter()
            .map(|record| (record.credential_id.clone(), record))
            .collect();
        true
    }

    fn write_snapshot(&self, credentials: &HashMap<String, OperatorCredentialRecord>) {
        let records = credentials.values().collect::<Vec<_>>();
        let Ok(bytes) = serde_json::to_vec(&records) else {
            return;
        };
        let temporary = self.snapshot.with_extension("tmp");
        let written = {
            #[cfg(unix)]
            {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&temporary)
                    .and_then(|mut file| file.write_all(&bytes))
            }
            #[cfg(not(unix))]
            {
                std::fs::write(&temporary, &bytes)
            }
        };
        if written.is_ok() {
            let _ = std::fs::rename(&temporary, &self.snapshot);
        }
    }

    pub fn records(&self) -> Vec<OperatorCredentialRecord> {
        let mut records = self
            .credentials
            .read()
            .expect("credentials")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        records
    }

    pub fn loaded_at(&self) -> Option<DateTime<Utc>> {
        *self.loaded_at.read().expect("loaded")
    }

    /// Whether any active credential grants `compute.admin`.
    pub fn has_active_admin(&self) -> bool {
        let now = Utc::now();
        self.credentials
            .read()
            .expect("credentials")
            .values()
            .any(|record| {
                status(record, now) == "active"
                    && record
                        .scopes
                        .iter()
                        .any(|scope| scope == Scope::Admin.as_str())
            })
    }
}

tokio::task_local! {
    /// The request an operation serves, so the events it records name who
    /// asked for it.
    pub(crate) static REQUEST: RequestContext;
}

tokio::task_local! {
    /// How fresh the desired state a response was built from is.
    pub(crate) static FRESHNESS: std::sync::Arc<std::sync::Mutex<Option<(&'static str, DateTime<Utc>)>>>;
}

/// Record, for the response being built, where its desired state came
/// from: `live` (read now), `cached` (a recent read, nothing written
/// since), or `stale` (durable state is unreachable; the last read).
pub(crate) fn set_freshness(kind: &'static str, as_of: DateTime<Utc>) {
    let _ = FRESHNESS.try_with(|freshness| {
        let mut freshness = freshness.lock().expect("freshness");
        // The least fresh source wins.
        let rank = |kind: &str| match kind {
            "live" => 0,
            "cached" => 1,
            _ => 2,
        };
        if freshness.is_none_or(|(current, _)| rank(kind) >= rank(current)) {
            *freshness = Some((kind, as_of));
        }
    });
}

#[derive(Debug, Clone)]
pub(crate) struct RequestContext {
    pub request_id: String,
    pub operator_id: String,
    pub credential_id: Option<String>,
}

impl RequestContext {
    pub fn current() -> Option<Self> {
        REQUEST.try_with(Clone::clone).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority(mode: SecurityMode) -> (Authority, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let authority = Authority::new(
            SecurityConfig {
                mode,
                reason: "test".into(),
                tls: mode == SecurityMode::Production,
                legacy_token: None,
            },
            dir.path().join("credentials.json"),
        );
        (authority, dir)
    }

    fn request(scopes: &[&str]) -> CredentialRequest {
        CredentialRequest {
            operator_id: "developer-42".into(),
            scopes: scopes.iter().map(|scope| scope.to_string()).collect(),
            description: None,
            expires_in_seconds: None,
        }
    }

    #[test]
    fn production_requires_a_valid_active_credential() {
        let (authority, _dir) = authority(SecurityMode::Production);
        assert!(matches!(
            authority.authenticate(None),
            Err(EnvironmentError::Unauthorized(_))
        ));
        let (record, token) = mint(&request(&["compute.read"]), None, None).unwrap();
        assert!(
            !serde_json::to_string(&record)
                .unwrap()
                .contains(token.split('_').last().unwrap())
        );
        authority.upsert(record.clone());
        let principal = authority
            .authenticate(Some(&format!("Bearer {token}")))
            .unwrap();
        assert_eq!(principal.operator_id, "developer-42");
        assert!(principal.allows(Scope::Read));
        assert!(!principal.allows(Scope::Deploy));
        // A wrong secret for a real credential ID.
        let forged = format!("{}{}", &token[..token.len() - 1], "0");
        let forged = if forged == token {
            format!("{}1", &token[..token.len() - 1])
        } else {
            forged
        };
        assert!(
            authority
                .authenticate(Some(&format!("Bearer {forged}")))
                .is_err()
        );
        assert!(authority.authenticate(Some("Bearer nonsense")).is_err());
        assert!(authority.authenticate(Some("Basic abc")).is_err());
        // Expired and revoked credentials are refused.
        let mut expired = record.clone();
        expired.expires_at = Some(Utc::now() - chrono::TimeDelta::seconds(1));
        authority.upsert(expired);
        assert!(
            authority
                .authenticate(Some(&format!("Bearer {token}")))
                .is_err()
        );
        let mut revoked = record;
        revoked.revoked_at = Some(Utc::now());
        authority.upsert(revoked);
        let error = authority
            .authenticate(Some(&format!("Bearer {token}")))
            .unwrap_err();
        assert!(error.message().contains("revoked"));
        // The error never echoes the token.
        assert!(!error.message().contains(&token));
    }

    #[test]
    fn development_admits_anonymous_requests_but_still_checks_credentials() {
        let (authority, _dir) = authority(SecurityMode::Development);
        assert!(authority.authenticate(None).unwrap().development);
        assert!(
            authority
                .authenticate(Some("Bearer cmpt_cred_x_y"))
                .is_err()
        );
    }

    #[test]
    fn the_snapshot_holds_verifiers_only() {
        let (authority, dir) = authority(SecurityMode::Production);
        let (record, token) = mint(&request(&["compute.admin"]), None, None).unwrap();
        authority.replace(vec![record]);
        let snapshot = std::fs::read_to_string(dir.path().join("credentials.json")).unwrap();
        let secret = token.rsplit_once('_').unwrap().1;
        assert!(!snapshot.contains(secret));
        let restored = Authority::new(
            authority.config.clone(),
            dir.path().join("credentials.json"),
        );
        assert!(restored.load_snapshot());
        assert!(
            restored
                .authenticate(Some(&format!("Bearer {token}")))
                .is_ok()
        );
        assert!(restored.has_active_admin());
    }

    #[test]
    fn every_route_declares_a_scope_and_unknown_routes_need_admin() {
        assert_eq!(required_scope("GET", &["environments"]), Scope::Read);
        assert_eq!(
            required_scope("GET", &["auth", "credentials"]),
            Scope::Admin
        );
        assert_eq!(
            required_scope(
                "POST",
                &[
                    "environments",
                    "e",
                    "projects",
                    "p",
                    "workloads",
                    "w",
                    "run"
                ]
            ),
            Scope::Execute
        );
        assert_eq!(required_scope("POST", &["deployments"]), Scope::Deploy);
        assert_eq!(
            required_scope("POST", &["applications", "a", "deployments"]),
            Scope::Deploy
        );
        assert_eq!(
            required_scope("POST", &["applications", "a", "rollback"]),
            Scope::Deploy
        );
        assert_eq!(
            required_scope("POST", &["applications", "a", "stop"]),
            Scope::Operate
        );
        assert_eq!(
            required_scope("GET", &["compute", "capabilities"]),
            Scope::Read
        );
        assert_eq!(
            required_scope("POST", &["deployments", "d", "rollback"]),
            Scope::Deploy
        );
        assert_eq!(
            required_scope(
                "POST",
                &[
                    "environments",
                    "e",
                    "projects",
                    "p",
                    "workloads",
                    "w",
                    "restart"
                ]
            ),
            Scope::Operate
        );
        assert_eq!(required_scope("POST", &["shutdown"]), Scope::Admin);
        assert_eq!(
            required_scope("POST", &["auth", "credentials"]),
            Scope::Admin
        );
        assert_eq!(required_scope("PATCH", &["anything"]), Scope::Admin);
        for (method, route) in crate::api::ROUTES {
            let segments = route
                .trim_matches('/')
                .split('/')
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>();
            // Declared, never panics; reads are never admin-only except
            // credentials and audit.
            let scope = required_scope(method, &segments);
            if *method == "GET" && !route.starts_with("/auth") && !route.starts_with("/audit") {
                assert_eq!(scope, Scope::Read, "{method} {route}");
            }
        }
    }
}
