//! Caller-owned provider pools, capability discovery, and capability caching.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use compute_provider::{
    ComputeProvider, LocalProvider, ProviderCapabilities, ProviderErrorKind, RemoteProvider,
};
use serde::{Deserialize, Serialize};

use crate::PlacementError;
use crate::descriptor::{Availability, DescriptorError, Health, ProviderDescriptor, ProviderKind};

pub const DEFAULT_CAPABILITY_TTL_SECONDS: u64 = 300;
pub const CAPABILITY_CACHE_VERSION: &str = "compute.provider.cache@1";

/// Pool-wide selection policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolPolicy {
    /// Exclude providers whose observed health is not `healthy` from
    /// selection. Off by default: health never overrides capability.
    #[serde(default)]
    pub require_healthy: bool,
    /// How long discovered capabilities remain valid.
    #[serde(default = "default_ttl")]
    pub capability_ttl_seconds: u64,
    /// Permit stale cached capabilities to establish compatibility.
    #[serde(default)]
    pub allow_stale_capabilities: bool,
}

impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            require_healthy: false,
            capability_ttl_seconds: DEFAULT_CAPABILITY_TTL_SECONDS,
            allow_stale_capabilities: false,
        }
    }
}

fn default_ttl() -> u64 {
    DEFAULT_CAPABILITY_TTL_SECONDS
}

/// One configured provider. Credentials are referenced by environment
/// variable name and never stored, displayed, or hashed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Accepted for compatibility and not used: an application's endpoint
    /// is the one the provider's Compute daemon returns when it deploys it
    /// (`compute start --application-host` sets what that daemon reports).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_endpoint: Option<String>,
    #[serde(default)]
    pub priority: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
}

/// TOML pool configuration:
///
/// ```toml
/// [pool]
/// require_healthy = false
///
/// [providers.local]
/// kind = "local"
/// priority = 100
///
/// [providers.dev]
/// kind = "remote"
/// endpoint = "http://compute-dev:8080"
/// priority = 50
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PoolConfig {
    #[serde(default)]
    pub pool: PoolPolicy,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

impl PoolConfig {
    pub fn parse(text: &str) -> Result<Self, PlacementError> {
        let config: Self = toml::from_str(text)
            .map_err(|error| PlacementError::InvalidConfig(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self, PlacementError> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            PlacementError::InvalidConfig(format!("{}: {error}", path.display()))
        })?;
        Self::parse(&text)
    }

    /// The implicit pool used when no configuration exists: the local
    /// provider alone.
    pub fn local_only() -> Self {
        Self {
            pool: PoolPolicy::default(),
            providers: BTreeMap::from([(
                "local".into(),
                ProviderConfig {
                    kind: ProviderKind::Local,
                    endpoint: None,
                    application_endpoint: None,
                    priority: 0,
                    token_env: None,
                },
            )]),
        }
    }

    pub fn validate(&self) -> Result<(), PlacementError> {
        if self.pool.capability_ttl_seconds == 0 {
            return Err(PlacementError::InvalidConfig(
                "capability_ttl_seconds must be positive".into(),
            ));
        }
        for (id, provider) in &self.providers {
            validate_provider_id(id)?;
            provider.validate(id)?;
        }
        Ok(())
    }
}

impl ProviderConfig {
    fn validate(&self, id: &str) -> Result<(), PlacementError> {
        let invalid = |message: &str| {
            Err(PlacementError::InvalidConfig(format!(
                "provider {id}: {message}"
            )))
        };
        match self.kind {
            ProviderKind::Local => {
                if self.endpoint.is_some() {
                    return invalid("a local provider has no endpoint");
                }
                if self.token_env.is_some() {
                    return invalid("a local provider has no credentials");
                }
            }
            ProviderKind::Remote => match &self.endpoint {
                Some(endpoint)
                    if (endpoint.starts_with("http://") || endpoint.starts_with("https://"))
                        && !endpoint
                            .chars()
                            .any(|c| c.is_control() || c.is_whitespace()) =>
                {
                    if endpoint.split("://").nth(1).is_some_and(|rest| {
                        rest.split('/')
                            .next()
                            .is_some_and(|authority| authority.contains('@'))
                    }) {
                        return invalid("credentials must not be embedded in the endpoint");
                    }
                }
                _ => return invalid("a remote provider requires an http:// or https:// endpoint"),
            },
        }
        if let Some(name) = &self.token_env
            && (name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
        {
            return invalid("token_env must name an environment variable");
        }
        if let Some(endpoint) = &self.application_endpoint
            && (!(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
                || endpoint
                    .chars()
                    .any(|character| character.is_control() || character.is_whitespace()))
        {
            return invalid("application_endpoint must be an http:// or https:// URL");
        }
        Ok(())
    }

    /// Configuration fields that identify where the provider is. Used to
    /// invalidate cached capabilities when configuration changes.
    pub fn fingerprint(&self) -> String {
        crate::canonical_identity(&(self.kind, &self.endpoint, &self.application_endpoint))
    }
}

pub fn validate_provider_id(id: &str) -> Result<(), PlacementError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(PlacementError::InvalidConfig(format!(
            "invalid provider identifier {id:?}: use 1-64 ASCII letters, digits, '-' or '_'"
        )));
    }
    Ok(())
}

pub struct PoolMember {
    pub id: String,
    pub config: ProviderConfig,
    pub provider: Arc<dyn ComputeProvider>,
    /// Durable-job transport, present for remote providers.
    pub jobs: Option<Arc<RemoteProvider>>,
}

/// Public, credential-free view of a pool member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolMemberInspection {
    pub provider_id: String,
    pub kind: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_endpoint: Option<String>,
    pub priority: i64,
    pub authenticated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolInspection {
    pub policy: PoolPolicy,
    /// Members in selection order: priority descending, then identifier.
    pub providers: Vec<PoolMemberInspection>,
}

/// A caller-owned set of execution providers. The pool evaluates providers;
/// the providers execute.
pub struct ProviderPool {
    policy: PoolPolicy,
    members: BTreeMap<String, PoolMember>,
}

impl ProviderPool {
    pub fn new(policy: PoolPolicy) -> Self {
        Self {
            policy,
            members: BTreeMap::new(),
        }
    }

    /// Build a pool whose members are the configured local and remote
    /// providers. Remote credentials are read from `token_env` here and
    /// held only by the transport.
    pub fn from_config(config: &PoolConfig) -> Result<Self, PlacementError> {
        config.validate()?;
        let mut pool = Self::new(config.pool.clone());
        for (id, provider) in &config.providers {
            match provider.kind {
                ProviderKind::Local => {
                    // This machine hosts deployments through its local
                    // Compute daemon, which `compute deploy` starts on
                    // demand.
                    pool.add(
                        id.clone(),
                        provider.clone(),
                        Arc::new(LocalProvider::new().with_execution_modes(
                            compute_provider::ExecutionModes {
                                run: true,
                                jobs: false,
                                deployments: true,
                            },
                        )),
                    )?;
                }
                ProviderKind::Remote => {
                    let mut remote =
                        RemoteProvider::new(provider.endpoint.clone().expect("validated"));
                    if let Some(name) = &provider.token_env {
                        let token = std::env::var(name).map_err(|_| {
                            PlacementError::InvalidConfig(format!(
                                "provider {id}: environment variable {name} is not set"
                            ))
                        })?;
                        remote = remote.with_bearer_token(token);
                    }
                    pool.add_remote(id.clone(), provider.clone(), Arc::new(remote))?;
                }
            }
        }
        Ok(pool)
    }

    pub fn policy(&self) -> &PoolPolicy {
        &self.policy
    }

    pub fn configs(&self) -> BTreeMap<String, ProviderConfig> {
        self.members
            .iter()
            .map(|(id, member)| (id.clone(), member.config.clone()))
            .collect()
    }

    pub fn add(
        &mut self,
        id: impl Into<String>,
        config: ProviderConfig,
        provider: Arc<dyn ComputeProvider>,
    ) -> Result<(), PlacementError> {
        self.insert(id.into(), config, provider, None)
    }

    /// Add a remote provider, which can also accept durable jobs.
    pub fn add_remote(
        &mut self,
        id: impl Into<String>,
        config: ProviderConfig,
        provider: Arc<RemoteProvider>,
    ) -> Result<(), PlacementError> {
        let jobs = provider.clone();
        self.insert(id.into(), config, provider, Some(jobs))
    }

    fn insert(
        &mut self,
        id: String,
        config: ProviderConfig,
        provider: Arc<dyn ComputeProvider>,
        jobs: Option<Arc<RemoteProvider>>,
    ) -> Result<(), PlacementError> {
        validate_provider_id(&id)?;
        config.validate(&id)?;
        if self.members.contains_key(&id) {
            return Err(PlacementError::InvalidConfig(format!(
                "provider {id} is already in the pool"
            )));
        }
        self.members.insert(
            id.clone(),
            PoolMember {
                id,
                config,
                provider,
                jobs,
            },
        );
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Option<PoolMember> {
        self.members.remove(id)
    }

    pub fn member(&self, id: &str) -> Option<&PoolMember> {
        self.members.get(id)
    }

    pub fn members(&self) -> impl Iterator<Item = &PoolMember> {
        self.members.values()
    }

    pub fn inspect(&self) -> PoolInspection {
        let mut providers = self
            .members
            .values()
            .map(|member| PoolMemberInspection {
                provider_id: member.id.clone(),
                kind: member.config.kind,
                endpoint: member.config.endpoint.clone(),
                application_endpoint: member.config.application_endpoint.clone(),
                priority: member.config.priority,
                authenticated: member.config.token_env.is_some(),
            })
            .collect::<Vec<_>>();
        providers.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.provider_id.cmp(&right.provider_id))
        });
        PoolInspection {
            policy: self.policy.clone(),
            providers,
        }
    }

    /// Discover capabilities of every member (or of `only`).
    pub async fn capabilities(
        &self,
        cache: &mut CapabilityCache,
        mode: DiscoveryMode,
        only: Option<&str>,
        now: DateTime<Utc>,
    ) -> Vec<DiscoveryRecord> {
        let mut records = vec![];
        for member in self.members.values() {
            if only.is_some_and(|id| id != member.id) {
                continue;
            }
            records.push(self.discover(member, cache, mode, now).await);
        }
        records
    }

    async fn discover(
        &self,
        member: &PoolMember,
        cache: &mut CapabilityCache,
        mode: DiscoveryMode,
        now: DateTime<Utc>,
    ) -> DiscoveryRecord {
        let fingerprint = member.config.fingerprint();
        if mode == DiscoveryMode::PreferCache
            && let Some(entry) = cache.entries.get(&member.id)
            && entry.fingerprint == fingerprint
        {
            let fresh = now < entry.expires_at;
            let availability = Availability {
                health: if fresh { entry.health } else { Health::Unknown },
                fetched_at: entry.fetched_at,
                expires_at: entry.expires_at,
            };
            // Cached capabilities are re-validated: the cache is input too.
            return match ProviderDescriptor::from_capabilities(
                &member.id,
                member.config.kind,
                &entry.capabilities,
                availability,
            ) {
                Ok(descriptor) => DiscoveryRecord {
                    provider_id: member.id.clone(),
                    status: if fresh {
                        DiscoveryStatus::Cached
                    } else {
                        DiscoveryStatus::Stale
                    },
                    descriptor: Some(descriptor),
                    error: None,
                },
                Err(error) => DiscoveryRecord::invalid(&member.id, error),
            };
        }

        let capabilities = match member.provider.capabilities().await {
            Ok(capabilities) => capabilities,
            Err(error) => {
                let malformed = error.kind == ProviderErrorKind::TransportFailure
                    && error.message.starts_with("malformed remote response");
                return if malformed {
                    DiscoveryRecord::invalid(
                        &member.id,
                        DescriptorError {
                            code: "provider_capabilities_invalid".into(),
                            field: "response".into(),
                            message: error.message,
                        },
                    )
                } else {
                    DiscoveryRecord {
                        provider_id: member.id.clone(),
                        status: DiscoveryStatus::Unavailable,
                        descriptor: None,
                        error: Some(DiscoveryError {
                            code: "provider_unavailable".into(),
                            message: error.to_string(),
                        }),
                    }
                };
            }
        };
        let health = match member.provider.health().await {
            Ok(value) if value.healthy => Health::Healthy,
            Ok(_) => Health::Unhealthy,
            Err(_) => Health::Unknown,
        };
        let expires_at = now + Duration::seconds(ttl_seconds(self.policy.capability_ttl_seconds));
        match ProviderDescriptor::from_capabilities(
            &member.id,
            member.config.kind,
            &capabilities,
            Availability {
                health,
                fetched_at: now,
                expires_at,
            },
        ) {
            Ok(descriptor) => {
                cache.entries.insert(
                    member.id.clone(),
                    CacheEntry {
                        fingerprint,
                        fetched_at: now,
                        expires_at,
                        health,
                        capabilities,
                    },
                );
                DiscoveryRecord {
                    provider_id: member.id.clone(),
                    status: DiscoveryStatus::Discovered,
                    descriptor: Some(descriptor),
                    error: None,
                }
            }
            Err(error) => {
                cache.entries.remove(&member.id);
                DiscoveryRecord::invalid(&member.id, error)
            }
        }
    }
}

fn ttl_seconds(value: u64) -> i64 {
    i64::try_from(value)
        .unwrap_or(i64::MAX / 1_000_000)
        .min(10 * 365 * 24 * 60 * 60)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryMode {
    /// Use fresh cached capabilities; report stale ones as stale; discover
    /// providers that have no cache entry.
    PreferCache,
    /// Discover every provider now.
    Refresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryStatus {
    Discovered,
    Cached,
    Stale,
    Invalid,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryRecord {
    pub provider_id: String,
    pub status: DiscoveryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<ProviderDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<DiscoveryError>,
}

impl DiscoveryRecord {
    fn invalid(provider_id: &str, error: DescriptorError) -> Self {
        Self {
            provider_id: provider_id.into(),
            status: DiscoveryStatus::Invalid,
            descriptor: None,
            error: Some(DiscoveryError {
                code: error.code.clone(),
                message: format!("{}: {}", error.field, error.message),
            }),
        }
    }

    pub fn health(&self) -> Health {
        self.descriptor
            .as_ref()
            .map_or(Health::Unknown, |descriptor| descriptor.availability.health)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheEntry {
    pub fingerprint: String,
    pub fetched_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub health: Health,
    pub capabilities: ProviderCapabilities,
}

/// Capability descriptors with explicit freshness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityCache {
    pub version: String,
    #[serde(default)]
    pub entries: BTreeMap<String, CacheEntry>,
}

impl Default for CapabilityCache {
    fn default() -> Self {
        Self {
            version: CAPABILITY_CACHE_VERSION.into(),
            entries: BTreeMap::new(),
        }
    }
}

impl CapabilityCache {
    /// Load a cache file. A missing file is an empty cache; an unreadable
    /// or foreign one is rejected rather than trusted.
    pub fn load(path: &Path) -> Result<Self, PlacementError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path).map_err(|error| {
            PlacementError::InvalidCache(format!("{}: {error}", path.display()))
        })?;
        let cache: Self = serde_json::from_slice(&bytes).map_err(|error| {
            PlacementError::InvalidCache(format!("{}: {error}", path.display()))
        })?;
        if cache.version != CAPABILITY_CACHE_VERSION {
            return Err(PlacementError::InvalidCache(format!(
                "unsupported cache version {}",
                cache.version
            )));
        }
        Ok(cache)
    }

    pub fn save(&self, path: &Path) -> Result<(), PlacementError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| PlacementError::InvalidCache(error.to_string()))?;
        }
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| PlacementError::InvalidCache(error.to_string()))?;
        bytes.push(b'\n');
        std::fs::write(path, bytes).map_err(|error| PlacementError::InvalidCache(error.to_string()))
    }
}
