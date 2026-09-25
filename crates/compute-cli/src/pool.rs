//! `compute provider`, `compute placement`, and `compute pool` commands.
//!
//! These commands evaluate a caller-owned provider pool and hand the
//! canonical workload to exactly one provider through the existing provider
//! contract. They never retry elsewhere.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use clap::{Args, Subcommand};
use compute_core::{
    CapacitySnapshot, ComputeError, DependencyCapsule, EnvironmentVariable, IsolationProfile,
    NetworkPolicy, PlatformIdentity, ProviderCapacity, ResourceRequirements, WorkloadBundle,
};
use compute_placement::{
    CapabilityCache, DiscoveryMode, DiscoveryRecord, PlacementOutcome, PlacementPolicy,
    PlacementReport, PlacementRequirements, PoolConfig, ProviderPool, RequirementOptions,
    SubmissionMode, dispatch, place_with_policy,
};
use compute_provider::{ComputeProvider, LocalProvider, ProviderRequest, RemoteProvider};

use crate::direct;

pub const DEFAULT_POOL_CONFIG: &str = "compute-pool.toml";
pub const DEFAULT_CAPABILITY_CACHE: &str = ".compute/provider-capabilities.json";
/// Exit status for a completed placement that selected no provider.
pub const PLACEMENT_FAILED_EXIT: i32 = 2;

/// Where the caller-owned pool configuration and capability cache live.
#[derive(Args, Debug, Clone)]
pub struct PoolLocation {
    /// Provider pool configuration (TOML). Defaults to $COMPUTE_POOL_CONFIG,
    /// then ./compute-pool.toml, then a pool containing only `local`.
    #[arg(long, global = true)]
    pub pool_config: Option<PathBuf>,
    /// Capability cache. Defaults to $COMPUTE_CAPABILITY_CACHE, then
    /// ./.compute/provider-capabilities.json.
    #[arg(long, global = true)]
    pub capability_cache: Option<PathBuf>,
}

impl PoolLocation {
    fn config(&self) -> compute_core::Result<PoolConfig> {
        let explicit = self
            .pool_config
            .clone()
            .or_else(|| std::env::var_os("COMPUTE_POOL_CONFIG").map(PathBuf::from));
        let path = match explicit {
            Some(path) => path,
            None if Path::new(DEFAULT_POOL_CONFIG).is_file() => DEFAULT_POOL_CONFIG.into(),
            None => return Ok(PoolConfig::local_only()),
        };
        let mut config = PoolConfig::load(&path).map_err(placement_error)?;
        if config.providers.is_empty() {
            config.providers = PoolConfig::local_only().providers;
        }
        Ok(config)
    }

    fn cache_path(&self) -> PathBuf {
        self.capability_cache
            .clone()
            .or_else(|| std::env::var_os("COMPUTE_CAPABILITY_CACHE").map(PathBuf::from))
            .unwrap_or_else(|| DEFAULT_CAPABILITY_CACHE.into())
    }

    pub(crate) fn pool(&self) -> compute_core::Result<ProviderPool> {
        ProviderPool::from_config(&self.config()?).map_err(placement_error)
    }

    /// Resolve a provider ID through the caller-owned pool.
    pub(crate) fn provider(&self, id: &str) -> compute_core::Result<Arc<dyn ComputeProvider>> {
        self.provider_with_jobs(id).map(|(provider, _)| provider)
    }

    pub(crate) fn provider_with_jobs(
        &self,
        id: &str,
    ) -> compute_core::Result<(Arc<dyn ComputeProvider>, Option<Arc<RemoteProvider>>)> {
        let pool = self.pool()?;
        let member = pool.member(id).ok_or_else(|| {
            ComputeError::InvalidWorkload(format!(
                "provider {id} is not configured in the caller-owned pool"
            ))
        })?;
        Ok((member.provider.clone(), member.jobs.clone()))
    }

    /// Resolve the durable-job transport for a configured remote provider.
    pub(crate) fn remote_provider(&self, id: &str) -> compute_core::Result<Arc<RemoteProvider>> {
        self.provider_with_jobs(id)?.1.ok_or_else(|| {
            ComputeError::InvalidWorkload(format!(
                "provider {id} does not support the remote job protocol"
            ))
        })
    }

    fn cache(&self) -> compute_core::Result<CapabilityCache> {
        CapabilityCache::load(&self.cache_path()).map_err(placement_error)
    }
}

#[derive(Args, Debug)]
pub struct ProviderCommand {
    #[command(subcommand)]
    pub command: ProviderCommands,
    #[command(flatten)]
    pub location: PoolLocation,
}

#[derive(Args, Debug)]
pub struct CapacityCommand {
    /// Restrict the view to one configured provider.
    #[arg(long)]
    pub provider: Option<String>,
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub location: PoolLocation,
}

#[derive(Args, Debug)]
pub struct JobsCommand {
    /// Restrict the view to one configured remote provider.
    #[arg(long)]
    pub provider: Option<String>,
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub location: PoolLocation,
}

#[derive(Subcommand, Debug)]
pub enum ProviderCommands {
    /// List configured providers with their discovery status and health.
    List {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        refresh: bool,
    },
    /// Show a provider's validated descriptor. A pool ID yields the
    /// canonical descriptor; `local` or an endpoint URL outside the pool
    /// yields the raw capability response.
    Inspect {
        #[arg(default_value = "local")]
        provider: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        refresh: bool,
    },
    /// Show a provider's raw capability response.
    Capabilities {
        #[arg(default_value = "local")]
        provider: String,
        #[arg(long)]
        json: bool,
    },
    /// Show the configured pool and its selection policy.
    Pool {
        #[arg(long)]
        json: bool,
    },
    /// Rediscover capabilities and rewrite the capability cache.
    Refresh {
        provider: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct PlacementCommand {
    #[command(subcommand)]
    pub command: PlacementCommands,
    #[command(flatten)]
    pub location: PoolLocation,
    #[command(flatten)]
    pub policy: crate::admission::PolicyLocation,
}

#[derive(Subcommand, Debug)]
pub enum PlacementCommands {
    /// Evaluate the pool for a workload. No execution occurs.
    Inspect(Box<PlacementArtifact>),
    /// Explain what the workload requires and why each provider is or is
    /// not compatible. No execution occurs.
    Explain(Box<PlacementArtifact>),
}

#[derive(Args, Debug)]
pub struct PoolCommand {
    #[command(subcommand)]
    pub command: PoolCommands,
    #[command(flatten)]
    pub location: PoolLocation,
    #[command(flatten)]
    pub policy: crate::admission::PolicyLocation,
}

#[derive(Subcommand, Debug)]
pub enum PoolCommands {
    /// Place and execute synchronously on the selected provider.
    Run(Box<PlacementArtifact>),
    /// Place and submit a durable job to the selected provider.
    Submit(Box<PlacementArtifact>),
}

#[derive(Args, Debug, Default)]
pub struct PlacementArtifact {
    #[arg(required_unless_present = "bundle", conflicts_with = "bundle")]
    pub path: Option<PathBuf>,
    #[arg(long)]
    pub bundle: Option<PathBuf>,
    /// Strict provider selection. The provider must be eligible; no fallback.
    #[arg(long)]
    pub provider: Option<String>,
    /// Placement preference policy: auto or local.
    #[arg(long = "policy", conflicts_with_all = ["provider", "prefer_provider"])]
    pub placement_policy: Option<String>,
    /// Prefer this eligible provider, but fall back when it is unavailable.
    #[arg(long, conflicts_with = "provider")]
    pub prefer_provider: Option<String>,
    /// Discover capabilities now instead of using cached descriptors.
    #[arg(long)]
    pub refresh: bool,
    /// Evaluate as a durable job submission (implied by `pool submit`).
    #[arg(long)]
    pub submit: bool,
    /// Require this exact distribution identity.
    #[arg(long)]
    pub distribution: Option<String>,
    /// Require this exact runtime artifact identity.
    #[arg(long)]
    pub runtime_artifact: Option<String>,
    /// Require this platform (`<os>-<architecture>`).
    #[arg(long, value_parser = parse_platform)]
    pub platform: Option<PlatformIdentity>,
    #[arg(long)]
    pub runtime: Option<String>,
    #[arg(long = "env", value_parser = crate::parse_env)]
    pub env: Vec<EnvironmentVariable>,
    #[arg(long)]
    pub env_file: Option<PathBuf>,
    #[arg(long = "input")]
    pub inputs: Vec<PathBuf>,
    #[arg(long = "output")]
    pub outputs: Vec<PathBuf>,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    #[arg(long)]
    pub entrypoint: Option<PathBuf>,
    #[arg(long)]
    pub deps: Option<PathBuf>,
    #[arg(long, value_parser = crate::parse_network)]
    pub network: Option<NetworkPolicy>,
    #[arg(long, value_parser = crate::parse_isolation)]
    pub isolation: Option<IsolationProfile>,
    #[arg(long, value_parser = crate::parse_memory)]
    pub memory: Option<u64>,
    #[arg(long, value_parser = crate::parse_duration)]
    pub timeout: Option<Duration>,
    /// Write the execution receipt here (`pool run`).
    #[arg(long)]
    pub receipt: Option<PathBuf>,
    /// Write the placement report here.
    #[arg(long)]
    pub placement_output: Option<PathBuf>,
    /// Prevent duplicate jobs when retrying a submission (`pool submit`).
    #[arg(long)]
    pub idempotency_key: Option<String>,
    #[arg(long)]
    pub json: bool,
    #[arg(last = true)]
    pub args: Vec<String>,
}

fn placement_error(error: compute_placement::PlacementError) -> ComputeError {
    ComputeError::InvalidWorkload(error.to_string())
}

fn parse_platform(value: &str) -> Result<PlatformIdentity, String> {
    compute_placement::parse_platform(value)
        .ok_or_else(|| "platform must be <os>-<architecture>".into())
}

fn print_json(value: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("placement values are serializable")
    );
}

pub async fn provider(command: ProviderCommand) -> compute_core::Result<()> {
    let location = command.location;
    match command.command {
        ProviderCommands::List { json, refresh } => {
            let pool = location.pool()?;
            let records = discover(&location, &pool, refresh, None).await?;
            let inspection = pool.inspect();
            if json {
                let providers = inspection
                    .providers
                    .iter()
                    .map(|member| {
                        let record = records
                            .iter()
                            .find(|record| record.provider_id == member.provider_id);
                        serde_json::json!({
                            "provider_id": member.provider_id,
                            "kind": member.kind,
                            "endpoint": member.endpoint,
                            "priority": member.priority,
                            "discovery": record.map(|record| record.status),
                            "health": record.map(DiscoveryRecord::health),
                            "capability_version": record
                                .and_then(|record| record.descriptor.as_ref())
                                .map(|descriptor| &descriptor.capability_version),
                            "runtimes": record
                                .and_then(|record| record.descriptor.as_ref())
                                .map(compute_placement::descriptor::runtime_summary),
                            "error": record.and_then(|record| record.error.as_ref()),
                        })
                    })
                    .collect::<Vec<_>>();
                print_json(&serde_json::json!({ "providers": providers }));
            } else {
                println!("Provider\tKind\tPriority\tDiscovery\tHealth\tEndpoint");
                for member in inspection.providers {
                    let record = records
                        .iter()
                        .find(|record| record.provider_id == member.provider_id);
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        member.provider_id,
                        member.kind,
                        member.priority,
                        record
                            .map(|record| enum_label(&record.status))
                            .unwrap_or_else(|| "-".into()),
                        record
                            .map(|record| record.health().to_string())
                            .unwrap_or_else(|| "-".into()),
                        member.endpoint.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        ProviderCommands::Inspect {
            provider,
            json,
            refresh,
        } => {
            let pool = location.pool()?;
            if pool.member(&provider).is_some() {
                let records = discover(&location, &pool, refresh, Some(&provider)).await?;
                let record = records.into_iter().next().expect("member was discovered");
                if json {
                    print_json(&record);
                } else {
                    print_record(&record);
                }
            } else {
                let capabilities = adhoc(&provider)?
                    .capabilities()
                    .await
                    .map_err(crate::provider_error)?;
                print_json(&capabilities);
            }
        }
        ProviderCommands::Capabilities { provider, json: _ } => {
            let pool = location.pool()?;
            let capabilities = match pool.member(&provider) {
                Some(member) => member.provider.capabilities().await,
                None => adhoc(&provider)?.capabilities().await,
            }
            .map_err(crate::provider_error)?;
            print_json(&capabilities);
        }
        ProviderCommands::Pool { json } => {
            let pool = location.pool()?;
            let inspection = pool.inspect();
            if json {
                print_json(&serde_json::json!({
                    "configuration": location
                        .pool_config
                        .clone()
                        .or_else(|| std::env::var_os("COMPUTE_POOL_CONFIG").map(PathBuf::from))
                        .or_else(|| Path::new(DEFAULT_POOL_CONFIG)
                            .is_file()
                            .then(|| PathBuf::from(DEFAULT_POOL_CONFIG))),
                    "capability_cache": location.cache_path(),
                    "selection_policy": compute_placement::SelectionPolicy::from_pool(&inspection.policy),
                    "capability_ttl_seconds": inspection.policy.capability_ttl_seconds,
                    "providers": inspection.providers,
                }));
            } else {
                let policy = &inspection.policy;
                println!("Selection: compatibility, then priority (descending), then provider ID");
                println!("Require healthy: {}", policy.require_healthy);
                println!("Capability TTL: {}s", policy.capability_ttl_seconds);
                println!(
                    "Allow stale capabilities: {}",
                    policy.allow_stale_capabilities
                );
                println!("Provider\tKind\tPriority\tEndpoint");
                for member in inspection.providers {
                    println!(
                        "{}\t{}\t{}\t{}",
                        member.provider_id,
                        member.kind,
                        member.priority,
                        member.endpoint.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        ProviderCommands::Refresh { provider, json } => {
            let pool = location.pool()?;
            if let Some(id) = &provider
                && pool.member(id).is_none()
            {
                return Err(ComputeError::InvalidWorkload(format!(
                    "provider {id} is not configured in this pool"
                )));
            }
            let records = discover(&location, &pool, true, provider.as_deref()).await?;
            if json {
                print_json(&serde_json::json!({ "providers": records }));
            } else {
                for record in &records {
                    println!(
                        "{}\t{}\t{}",
                        record.provider_id,
                        enum_label(&record.status),
                        record
                            .descriptor
                            .as_ref()
                            .map(|descriptor| descriptor.availability.expires_at.to_rfc3339())
                            .or_else(|| record.error.as_ref().map(|error| error.message.clone()))
                            .unwrap_or_default()
                    );
                }
            }
        }
    }
    Ok(())
}

pub async fn capacity(command: CapacityCommand) -> compute_core::Result<()> {
    let pool = command.location.pool()?;
    let inspection = pool.inspect();
    let mut rows = Vec::new();
    for member in inspection.providers {
        if command
            .provider
            .as_deref()
            .is_some_and(|wanted| wanted != member.provider_id)
        {
            continue;
        }
        let pool_member = pool
            .member(&member.provider_id)
            .expect("inspected member exists");
        let snapshot = match &pool_member.jobs {
            Some(remote) => remote
                .capacity_snapshot()
                .await
                .map_err(crate::provider_error)?,
            None => {
                let capabilities = pool_member
                    .provider
                    .capabilities()
                    .await
                    .map_err(crate::provider_error)?;
                let capacity = ProviderCapacity {
                    cpu_millis: capabilities
                        .resources
                        .capacity
                        .cpu_count
                        .saturating_mul(1_000),
                    memory_bytes: capabilities.resources.capacity.memory_bytes,
                    disk_bytes: capabilities.resources.capacity.disk_bytes,
                    max_concurrency: capabilities
                        .max_concurrent_jobs
                        .unwrap_or(1)
                        .try_into()
                        .unwrap_or(u32::MAX),
                };
                CapacitySnapshot {
                    available: ResourceRequirements {
                        cpu_millis: capacity.cpu_millis,
                        memory_bytes: capacity.memory_bytes,
                        disk_bytes: capacity.disk_bytes,
                        concurrency: capacity.max_concurrency,
                    },
                    reserved: ResourceRequirements::default(),
                    capacity,
                }
            }
        };
        rows.push(serde_json::json!({
            "provider_id": member.provider_id,
            "capacity": snapshot.capacity,
            "reserved": snapshot.reserved,
            "available": snapshot.available,
        }));
    }
    if command.provider.is_some() && rows.is_empty() {
        return Err(ComputeError::InvalidWorkload(format!(
            "provider {} is not configured in this pool",
            command.provider.as_deref().unwrap_or_default()
        )));
    }
    if command.json {
        print_json(&serde_json::json!({ "providers": rows }));
    } else {
        println!(
            "Provider\tCPU (available/capacity)\tMemory (available/capacity)\tDisk (available/capacity)\tJobs (available/capacity)"
        );
        for row in rows {
            let id = row["provider_id"].as_str().unwrap_or("-");
            let capacity: ProviderCapacity =
                serde_json::from_value(row["capacity"].clone()).expect("capacity serializes");
            let available: ResourceRequirements =
                serde_json::from_value(row["available"].clone()).expect("capacity serializes");
            println!(
                "{}\t{:.3}/{}\t{}/{}\t{}/{}\t{}/{}",
                id,
                available.cpu_millis as f64 / 1_000.0,
                capacity.cpu_millis as f64 / 1_000.0,
                format_bytes(available.memory_bytes),
                format_bytes(capacity.memory_bytes),
                format_bytes(available.disk_bytes),
                format_bytes(capacity.disk_bytes),
                available.concurrency,
                capacity.max_concurrency
            );
        }
    }
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB && bytes % GIB == 0 {
        format!("{} GiB", bytes / GIB)
    } else if bytes >= MIB && bytes % MIB == 0 {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} B")
    }
}

pub async fn jobs(command: JobsCommand) -> compute_core::Result<()> {
    let pool = command.location.pool()?;
    let inspection = pool.inspect();
    let mut rows = Vec::new();
    for member in inspection.providers {
        if command
            .provider
            .as_deref()
            .is_some_and(|wanted| wanted != member.provider_id)
        {
            continue;
        }
        let Some(remote) = &pool
            .member(&member.provider_id)
            .expect("member exists")
            .jobs
        else {
            continue;
        };
        for job in remote.jobs().await.map_err(crate::provider_error)? {
            rows.push(serde_json::json!({
                "provider_id": member.provider_id,
                "job": job,
            }));
        }
    }
    if command.json {
        print_json(&serde_json::json!({ "jobs": rows }));
    } else {
        println!("Provider\tJob\tStatus\tReservation\tResources");
        for row in rows {
            let job: compute_core::ExecutionJob =
                serde_json::from_value(row["job"].clone()).expect("job serializes");
            let reservation = job.reservation.as_ref();
            println!(
                "{}\t{}\t{:?}\t{}\t{}m CPU, {} bytes memory, {} bytes disk",
                row["provider_id"].as_str().unwrap_or("-"),
                job.job_id,
                job.status,
                reservation
                    .map(|value| format!("{}:{:?}", value.reservation_id, value.state))
                    .unwrap_or_else(|| "-".into()),
                reservation.map_or(0, |value| value.resources.cpu_millis),
                reservation.map_or(0, |value| value.resources.memory_bytes),
                reservation.map_or(0, |value| value.resources.disk_bytes),
            );
        }
    }
    Ok(())
}

fn adhoc(value: &str) -> compute_core::Result<Box<dyn ComputeProvider>> {
    if value == "local" {
        Ok(Box::new(LocalProvider::new()))
    } else if value.starts_with("http://") || value.starts_with("https://") {
        Ok(Box::new(RemoteProvider::new(value)))
    } else {
        Err(ComputeError::InvalidWorkload(format!(
            "provider {value} is not configured in this pool and is not an http(s) endpoint"
        )))
    }
}

pub(crate) async fn runtime_inventory(
    location: &PoolLocation,
    provider: &str,
) -> compute_core::Result<compute_core::RuntimeInventory> {
    let pool = location.pool()?;
    let capabilities = match pool.member(provider) {
        Some(member) => member.provider.capabilities().await,
        None => adhoc(provider)?.capabilities().await,
    }
    .map_err(crate::provider_error)?;
    Ok(capabilities.inventory)
}

/// Discover capabilities. `refresh` forces discovery and persists the
/// result to the capability cache.
async fn discover(
    location: &PoolLocation,
    pool: &ProviderPool,
    refresh: bool,
    only: Option<&str>,
) -> compute_core::Result<Vec<DiscoveryRecord>> {
    let mut cache = location.cache()?;
    let mode = if refresh {
        DiscoveryMode::Refresh
    } else {
        DiscoveryMode::PreferCache
    };
    let records = pool.capabilities(&mut cache, mode, only, Utc::now()).await;
    if refresh {
        cache
            .save(&location.cache_path())
            .map_err(placement_error)?;
    }
    Ok(records)
}

fn print_record(record: &DiscoveryRecord) {
    println!("Provider: {}", record.provider_id);
    println!("Discovery: {}", enum_label(&record.status));
    println!("Health: {}", record.health());
    if let Some(error) = &record.error {
        println!("Error: {}: {}", error.code, error.message);
    }
    let Some(descriptor) = &record.descriptor else {
        return;
    };
    println!("Kind: {}", descriptor.provider_kind);
    println!("Protocol: {}", descriptor.protocol_version);
    println!("Capability version: {}", descriptor.capability_version);
    println!(
        "Distribution: {} ({})",
        descriptor.distribution.id.as_deref().unwrap_or("unknown"),
        descriptor.distribution.platform.label()
    );
    println!(
        "Isolation: {}",
        join(
            descriptor
                .isolation_profiles
                .iter()
                .map(ToString::to_string)
        )
    );
    println!(
        "Network: {}",
        join(
            descriptor
                .network_capabilities
                .iter()
                .map(ToString::to_string)
        )
    );
    println!("Runtimes:");
    for runtime in &descriptor.runtimes {
        let status = enum_label(&runtime.lifecycle);
        println!(
            "  {} {} ({status}; distribution: {}; isolation: {}; network: {})",
            runtime.kind,
            runtime
                .effective_version()
                .lines()
                .next()
                .unwrap_or_default(),
            runtime
                .distribution
                .as_ref()
                .map(|distribution| distribution.id.as_str())
                .unwrap_or("host/installed"),
            join(runtime.isolation_profiles.iter().map(ToString::to_string)),
            join(runtime.network_policies.iter().map(ToString::to_string))
        );
    }
    if !descriptor.unavailable_runtimes.is_empty() {
        println!(
            "Unavailable runtimes: {}",
            join(
                descriptor
                    .unavailable_runtimes
                    .iter()
                    .map(ToString::to_string)
            )
        );
    }
    println!(
        "Artifacts: {} (max request {} bytes, max output {} bytes, jobs {})",
        join(descriptor.artifact_limits.modes.iter().cloned()),
        descriptor.artifact_limits.max_request_bytes,
        descriptor.artifact_limits.max_output_bytes,
        if descriptor.artifact_limits.jobs {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "Fetched: {}; expires: {}",
        descriptor.availability.fetched_at.to_rfc3339(),
        descriptor.availability.expires_at.to_rfc3339()
    );
}

fn join(values: impl Iterator<Item = String>) -> String {
    let values = values.collect::<Vec<_>>();
    if values.is_empty() {
        "-".into()
    } else {
        values.join(", ")
    }
}

pub(crate) fn enum_label(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Build the canonical bundle and provider request for a placement command.
pub(crate) fn prepare(
    artifact: &PlacementArtifact,
    policy: &crate::admission::PolicyLocation,
) -> compute_core::Result<(WorkloadBundle, ProviderRequest, direct::DirectPlacement)> {
    let (bundle, placement) = if let Some(path) = &artifact.bundle {
        if artifact.runtime.is_some()
            || !artifact.env.is_empty()
            || artifact.env_file.is_some()
            || !artifact.inputs.is_empty()
            || !artifact.outputs.is_empty()
            || artifact.cwd.is_some()
            || artifact.entrypoint.is_some()
            || artifact.deps.is_some()
            || artifact.network.is_some()
            || artifact.memory.is_some()
            || artifact.timeout.is_some()
            || !artifact.args.is_empty()
        {
            return Err(ComputeError::InvalidWorkload(
                "--bundle cannot be combined with direct execution overrides".into(),
            ));
        }
        (
            WorkloadBundle::read(path)?,
            direct::DirectPlacement::default(),
        )
    } else {
        let resolved = direct::resolve(direct::DirectOptions {
            path: artifact.path.clone().expect("required by clap"),
            runtime: artifact.runtime.clone(),
            args: artifact.args.clone(),
            env: artifact.env.clone(),
            env_file: artifact.env_file.clone(),
            inputs: artifact.inputs.clone(),
            outputs: artifact.outputs.clone(),
            cwd: artifact.cwd.clone(),
            entrypoint: artifact.entrypoint.clone(),
            deps: artifact.deps.clone(),
            network: artifact.network.clone(),
            isolation: artifact.isolation,
            memory: artifact.memory,
            timeout: artifact.timeout,
            defaults: policy.defaults()?,
        })?;
        let placement = resolved.placement.clone();
        (
            WorkloadBundle::create_from_with_capsule(
                resolved.workload,
                &resolved.root,
                resolved.dependency_capsule,
            )?,
            placement,
        )
    };
    let mut request = ProviderRequest::bundle(bundle.to_bytes()?);
    request.expected.workload_id = Some(bundle.workload_id()?);
    request.expected.bundle_id = Some(bundle.bundle_id()?);
    request.expected.dependency_id = bundle
        .dependency_capsule
        .as_ref()
        .map(DependencyCapsule::capsule_id)
        .transpose()?;
    Ok((bundle, request, placement))
}

pub(crate) async fn evaluate(
    location: &PoolLocation,
    policy: &crate::admission::PolicyLocation,
    artifact: &PlacementArtifact,
    submission: SubmissionMode,
) -> compute_core::Result<(ProviderPool, PlacementReport, ProviderRequest)> {
    let (bundle, mut request, configured_placement) = prepare(artifact, policy)?;
    // Placement pins these identities, so they are part of the request whose
    // exact size the requirements record.
    request.expected.distribution_id = artifact.distribution.clone();
    request.execution.isolation = Some(
        artifact
            .isolation
            .unwrap_or(bundle.workload.isolation.profile),
    );
    let request_bytes = serde_json::to_vec(&request)?.len() as u64;
    let requirements = PlacementRequirements::from_bundle(
        &bundle,
        request_bytes,
        submission,
        &RequirementOptions {
            isolation: artifact.isolation,
            distribution_id: artifact.distribution.clone(),
            runtime_artifact_id: artifact.runtime_artifact.clone(),
            platform: artifact.platform.clone(),
        },
    )
    .map_err(placement_error)?;
    let contract =
        compute_policy::ExecutionContract::from_bundle(&bundle, Some(requirements.isolation))
            .map_err(|error| ComputeError::InvalidWorkload(error.to_string()))?;
    let admission = compute_placement::AdmissionContext::new(&policy.sources()?, contract);
    let pool = location.pool()?;
    let placement_policy = parse_placement_policy(
        artifact.provider.as_deref(),
        artifact
            .placement_policy
            .as_deref()
            .or(configured_placement.policy.as_deref()),
        artifact
            .prefer_provider
            .as_deref()
            .or(configured_placement.prefer_provider.as_deref()),
    )?;
    let explicit = match &placement_policy {
        PlacementPolicy::Provider(id) => Some(id.as_str()),
        _ => None,
    };
    let only = explicit.filter(|id| pool.member(id).is_some());
    let records = if explicit.is_some() && only.is_none() {
        vec![]
    } else {
        discover(location, &pool, artifact.refresh, only).await?
    };
    let report = place_with_policy(
        &pool.configs(),
        pool.policy(),
        &records,
        &requirements,
        &admission,
        placement_policy,
    );
    if let Some(path) = &artifact.placement_output {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    }
    Ok((pool, report, request))
}

fn parse_placement_policy(
    provider: Option<&str>,
    policy: Option<&str>,
    prefer_provider: Option<&str>,
) -> compute_core::Result<PlacementPolicy> {
    // `--provider auto` asks placement to choose; `provider:<id>` and a bare
    // ID name one configured provider.
    if let Some(value) = provider.filter(|value| *value != "auto") {
        let id = value.strip_prefix("provider:").unwrap_or(value);
        compute_placement::validate_provider_id(id)
            .map_err(|error| ComputeError::InvalidWorkload(error.to_string()))?;
        return Ok(PlacementPolicy::Provider(id.to_owned()));
    }
    if let Some(id) = prefer_provider {
        compute_placement::validate_provider_id(id)
            .map_err(|error| ComputeError::InvalidWorkload(error.to_string()))?;
        return Ok(PlacementPolicy::PreferProvider(id.to_owned()));
    }
    match policy.unwrap_or("auto") {
        "auto" => Ok(PlacementPolicy::Auto),
        "local" => Ok(PlacementPolicy::PreferLocal),
        "remote" => Ok(PlacementPolicy::PreferRemote),
        value => Err(ComputeError::InvalidWorkload(format!(
            "unknown placement policy {value:?}; expected auto or local"
        ))),
    }
}

pub async fn placement(command: PlacementCommand) -> compute_core::Result<()> {
    let location = command.location;
    let policy = command.policy;
    let (artifact, explain) = match command.command {
        PlacementCommands::Inspect(artifact) => (artifact, false),
        PlacementCommands::Explain(artifact) => (artifact, true),
    };
    reject_execution_flags(&artifact)?;
    let submission = if artifact.submit {
        SubmissionMode::Job
    } else {
        SubmissionMode::Synchronous
    };
    let (_, report, _) = evaluate(&location, &policy, &artifact, submission).await?;
    match (explain, artifact.json) {
        (false, true) => print_json(&report),
        (false, false) => print_summary(&report),
        (true, true) => print_json(&serde_json::json!({
            "placement_id": report.placement_id,
            "outcome": report.outcome,
            "selection_mode": report.selection_mode,
            "selected_provider": report.selected.as_ref().map(|selected| &selected.provider_id),
            "explanation": report.explanation,
            "providers": report.providers.iter().map(|provider| serde_json::json!({
                "provider_id": provider.provider_id,
                "status": provider.status,
                "capacity_status": provider.capacity_status,
                "candidate": provider.candidate,
                "reasons": provider.reasons,
                "capacity_reasons": provider.capacity_reasons,
                "error": provider.error,
            })).collect::<Vec<_>>(),
        })),
        (true, false) => print_explanation(&report),
    }
    if report.outcome == PlacementOutcome::PlacementFailed {
        std::process::exit(PLACEMENT_FAILED_EXIT);
    }
    Ok(())
}

fn reject_execution_flags(artifact: &PlacementArtifact) -> compute_core::Result<()> {
    if artifact.receipt.is_some() || artifact.idempotency_key.is_some() {
        return Err(ComputeError::InvalidWorkload(
            "placement inspection does not execute; --receipt and --idempotency-key are not valid"
                .into(),
        ));
    }
    Ok(())
}

fn print_summary(report: &PlacementReport) {
    println!("Placement: {}", report.placement_id);
    println!("Outcome: {}", enum_label(&report.outcome));
    println!("Selection mode: {}", report.selection_mode);
    println!("Requirements:");
    for line in &report.explanation.requires {
        println!("  {line}");
    }
    println!("Providers evaluated:");
    for provider in &report.providers {
        let runtime = provider
            .runtime_lifecycle
            .as_ref()
            .map(enum_label)
            .unwrap_or_else(|| "-".into());
        println!(
            "  {}\t{}\tpriority {}\truntime {}\trank {}{}",
            provider.provider_id,
            provider.status.as_str(),
            provider.priority,
            runtime,
            provider
                .candidate
                .rank
                .map(|rank| rank.to_string())
                .unwrap_or_else(|| "-".into()),
            if provider.candidate.selected {
                "\tselected"
            } else {
                ""
            }
        );
        if !provider.candidate.reasons.is_empty() {
            println!("    policy: {}", provider.candidate.reasons.join(", "));
        }
        if provider.reasons.is_empty()
            && provider.status == compute_placement::EvaluationStatus::Compatible
        {
            println!("    ✓ entire workload contract");
        }
        for reason in &provider.reasons {
            println!(
                "    ✗ {}: {}",
                reason.code.as_str(),
                reason.detail.clone().unwrap_or_else(|| format!(
                    "requires {}, available {}",
                    reason.required, reason.available
                ))
            );
        }
        match provider.capacity_status {
            compute_placement::CapacityStatus::Available => {
                println!("    ✓ capacity currently available");
            }
            compute_placement::CapacityStatus::Unavailable => {
                for reason in &provider.capacity_reasons {
                    println!(
                        "    ⏳ {}: {}",
                        reason.code.as_str(),
                        reason
                            .detail
                            .as_deref()
                            .unwrap_or("temporarily unavailable")
                    );
                }
            }
            compute_placement::CapacityStatus::Unknown => {
                println!("    ? capacity unknown");
            }
        }
    }
    println!(
        "Compatible: {}",
        join(report.compatible_providers.iter().cloned())
    );
    println!(
        "Incompatible: {}",
        join(report.incompatible_providers.iter().cloned())
    );
    println!(
        "Capacity available: {}",
        join(report.capacity_available_providers.iter().cloned())
    );
    println!(
        "Capacity unavailable (will queue if selected): {}",
        join(report.capacity_unavailable_providers.iter().cloned())
    );
    println!(
        "Excluded: {}",
        join(report.excluded_providers.iter().cloned())
    );
    match (&report.selected, &report.failure) {
        (Some(selected), _) => println!("Selected provider: {}", selected.provider_id),
        (None, Some(failure)) => {
            println!("Placement failed: {}: {}", failure.code, failure.message)
        }
        (None, None) => println!("Placement failed"),
    }
}

fn print_explanation(report: &PlacementReport) {
    println!("What does this workload require?");
    for line in &report.explanation.requires {
        println!("  - {line}");
    }
    println!("\nWhich providers were considered, and why is each compatible or incompatible?");
    if report.explanation.considered.is_empty() {
        println!("  (none)");
    }
    for line in &report.explanation.considered {
        println!("  - {line}");
    }
    println!("\nWhy was the selected provider selected?");
    println!("  {}", report.explanation.selection);
    println!("\nPlacement: {}", report.placement_id);
}

pub async fn pool(command: PoolCommand) -> compute_core::Result<()> {
    let location = command.location;
    let policy = command.policy;
    match command.command {
        PoolCommands::Run(artifact) => {
            if artifact.idempotency_key.is_some() {
                return Err(ComputeError::InvalidWorkload(
                    "--idempotency-key is valid only for pool submit".into(),
                ));
            }
            let (pool, report, request) =
                evaluate(&location, &policy, &artifact, SubmissionMode::Synchronous).await?;
            if !placed(&report, artifact.json) {
                std::process::exit(PLACEMENT_FAILED_EXIT);
            }
            let selected = report.selected.as_ref().expect("placed");
            if !artifact.json {
                eprintln!("Providers:");
                for provider in &report.providers {
                    eprintln!(
                        "  {:<12} {}",
                        provider.provider_id,
                        provider.status.as_str()
                    );
                }
                eprintln!("Selected: {}", selected.provider_id);
                eprintln!(
                    "Runtime: {}{}",
                    report.requirements.runtime.kind,
                    report
                        .requirements
                        .runtime
                        .version
                        .as_deref()
                        .map(|version| format!(" {version}"))
                        .unwrap_or_default()
                );
                eprintln!("Isolation: {}", report.requirements.isolation);
                eprintln!("Network: {}", report.requirements.network);
            }
            let remote_jobs = pool
                .member(&selected.provider_id)
                .and_then(|member| member.jobs.clone());
            let (result, job_id) = if let Some(jobs) = remote_jobs {
                let submission = match dispatch::submit(&pool, &report, request, None).await {
                    Ok(submission) => submission,
                    Err(error) => return dispatch_failure(&error, artifact.json),
                };
                let job_id = submission.job.job_id;
                let result = wait_for_result(&jobs, &job_id.0).await?;
                let receipt = result.result.receipt.as_ref().ok_or_else(|| {
                    ComputeError::InvalidReceipt("the provider returned no receipt".into())
                })?;
                report
                    .verify_receipt(receipt)
                    .map_err(ComputeError::InvalidReceipt)?;
                (result.result, Some(job_id))
            } else {
                let response = match dispatch::execute(&pool, &report, request).await {
                    Ok(response) => response,
                    Err(error) => return dispatch_failure(&error, artifact.json),
                };
                (response.result, None)
            };
            if let Some(path) = &artifact.receipt {
                let receipt = result.receipt.as_ref().expect("verified by dispatch");
                std::fs::write(path, receipt.encoded_bytes()?)?;
            }
            if artifact.json {
                let mut value = serde_json::to_value(&result)?;
                if let Some(job_id) = &job_id {
                    value["job_id"] = serde_json::to_value(job_id)?;
                }
                value["placement"] = serde_json::to_value(&report)?;
                print_json(&value);
                if !matches!(result.status, compute_core::ExecutionStatus::Completed)
                    || result.exit_code.is_some_and(|code| code != 0)
                {
                    std::process::exit(result.exit_code.filter(|code| *code != 0).unwrap_or(1));
                }
            } else {
                if let Some(job_id) = &job_id {
                    crate::print_remote_execution_result(job_id, result, false, None)?;
                } else {
                    crate::print_execution_result(result, false, None)?;
                }
            }
        }
        PoolCommands::Submit(artifact) => {
            if artifact.receipt.is_some() {
                return Err(ComputeError::InvalidWorkload(
                    "--receipt is valid for pool run; fetch job receipts with remote receipt"
                        .into(),
                ));
            }
            let (pool, report, request) =
                evaluate(&location, &policy, &artifact, SubmissionMode::Job).await?;
            if !placed(&report, artifact.json) {
                std::process::exit(PLACEMENT_FAILED_EXIT);
            }
            let submission = match dispatch::submit(
                &pool,
                &report,
                request,
                artifact.idempotency_key.as_deref(),
            )
            .await
            {
                Ok(submission) => submission,
                Err(error) => return dispatch_failure(&error, artifact.json),
            };
            if artifact.json {
                print_json(&serde_json::json!({
                    "job_id": submission.job.job_id,
                    "status": submission.job.status,
                    "request_id": submission.job.request_id,
                    "placement_id": submission.placement_id,
                    "provider_id": submission.provider_id,
                    "provider": submission.provider_identity,
                    "endpoint": submission.endpoint,
                    "placement": report,
                }));
            } else {
                println!("Job: {}", submission.job.job_id);
                println!("Status: {}", enum_label(&submission.job.status));
                println!("Provider: {}", submission.provider_id);
                if let Some(endpoint) = &submission.endpoint {
                    println!("Endpoint: {endpoint}");
                }
                println!("Placement: {}", submission.placement_id);
            }
        }
    }
    Ok(())
}

async fn wait_for_result(
    provider: &RemoteProvider,
    job_id: &str,
) -> compute_core::Result<compute_core::JobResult> {
    let mut delay = Duration::from_millis(100);
    loop {
        let status = provider
            .job_status(job_id)
            .await
            .map_err(crate::provider_error)?;
        if status.status.is_terminal() {
            return provider
                .job_result(job_id)
                .await
                .map_err(crate::provider_error);
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}

/// Report a failed placement. Returns whether a provider was selected.
fn placed(report: &PlacementReport, json: bool) -> bool {
    if report.outcome == PlacementOutcome::Placed {
        return true;
    }
    if json {
        print_json(&serde_json::json!({ "placement": report }));
    }
    let failure = report.failure.as_ref();
    eprintln!(
        "placement_failed: {}: {}; nothing was executed",
        failure.map_or("", |failure| failure.code.as_str()),
        failure.map_or("", |failure| failure.message.as_str())
    );
    for provider in &report.providers {
        eprintln!(
            "provider {}: {}",
            provider.provider_id,
            provider.status.as_str()
        );
        for reason in &provider.reasons {
            eprintln!(
                "  {}: required {}, available {}{}",
                reason.code.as_str(),
                reason.required,
                reason.available,
                reason
                    .detail
                    .as_deref()
                    .map(|detail| format!(" ({detail})"))
                    .unwrap_or_default()
            );
        }
        for reason in &provider.capacity_reasons {
            eprintln!(
                "  temporary {}: required {}, available {}{}",
                reason.code.as_str(),
                reason.required,
                reason.available,
                reason
                    .detail
                    .as_deref()
                    .map(|detail| format!(" ({detail})"))
                    .unwrap_or_default()
            );
        }
    }
    false
}

fn dispatch_failure(
    error: &compute_placement::DispatchError,
    json: bool,
) -> compute_core::Result<()> {
    if json {
        print_json(&serde_json::json!({ "error": error }));
    }
    Err(ComputeError::Runtime(format!(
        "{error} (placement {}, provider {}, not retried on another provider)",
        error.placement_id,
        error.provider_id.as_deref().unwrap_or("-")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_accepts_auto_a_prefixed_id_or_a_bare_id() {
        assert_eq!(
            parse_placement_policy(Some("auto"), None, None).unwrap(),
            PlacementPolicy::Auto
        );
        assert_eq!(
            parse_placement_policy(Some("provider:production"), None, None).unwrap(),
            PlacementPolicy::Provider("production".into())
        );
        assert_eq!(
            parse_placement_policy(Some("linux-worker"), None, None).unwrap(),
            PlacementPolicy::Provider("linux-worker".into())
        );
        assert!(parse_placement_policy(Some("provider:"), None, None).is_err());
    }
}
