//! `compute session open | close | opened`: work sessions, through the
//! Compute daemon. A session is a way into an environment's computer, never
//! the environment itself:
//!
//! ```text
//! compute session open myapp          enter myapp; closing leaves it running
//! compute session open --cpu 2        a temporary environment of your own;
//!                                     closing (or its TTL) destroys it
//! ```

use std::time::Duration;

use clap::Args;
use compute_core::{ComputerLifecycle, ComputerRequirements, NetworkPolicy};
use compute_environment::{ComputerRequest, OpenSessionRequest, WorkSessionView};

use crate::environment_cmd::{DaemonLocation, error, parse_pair, print_json};

#[derive(Args, Debug)]
pub struct OpenArgs {
    /// The environment to enter. Without one, a temporary environment is
    /// made for the session.
    environment: Option<String>,
    #[arg(long, conflicts_with = "environment")]
    cpu: Option<u32>,
    #[arg(long, value_parser = crate::parse_memory, conflicts_with = "environment")]
    memory: Option<u64>,
    /// A machine feature the target must have (repeatable).
    #[arg(long = "feature", conflicts_with = "environment")]
    features: Vec<String>,
    /// The temporary environment's lifetime (default 1h).
    #[arg(long, value_parser = crate::parse_retention, conflicts_with = "environment")]
    ttl: Option<Duration>,
    #[arg(long, conflicts_with = "environment")]
    target: Option<String>,
    #[arg(long = "set", value_parser = parse_pair, conflicts_with = "environment")]
    env: Vec<(String, String)>,
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    daemon: DaemonLocation,
}

#[derive(Args, Debug)]
pub struct CloseArgs {
    session: String,
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    daemon: DaemonLocation,
}

#[derive(Args, Debug)]
pub struct OpenedArgs {
    /// Only sessions in this environment.
    #[arg(long)]
    environment: Option<String>,
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    daemon: DaemonLocation,
}

pub async fn open(args: OpenArgs) -> compute_core::Result<()> {
    let client = args.daemon.client()?;
    let request = match &args.environment {
        Some(environment) => OpenSessionRequest {
            environment: Some(environment.clone()),
            ..Default::default()
        },
        None => OpenSessionRequest {
            computer: Some(ComputerRequest {
                lifecycle: ComputerLifecycle::Ephemeral,
                requirements: ComputerRequirements {
                    cpu_count: args.cpu,
                    memory_bytes: args.memory,
                    network: NetworkPolicy::Network,
                    features: args.features.clone(),
                    ..Default::default()
                },
                target: args.target.clone(),
                ttl_seconds: args.ttl.map(|ttl| ttl.as_secs().max(1)),
            }),
            env: args.env.iter().cloned().collect(),
            ..Default::default()
        },
    };
    let session: WorkSessionView = client
        .post("/sessions", Some(&request))
        .await
        .map_err(error)?;
    print_session(&session, args.json);
    Ok(())
}

pub async fn close(args: CloseArgs) -> compute_core::Result<()> {
    let client = args.daemon.client()?;
    let session: WorkSessionView = client
        .delete(&format!("/sessions/{}", args.session))
        .await
        .map_err(error)?;
    print_session(&session, args.json);
    Ok(())
}

pub async fn opened(args: OpenedArgs) -> compute_core::Result<()> {
    let client = args.daemon.client()?;
    let mut path = "/sessions".to_owned();
    if let Some(environment) = &args.environment {
        path.push_str(&format!("?environment={environment}"));
    }
    let sessions: Vec<WorkSessionView> = client.get(&path).await.map_err(error)?;
    if args.json {
        print_json(&sessions);
        return Ok(());
    }
    for session in sessions {
        println!(
            "{:<30} {:<24} {:<10} {:<7} {}",
            session.session_id,
            session.environment,
            session.kind.as_str(),
            match session.status {
                compute_state::WorkSessionStatus::Open => "open",
                compute_state::WorkSessionStatus::Closed => "closed",
            },
            session.opened_at.to_rfc3339()
        );
    }
    Ok(())
}

fn print_session(session: &WorkSessionView, json: bool) {
    if json {
        print_json(session);
        return;
    }
    let open = session.status == compute_state::WorkSessionStatus::Open;
    println!("Session:     {}", session.session_id);
    println!(
        "Environment: {} ({})",
        session.environment, session.environment_id
    );
    println!(
        "Kind:        {}",
        match session.kind {
            compute_state::WorkSessionKind::Attached => "attached: the environment outlives it",
            compute_state::WorkSessionKind::Ephemeral =>
                "ephemeral: its temporary environment ends with it",
        }
    );
    println!(
        "Status:      {}{}",
        if open { "open" } else { "closed" },
        session
            .close_reason
            .as_ref()
            .map(|reason| format!(" ({reason})"))
            .unwrap_or_default()
    );
    if let Some(expires_at) = session.expires_at {
        println!("Expires:     {}", expires_at.to_rfc3339());
    }
    if open {
        println!(
            "Work:        compute environment exec {} -- <command>",
            session.environment
        );
    }
}
