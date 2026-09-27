//! `compute session`: a temporary, authorized, durable computer.
//!
//! `create` places the session through the caller-owned pool exactly as a
//! job is placed, then hands it to the one provider placement selected.
//! Every other command addresses the session by its ID: the provider that
//! holds it is found by asking the pool, so callers never need to know which
//! provider supplied the machine.

use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Subcommand};
use compute_core::{
    ComputeError, ComputeSession, EnvironmentVariable, IsolationProfile, NetworkPolicy,
    SessionCommand as Command, SessionEndpointRequest, SessionLogs, SessionResources, SessionSpec,
    SessionStatus,
};
use compute_placement::{
    PlacementOutcome, PlacementRequirements, RequirementOptions, dispatch, place_with_policy,
};
use compute_provider::{
    ProviderErrorKind, RemoteProvider, SessionCreateRequest, SessionEnvironmentSpec,
};

use crate::admission::PolicyLocation;
use crate::pool::{self, PoolLocation};

#[derive(Args, Debug)]
pub struct SessionCommand {
    #[command(subcommand)]
    command: SessionCommands,
    #[command(flatten)]
    location: PoolLocation,
    #[command(flatten)]
    policy: PolicyLocation,
}

#[derive(Subcommand, Debug)]
enum SessionCommands {
    /// Create a session on a provider chosen by placement.
    Create(CreateArgs),
    /// List your sessions on every provider in the pool.
    List(ListArgs),
    /// Show a session's complete, authoritative state.
    #[command(alias = "inspect")]
    Info(TargetArgs),
    /// Get connection details for a session.
    Connect(TargetArgs),
    /// Run a command in a session as a durable job.
    Exec(ExecArgs),
    /// Show the output of every execution in a session.
    Logs(TargetArgs),
    /// Stop active executions, keeping the environment and the record.
    Stop(TargetArgs),
    /// Resume a stopped session's environment. Never creates a new one.
    Resume(TargetArgs),
    /// Keep an ephemeral session until it is destroyed.
    Claim(TargetArgs),
    /// Tear the environment down. The record remains as evidence.
    Destroy(TargetArgs),
    /// Start working, through the Compute daemon: enter an environment you
    /// have (it keeps running when you close), or with no environment, get
    /// a temporary one of your own (destroyed when you close or it expires).
    Open(crate::work_cmd::OpenArgs),
    /// Stop working. Only a session's own temporary environment goes with it.
    Close(crate::work_cmd::CloseArgs),
    /// Your work sessions, through the Compute daemon.
    Opened(crate::work_cmd::OpenedArgs),
}

#[derive(Args, Debug)]
struct CreateArgs {
    /// Logical CPUs the environment needs.
    #[arg(long)]
    cpu: Option<u32>,
    /// Memory the environment needs (512Mi, 2Gi, ...).
    #[arg(long, value_parser = crate::parse_memory)]
    memory: Option<u64>,
    /// Scratch disk the environment needs.
    #[arg(long, value_parser = crate::parse_memory)]
    disk: Option<u64>,
    /// How long the session lives unless claimed (30m, 1h, 2d).
    #[arg(long, default_value = "1h", value_parser = crate::parse_retention)]
    ttl: Duration,
    /// Network the environment may use. Process environments enforce only
    /// `network`; a narrower policy needs a provider that can enforce it.
    #[arg(long, default_value = "network", value_parser = crate::parse_network)]
    network: NetworkPolicy,
    #[arg(long, value_parser = crate::parse_isolation)]
    isolation: Option<IsolationProfile>,
    /// A capability the environment must offer (repeatable): exec,
    /// terminal, filesystem, network, public_endpoint, persistent_storage,
    /// suspend, resume, claim.
    #[arg(long = "require")]
    require: Vec<String>,
    /// Expose a port: `PORT`, `PORT/PROTOCOL`, with `:public` for a public
    /// endpoint. Each one is authorized separately.
    #[arg(long = "expose", value_parser = parse_endpoint)]
    expose: Vec<SessionEndpointRequest>,
    /// Strict provider selection. The provider must be eligible.
    #[arg(long)]
    provider: Option<String>,
    /// Placement preference policy: auto, local, or remote.
    #[arg(long = "policy", conflicts_with_all = ["provider", "prefer_provider"])]
    placement_policy: Option<String>,
    /// Prefer this eligible provider, but fall back when it is unavailable.
    #[arg(long, conflicts_with = "provider")]
    prefer_provider: Option<String>,
    /// Discover capabilities now instead of using cached descriptors.
    #[arg(long)]
    refresh: bool,
    /// Wait until the session is ready (or has failed).
    #[arg(long)]
    wait: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Only this provider.
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct TargetArgs {
    session_id: String,
    /// The provider that holds the session. Found through the pool when
    /// omitted.
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ExecArgs {
    session_id: String,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long = "env", value_parser = crate::parse_env)]
    env: Vec<EnvironmentVariable>,
    /// Wall-time limit for this command.
    #[arg(long, value_parser = crate::parse_duration)]
    timeout: Option<Duration>,
    /// Return the durable job and execution IDs without waiting.
    #[arg(long)]
    detach: bool,
    /// Write the execution receipt here.
    #[arg(long)]
    receipt: Option<std::path::PathBuf>,
    #[arg(long)]
    json: bool,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

fn parse_endpoint(value: &str) -> Result<SessionEndpointRequest, String> {
    let (value, public) = match value.strip_suffix(":public") {
        Some(value) => (value, true),
        None => (value, false),
    };
    let (port, protocol) = value.split_once('/').unwrap_or((value, "http"));
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| format!("expected PORT[/PROTOCOL][:public], found {value:?}"))?;
    Ok(SessionEndpointRequest {
        port,
        protocol: protocol.to_owned(),
        public,
    })
}

pub async fn session(command: SessionCommand) -> compute_core::Result<()> {
    let location = command.location;
    match command.command {
        SessionCommands::Open(args) => crate::work_cmd::open(args).await,
        SessionCommands::Close(args) => crate::work_cmd::close(args).await,
        SessionCommands::Opened(args) => crate::work_cmd::opened(args).await,
        SessionCommands::Create(args) => create(&location, &command.policy, args).await,
        SessionCommands::List(args) => list(&location, args).await,
        SessionCommands::Info(args) => {
            let (_, provider, session) = locate(&location, &args).await?;
            if args.json {
                pool::print_json(&session);
            } else {
                print_session(&provider, &session);
            }
            Ok(())
        }
        SessionCommands::Connect(args) => {
            let (remote, _, _) = locate(&location, &args).await?;
            let grant = remote
                .connect_session(&args.session_id)
                .await
                .map_err(session_error)?;
            if args.json {
                pool::print_json(&grant);
                return Ok(());
            }
            println!("Session:     {}", grant.session_id);
            println!("Mode:        {}", grant.connection.mode.as_str());
            if let Some(address) = &grant.connection.address {
                println!(
                    "Address:     {address}{}",
                    grant
                        .connection
                        .port
                        .map(|port| format!(":{port}"))
                        .unwrap_or_default()
                );
            }
            for (key, value) in &grant.connection.details {
                println!("{key}: {value}");
            }
            if !grant.command.is_empty() {
                println!("Command:     {}", grant.command.join(" "));
            }
            if !grant.credentials.is_empty() {
                // Short-lived credentials go to the caller only on request.
                println!("Credentials: issued (use --json to read them)");
            }
            if let Some(expires_at) = grant.expires_at {
                println!("Expires:     {}", expires_at.to_rfc3339());
            }
            Ok(())
        }
        SessionCommands::Exec(args) => exec(&location, args).await,
        SessionCommands::Logs(args) => {
            let (remote, _, _) = locate(&location, &args).await?;
            let logs = remote
                .session_logs(&args.session_id)
                .await
                .map_err(session_error)?;
            if args.json {
                pool::print_json(&logs);
            } else {
                print_logs(&logs);
            }
            Ok(())
        }
        SessionCommands::Stop(args) => {
            lifecycle(&location, &args, |remote, id| async move {
                remote.stop_session(&id).await
            })
            .await
        }
        SessionCommands::Resume(args) => {
            lifecycle(&location, &args, |remote, id| async move {
                remote.resume_session(&id).await
            })
            .await
        }
        SessionCommands::Claim(args) => {
            lifecycle(&location, &args, |remote, id| async move {
                remote.claim_session(&id).await
            })
            .await
        }
        SessionCommands::Destroy(args) => {
            lifecycle(&location, &args, |remote, id| async move {
                remote.destroy_session(&id).await
            })
            .await
        }
    }
}

async fn create(
    location: &PoolLocation,
    policy: &PolicyLocation,
    args: CreateArgs,
) -> compute_core::Result<()> {
    let environment = SessionEnvironmentSpec {
        resources: SessionResources {
            cpu_count: args.cpu,
            memory_bytes: args.memory,
            disk_bytes: args.disk,
        },
        network: args.network.clone(),
        isolation: args.isolation.unwrap_or_default(),
        architecture: None,
    };
    let spec = SessionSpec {
        ttl_seconds: Some(args.ttl.as_secs().max(1)),
        required_capabilities: args.require.clone(),
        endpoints: args.expose.clone(),
        ..SessionSpec::default()
    };
    let create = SessionCreateRequest::new(&environment, spec).map_err(session_error)?;
    let bundle = create.environment().map_err(session_error)?;
    // The session goes through the same placement as any workload: its
    // contract, its capabilities, the caller's policy, provider availability.
    let request_bytes = serde_json::to_vec(&create)?.len() as u64;
    let requirements = PlacementRequirements::for_session(
        &bundle,
        request_bytes,
        &RequirementOptions {
            isolation: args.isolation,
            ..RequirementOptions::default()
        },
        &args.require,
    )
    .map_err(pool::placement_error)?;
    let contract =
        compute_policy::ExecutionContract::from_bundle(&bundle, Some(requirements.isolation))
            .map_err(|error| ComputeError::InvalidWorkload(error.to_string()))?;
    let admission = compute_placement::AdmissionContext::new(&policy.sources()?, contract);
    let pool = location.pool()?;
    let placement_policy = pool::parse_placement_policy(
        args.provider.as_deref(),
        args.placement_policy.as_deref(),
        args.prefer_provider.as_deref(),
    )?;
    let explicit = match &placement_policy {
        compute_placement::PlacementPolicy::Provider(id) => Some(id.clone()),
        _ => None,
    };
    let only = explicit.as_deref().filter(|id| pool.member(id).is_some());
    let records = if explicit.is_some() && only.is_none() {
        vec![]
    } else {
        pool::discover(location, &pool, args.refresh, only).await?
    };
    let report = place_with_policy(
        &pool.configs(),
        pool.policy(),
        &records,
        &requirements,
        &admission,
        placement_policy,
    );
    if !pool::placed(&report, args.json) {
        std::process::exit(pool::PLACEMENT_FAILED_EXIT);
    }
    debug_assert_eq!(report.outcome, PlacementOutcome::Placed);
    let placed = match dispatch::create_session(&pool, &report, create).await {
        Ok(placed) => placed,
        Err(error) => return pool::dispatch_failure(&error, args.json),
    };
    let remote = location.remote_provider(&placed.provider_id)?;
    let session = if args.wait {
        wait_until_settled(&remote, &placed.session.session_id.0).await?
    } else {
        placed.session.clone()
    };
    if args.json {
        pool::print_json(&serde_json::json!({
            "placement_id": placed.placement_id,
            "provider_id": placed.provider_id,
            "session": session,
        }));
    } else {
        eprintln!("Placed on: {}", placed.provider_id);
        print_session(&placed.provider_id, &session);
    }
    if session.status == SessionStatus::Failed {
        std::process::exit(1);
    }
    Ok(())
}

async fn wait_until_settled(
    remote: &RemoteProvider,
    session_id: &str,
) -> compute_core::Result<ComputeSession> {
    let mut delay = Duration::from_millis(50);
    loop {
        let session = remote.session(session_id).await.map_err(session_error)?;
        if matches!(
            session.status,
            SessionStatus::Ready | SessionStatus::Running
        ) || session.status.is_terminal()
        {
            return Ok(session);
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(1));
    }
}

async fn list(location: &PoolLocation, args: ListArgs) -> compute_core::Result<()> {
    let mut sessions = vec![];
    for (id, remote) in candidates(location, args.provider.as_deref())? {
        match remote.sessions().await {
            Ok(found) => sessions.extend(found.into_iter().map(|session| (id.clone(), session))),
            Err(error) if args.provider.is_some() => return Err(session_error(error)),
            // A provider that does not host sessions has none of yours.
            Err(_) => {}
        }
    }
    if args.json {
        pool::print_json(
            &sessions
                .iter()
                .map(|(provider, session)| {
                    serde_json::json!({ "provider_id": provider, "session": session })
                })
                .collect::<Vec<_>>(),
        );
        return Ok(());
    }
    if sessions.is_empty() {
        println!("No sessions.");
        return Ok(());
    }
    println!(
        "{:<24} {:<12} {:<12} {:<26} EXPIRES",
        "SESSION", "STATUS", "PROVIDER", "CREATED"
    );
    for (provider, session) in &sessions {
        println!(
            "{:<24} {:<12} {:<12} {:<26} {}",
            short(&session.session_id.0),
            session.status.as_str(),
            provider,
            session.created_at.format("%Y-%m-%dT%H:%M:%SZ"),
            session
                .expires_at
                .map(|at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_else(|| "never (claimed)".into())
        );
    }
    Ok(())
}

async fn exec(location: &PoolLocation, args: ExecArgs) -> compute_core::Result<()> {
    let target = TargetArgs {
        session_id: args.session_id.clone(),
        provider: args.provider.clone(),
        json: args.json,
    };
    let (remote, _, _) = locate(location, &target).await?;
    let mut command = Command::new(args.command.clone());
    command.env = args
        .env
        .iter()
        .map(|pair| (pair.key.clone(), pair.value.clone()))
        .collect();
    command.timeout = args.timeout;
    let submission = remote
        .session_exec(&args.session_id, &command)
        .await
        .map_err(session_error)?;
    if args.detach {
        pool::print_json(&submission);
        return Ok(());
    }
    // The same durable-job wait and evidence handling as `remote run`.
    crate::finish_remote_job(
        &remote,
        &submission.job_id,
        args.json,
        args.receipt.as_deref(),
    )
    .await
}

async fn lifecycle<F, Fut>(
    location: &PoolLocation,
    args: &TargetArgs,
    operation: F,
) -> compute_core::Result<()>
where
    F: FnOnce(Arc<RemoteProvider>, String) -> Fut,
    Fut: std::future::Future<Output = Result<ComputeSession, compute_provider::ProviderError>>,
{
    let (remote, provider, _) = locate(location, args).await?;
    let session = operation(remote, args.session_id.clone())
        .await
        .map_err(session_error)?;
    if args.json {
        pool::print_json(&session);
    } else {
        print_session(&provider, &session);
    }
    Ok(())
}

/// Remote providers in the pool, or the one named.
fn candidates(
    location: &PoolLocation,
    provider: Option<&str>,
) -> compute_core::Result<Vec<(String, Arc<RemoteProvider>)>> {
    if let Some(id) = provider {
        return Ok(vec![(id.to_owned(), location.remote_provider(id)?)]);
    }
    let pool = location.pool()?;
    Ok(pool
        .members()
        .filter_map(|member| {
            member
                .jobs
                .clone()
                .map(|remote| (member.id.clone(), remote))
        })
        .collect())
}

/// Find the provider that holds a session by asking the pool. Only the
/// provider that holds it, for this principal, answers.
async fn locate(
    location: &PoolLocation,
    args: &TargetArgs,
) -> compute_core::Result<(Arc<RemoteProvider>, String, ComputeSession)> {
    compute_core::SessionId::parse(args.session_id.clone())?;
    let mut found = vec![];
    let mut last_error = None;
    for (id, remote) in candidates(location, args.provider.as_deref())? {
        match remote.session(&args.session_id).await {
            Ok(session) => found.push((remote, id, session)),
            Err(error) => last_error = Some(error),
        }
    }
    match found.len() {
        1 => Ok(found.pop().expect("one")),
        0 => Err(match (args.provider.as_deref(), last_error) {
            (Some(_), Some(error)) => session_error(error),
            _ => ComputeError::InvalidWorkload(format!(
                "session {} is not held for you by any provider in the pool",
                args.session_id
            )),
        }),
        _ => Err(ComputeError::InvalidWorkload(format!(
            "more than one provider answers for session {}; name one with --provider",
            args.session_id
        ))),
    }
}

fn session_error(error: compute_provider::ProviderError) -> ComputeError {
    let code = serde_json::to_value(error.kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    match error.kind {
        ProviderErrorKind::UnknownSession
        | ProviderErrorKind::SessionConflict
        | ProviderErrorKind::OperationUnsupported
        | ProviderErrorKind::Unauthorized
        | ProviderErrorKind::AdmissionDenied => {
            ComputeError::InvalidWorkload(format!("{code}: {}", error.message))
        }
        _ => ComputeError::Runtime(format!("{code}: {}", error.message)),
    }
}

fn short(value: &str) -> String {
    if value.len() > 20 {
        format!("{}…", &value[..20])
    } else {
        value.to_owned()
    }
}

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB && bytes.is_multiple_of(GIB) {
        format!("{} GiB", bytes / GIB)
    } else if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// The whole lifecycle, from one command.
fn print_session(provider: &str, session: &ComputeSession) {
    let yes = |value: bool| if value { "yes" } else { "no" };
    println!("Session:     {}", session.session_id);
    println!("Status:      {}", session.status);
    println!("Node:        {provider}");
    println!(
        "Provider:    {} ({})",
        session.provider_kind,
        match &session.provider {
            compute_core::ProviderIdentity::Local { id } => id.clone(),
            compute_core::ProviderIdentity::Remote { endpoint, .. } => endpoint.clone(),
        }
    );
    println!("Job:         {}", session.job_id);
    println!("Execution:   {}", session.execution_id);
    println!(
        "Ownership:   {}",
        match session.ownership {
            compute_core::SessionOwnership::Ephemeral => "ephemeral",
            compute_core::SessionOwnership::Claimed => "claimed",
        }
    );
    if let Some(placement) = &session.placement_id {
        println!("Placement:   {placement}");
    }
    println!();
    println!("Resources:");
    println!(
        "  CPU:        {}",
        session
            .resources
            .cpu_count
            .map_or_else(|| "unspecified".into(), |cpu| cpu.to_string())
    );
    println!(
        "  Memory:     {}",
        session
            .resources
            .memory_bytes
            .map_or_else(|| "unspecified".into(), format_bytes)
    );
    if let Some(disk) = session.resources.disk_bytes {
        println!("  Disk:       {}", format_bytes(disk));
    }
    println!("  Network:    {}", session.network);
    println!();
    println!("Capabilities:");
    for (name, present) in session.capabilities.entries() {
        println!("  {name:<20} {}", yes(present));
    }
    println!();
    if let Some(connection) = &session.connection {
        println!(
            "Connection:  {}{}",
            connection.mode.as_str(),
            connection
                .address
                .as_deref()
                .map(|address| format!(" {address}"))
                .unwrap_or_default()
        );
    }
    println!("Created:     {}", session.created_at.to_rfc3339());
    if let Some(ready_at) = session.ready_at {
        println!("Ready:       {}", ready_at.to_rfc3339());
    }
    println!(
        "Expires:     {}",
        session
            .expires_at
            .map_or_else(|| "never (claimed)".into(), |at| at.to_rfc3339())
    );
    if let Some(ended_at) = session.ended_at {
        println!("Ended:       {}", ended_at.to_rfc3339());
    }
    if !session.endpoints.is_empty() {
        println!();
        println!("Endpoints:");
        for endpoint in &session.endpoints {
            println!(
                "  {} {}://{}:{}{}",
                endpoint.id,
                endpoint.protocol,
                endpoint.address,
                endpoint.port,
                if endpoint.public { " (public)" } else { "" }
            );
        }
    }
    let active = session.active_executions().count();
    println!();
    println!(
        "Executions:  {} ({active} active)",
        session.executions.len()
    );
    for execution in session.executions.iter().rev().take(5) {
        println!(
            "  {} {} {:?} {}",
            execution.job_id,
            execution.purpose,
            execution.status,
            execution.command.join(" ")
        );
    }
    if let Some(failure) = &session.failure {
        println!();
        println!("Failure:");
        println!("  Phase:      {}", failure.phase.as_str());
        if let Some(provider) = &failure.provider {
            println!("  Provider:   {provider}");
        }
        println!("  Code:       {}", failure.code);
        println!("  Retryable:  {}", yes(failure.retryable));
        println!("  Message:    {}", failure.message);
    }
}

fn print_logs(logs: &SessionLogs) {
    for execution in &logs.executions {
        println!(
            "== {} {} [{}] {:?}{}",
            execution.job_id,
            execution.execution_id,
            execution.purpose,
            execution.status,
            if execution.command.is_empty() {
                String::new()
            } else {
                format!(": {}", execution.command.join(" "))
            }
        );
        print!("{}", execution.stdout);
        if !execution.stderr.is_empty() {
            eprint!("{}", execution.stderr);
        }
    }
    if let Some(environment) = &logs.environment {
        println!("== environment");
        print!("{environment}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_parse_port_protocol_and_visibility() {
        assert_eq!(
            parse_endpoint("8080").unwrap(),
            SessionEndpointRequest {
                port: 8080,
                protocol: "http".into(),
                public: false
            }
        );
        assert_eq!(
            parse_endpoint("5432/tcp:public").unwrap(),
            SessionEndpointRequest {
                port: 5432,
                protocol: "tcp".into(),
                public: true
            }
        );
        assert!(parse_endpoint("0").is_err());
        assert!(parse_endpoint("http").is_err());
    }

    #[test]
    fn memory_takes_binary_suffixes() {
        assert_eq!(crate::parse_memory("2Gi").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(crate::parse_memory("512Mi").unwrap(), 512 * 1024 * 1024);
        assert_eq!(
            crate::parse_retention("1h").unwrap(),
            Duration::from_secs(3600)
        );
        assert_eq!(
            crate::parse_retention("60m").unwrap(),
            Duration::from_secs(3600)
        );
    }
}
