//! Target credentials: which control planes a `compute serve` target
//! trusts.
//!
//! A target is controlled by exactly the control planes it has issued a
//! credential to. The credential uses Compute's credential format and
//! keeps only a verifier, as operator credentials do:
//!
//! ```text
//! token = cmpt_<credential_id>_<secret>
//!   credential_id  tcred_<16 hex>   (public; names the credential)
//!   secret         64 hex           (256 random bits; never stored)
//! ```
//!
//! The trust file (`compute.target.credentials@1`) holds, per credential,
//! the control plane it identifies and the SHA-256 verifier of its secret.
//! Every session and job a request creates belongs to the control plane
//! (`control-plane:<id>`), never to the token: rotating a control plane's
//! credential keeps what it owns, and another control plane's credential
//! reaches nothing of it. Revoking a credential, or deleting it from the
//! file, ends its access at the next request; the file is re-read whenever
//! it changes.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ProviderAuthorizer, ProviderError, ProviderErrorKind, ProviderOperation};

pub const TARGET_CREDENTIALS_VERSION: &str = "compute.target.credentials@1";
const TOKEN_PREFIX: &str = "cmpt_";
const ID_PREFIX: &str = "tcred_";

/// One credential a target trusts: never the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetCredentialRecord {
    pub credential_id: String,
    /// The control plane this credential authenticates. What it creates on
    /// the target belongs to this identity.
    pub control_plane: String,
    pub verifier: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
}

impl TargetCredentialRecord {
    pub fn active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// The trust file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetCredentials {
    pub version: String,
    #[serde(default)]
    pub credentials: Vec<TargetCredentialRecord>,
}

impl Default for TargetCredentials {
    fn default() -> Self {
        Self {
            version: TARGET_CREDENTIALS_VERSION.into(),
            credentials: vec![],
        }
    }
}

fn unauthorized(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Unauthorized, message)
}

fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::ArtifactInvalid, message)
}

fn verifier(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random<const N: usize>() -> Result<[u8; N], ProviderError> {
    use std::io::Read;
    let mut bytes = [0_u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("no secure randomness for credentials: {error}"),
            )
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

/// Validate a control-plane identity.
pub fn validate_control_plane(id: &str) -> Result<(), ProviderError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c))
    {
        return Err(invalid(
            "a control plane identity is 1-128 characters of letters, digits, - _ . @",
        ));
    }
    Ok(())
}

/// The owner every request authenticated for `control_plane` acts as.
pub fn control_plane_owner(control_plane: &str) -> String {
    format!("control-plane:{control_plane}")
}

impl TargetCredentials {
    /// Read a trust file. A missing file is an error: a target without one
    /// trusts nobody.
    pub fn load(path: &Path) -> Result<Self, ProviderError> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("target credentials {}: {error}", path.display()),
            )
        })?;
        let credentials: Self = serde_json::from_str(&text)
            .map_err(|error| invalid(format!("target credentials {}: {error}", path.display())))?;
        if credentials.version != TARGET_CREDENTIALS_VERSION {
            return Err(invalid(format!(
                "target credentials {} are {}, expected {TARGET_CREDENTIALS_VERSION}",
                path.display(),
                credentials.version
            )));
        }
        Ok(credentials)
    }

    /// Read a trust file, or start an empty one.
    pub fn load_or_default(path: &Path) -> Result<Self, ProviderError> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }

    /// Write the trust file atomically, readable by its owner only.
    pub fn save(&self, path: &Path) -> Result<(), ProviderError> {
        let io = |error: std::io::Error| {
            ProviderError::new(
                ProviderErrorKind::ProviderUnavailable,
                format!("target credentials {}: {error}", path.display()),
            )
        };
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let temporary = path.with_extension("json.tmp");
        write_private(
            &temporary,
            serde_json::to_vec_pretty(self).expect("credentials serialize"),
        )
        .map_err(io)?;
        std::fs::rename(&temporary, path).map_err(io)
    }

    /// Issue a credential for `control_plane`. Returns the record and the
    /// token, which is shown once and never stored.
    pub fn issue(
        &mut self,
        control_plane: &str,
    ) -> Result<(TargetCredentialRecord, String), ProviderError> {
        validate_control_plane(control_plane)?;
        let credential_id = format!("{ID_PREFIX}{}", hex(&random::<8>()?));
        let secret = hex(&random::<32>()?);
        let record = TargetCredentialRecord {
            credential_id: credential_id.clone(),
            control_plane: control_plane.into(),
            verifier: verifier(&secret),
            created_at: Utc::now(),
            revoked_at: None,
        };
        self.credentials.push(record.clone());
        Ok((record, format!("{TOKEN_PREFIX}{credential_id}_{secret}")))
    }

    /// Revoke a credential. Revoking one already revoked changes nothing.
    pub fn revoke(&mut self, credential_id: &str) -> Result<TargetCredentialRecord, ProviderError> {
        let record = self
            .credentials
            .iter_mut()
            .find(|record| record.credential_id == credential_id)
            .ok_or_else(|| invalid(format!("no target credential {credential_id}")))?;
        if record.revoked_at.is_none() {
            record.revoked_at = Some(Utc::now());
        }
        Ok(record.clone())
    }

    /// The control plane a bearer token authenticates.
    pub fn authenticate(&self, authorization: Option<&str>) -> Result<String, ProviderError> {
        let header = authorization.ok_or_else(|| {
            unauthorized("this target requires a credential: Authorization: Bearer <token>")
        })?;
        let token = header
            .strip_prefix("Bearer ")
            .map(str::trim)
            .ok_or_else(|| unauthorized("expected Authorization: Bearer <token>"))?;
        let rejected = || unauthorized("the target credential is not valid");
        let (credential_id, secret) = token
            .strip_prefix(TOKEN_PREFIX)
            .and_then(|rest| rest.rsplit_once('_'))
            .ok_or_else(rejected)?;
        let record = self
            .credentials
            .iter()
            .find(|record| record.credential_id == credential_id)
            .ok_or_else(rejected)?;
        if !constant_time_eq(verifier(secret).as_bytes(), record.verifier.as_bytes()) {
            return Err(rejected());
        }
        if !record.active() {
            return Err(unauthorized(format!(
                "target credential {credential_id} was revoked"
            )));
        }
        Ok(record.control_plane.clone())
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: Vec<u8>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: Vec<u8>) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// Write a token for a client to present, readable by its owner only.
pub fn write_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    write_private(path, format!("{token}\n").into_bytes())
}

/// Read a token a client presents.
pub fn read_token_file(path: &Path) -> std::io::Result<String> {
    let token = std::fs::read_to_string(path)?.trim().to_owned();
    if token.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} holds no token", path.display()),
        ));
    }
    Ok(token)
}

/// Where the trusted credentials come from.
enum Source {
    File {
        path: PathBuf,
        /// The file as last read, and when it was modified.
        cache: RwLock<Option<(std::time::SystemTime, TargetCredentials)>>,
    },
    Fixed(TargetCredentials),
}

/// A target's authority: every request must carry a credential the target
/// has issued, and acts as the control plane that credential names.
pub struct TargetAuthorizer {
    source: Source,
}

impl TargetAuthorizer {
    /// Trust the credentials in `path`, re-read whenever it changes. A
    /// missing or unreadable file admits no one.
    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        Self {
            source: Source::File {
                path: path.into(),
                cache: RwLock::new(None),
            },
        }
    }

    /// Trust exactly these credentials, for a target embedded in a process.
    pub fn fixed(credentials: TargetCredentials) -> Self {
        Self {
            source: Source::Fixed(credentials),
        }
    }

    /// A target that trusts one fresh credential for `control_plane`, and
    /// that credential's token.
    pub fn issue_for(control_plane: &str) -> Result<(Self, String), ProviderError> {
        let mut credentials = TargetCredentials::default();
        let (_, token) = credentials.issue(control_plane)?;
        Ok((Self::fixed(credentials), token))
    }

    fn credentials(&self) -> Result<TargetCredentials, ProviderError> {
        match &self.source {
            Source::Fixed(credentials) => Ok(credentials.clone()),
            Source::File { path, cache } => {
                let modified = std::fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .map_err(|error| {
                        unauthorized(format!(
                            "this target trusts no credentials ({}: {error})",
                            path.display()
                        ))
                    })?;
                if let Some((at, credentials)) = cache.read().expect("credentials").as_ref()
                    && *at == modified
                {
                    return Ok(credentials.clone());
                }
                let credentials =
                    TargetCredentials::load(path).map_err(|error| unauthorized(error.message))?;
                *cache.write().expect("credentials") = Some((modified, credentials.clone()));
                Ok(credentials)
            }
        }
    }
}

#[async_trait]
impl ProviderAuthorizer for TargetAuthorizer {
    async fn authorize(
        &self,
        _: ProviderOperation,
        authorization: Option<&str>,
    ) -> Result<(), ProviderError> {
        self.credentials()?.authenticate(authorization).map(|_| ())
    }

    async fn owner(&self, authorization: Option<&str>) -> Result<String, ProviderError> {
        self.credentials()?
            .authenticate(authorization)
            .map(|control_plane| control_plane_owner(&control_plane))
    }

    fn authentication(&self) -> &'static str {
        "credential"
    }
}

/// A target with no credentials configured: it refuses every request. The
/// default for an endpoint nobody gave an authority to.
pub struct NoCredentialsConfigured;

#[async_trait]
impl ProviderAuthorizer for NoCredentialsConfigured {
    async fn authorize(&self, _: ProviderOperation, _: Option<&str>) -> Result<(), ProviderError> {
        Err(unauthorized(
            "this target has no credentials configured and accepts no requests",
        ))
    }

    fn authentication(&self) -> &'static str {
        "unconfigured"
    }
}

/// Explicitly unauthenticated: anyone who reaches the endpoint controls it.
/// Only for local development and tests, by name (`compute serve
/// --insecure-unauthenticated`); the target advertises it in its
/// capabilities, and `compute target list` shows it.
pub struct InsecureUnauthenticated;

#[async_trait]
impl ProviderAuthorizer for InsecureUnauthenticated {
    async fn authorize(&self, _: ProviderOperation, _: Option<&str>) -> Result<(), ProviderError> {
        Ok(())
    }

    fn authentication(&self) -> &'static str {
        INSECURE_AUTHENTICATION
    }
}

/// How an unauthenticated endpoint describes itself.
pub const INSECURE_AUTHENTICATION: &str = "insecure-unauthenticated";

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_target_admits_only_its_credentials_and_owns_by_control_plane() {
        let mut credentials = TargetCredentials::default();
        let (first, token) = credentials.issue("cp-a").unwrap();
        let (_, rotated) = credentials.issue("cp-a").unwrap();
        let (_, other) = credentials.issue("cp-b").unwrap();
        let authorizer = TargetAuthorizer::fixed(credentials.clone());
        let bearer = |token: &str| format!("Bearer {token}");
        let op = ProviderOperation::SessionList;
        assert!(authorizer.authorize(op, None).await.is_err());
        assert!(authorizer.authorize(op, Some("Bearer nope")).await.is_err());
        let last = if token.ends_with('0') { '1' } else { '0' };
        let forged = format!("{}{last}", &token[..token.len() - 1]);
        assert!(
            authorizer
                .authorize(op, Some(&bearer(&forged)))
                .await
                .is_err()
        );
        assert!(
            authorizer
                .authorize(op, Some(&bearer(&token)))
                .await
                .is_ok()
        );
        assert_eq!(
            authorizer.owner(Some(&bearer(&token))).await.unwrap(),
            "control-plane:cp-a"
        );
        assert_eq!(
            authorizer.owner(Some(&bearer(&rotated))).await.unwrap(),
            "control-plane:cp-a"
        );
        assert_eq!(
            authorizer.owner(Some(&bearer(&other))).await.unwrap(),
            "control-plane:cp-b"
        );
        credentials.revoke(&first.credential_id).unwrap();
        let revoked = TargetAuthorizer::fixed(credentials);
        let error = revoked
            .authorize(op, Some(&bearer(&token)))
            .await
            .unwrap_err();
        assert!(error.message.contains("revoked"), "{error:?}");
        assert!(revoked.authorize(op, Some(&bearer(&rotated))).await.is_ok());
    }

    #[tokio::test]
    async fn the_trust_file_holds_verifiers_and_is_reread_when_it_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        let authorizer = TargetAuthorizer::from_file(&path);
        let op = ProviderOperation::Health;
        // No file: nobody is trusted.
        assert!(authorizer.authorize(op, None).await.is_err());
        let mut credentials = TargetCredentials::default();
        let (record, token) = credentials.issue("cp").unwrap();
        credentials.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(token.rsplit_once('_').unwrap().1));
        let bearer = format!("Bearer {token}");
        assert!(authorizer.authorize(op, Some(&bearer)).await.is_ok());
        // Revoked on disk: refused at the next request.
        std::thread::sleep(std::time::Duration::from_millis(20));
        credentials.revoke(&record.credential_id).unwrap();
        credentials.save(&path).unwrap();
        let touched = std::fs::File::options().append(true).open(&path).unwrap();
        touched
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(1))
            .unwrap();
        assert!(authorizer.authorize(op, Some(&bearer)).await.is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "the trust file is private");
        }
    }
}
