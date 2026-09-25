//! `compute auth` and `compute node`: operator credentials, the audit
//! trail, and the controller itself, through the Compute API.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand};
use compute_core::ComputeError;
use compute_environment::ControllerInfo;
use compute_environment::auth::{CredentialRequest, CredentialView, IssuedCredential, Principal};
use compute_state::AuditRecord;

use crate::environment_cmd::{DaemonLocation, error, print_json};

#[derive(Args, Debug)]
pub struct AuthCommand {
    #[command(subcommand)]
    pub command: AuthCommands,
    #[command(flatten)]
    pub daemon: DaemonLocation,
}

#[derive(Subcommand, Debug)]
pub enum AuthCommands {
    /// Issue a credential. Its token is printed once and never again.
    Create {
        /// Who the credential belongs to, such as developer-42.
        #[arg(long)]
        operator: String,
        /// compute.read, compute.execute, compute.deploy, compute.operate,
        /// or compute.admin. Repeatable.
        #[arg(long = "scope", required = true)]
        scopes: Vec<String>,
        #[arg(long)]
        description: Option<String>,
        /// Lifetime: seconds, or a number with s, m, h, or d.
        #[arg(long)]
        expires_in: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List credentials: never their tokens.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Revoke a credential now.
    Revoke {
        credential: String,
        #[arg(long)]
        json: bool,
    },
    /// Replace a credential with a new token for the same operator and
    /// scopes. The old one stops working now, or after --grace.
    Rotate {
        credential: String,
        /// How long the old token keeps working: seconds, or s, m, h, d.
        #[arg(long)]
        grace: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Who the configured credential is, and what it may do.
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// The audit trail of remote operations, newest first.
    Audit {
        #[arg(long)]
        operator: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
}

/// `90`, `90s`, `15m`, `12h`, `30d` as seconds.
pub(crate) fn duration_seconds(value: &str) -> compute_core::Result<u64> {
    let value = value.trim();
    let (number, unit) = match value.char_indices().last() {
        Some((index, unit)) if unit.is_ascii_alphabetic() => (&value[..index], unit),
        _ => (value, 's'),
    };
    let number: u64 = number
        .parse()
        .map_err(|_| ComputeError::InvalidWorkload(format!("invalid duration {value}")))?;
    let unit = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => {
            return Err(ComputeError::InvalidWorkload(format!(
                "invalid duration unit in {value}; use s, m, h, or d"
            )));
        }
    };
    Ok(number.saturating_mul(unit))
}

fn print_credential(view: &CredentialView) {
    println!(
        "{}\t{}\t{}\t{}\t{}",
        view.credential_id,
        view.operator_id,
        view.status,
        view.scopes.join(","),
        view.expires_at
            .map(|at| format!("expires {}", at.to_rfc3339()))
            .unwrap_or_else(|| "no expiry".into())
    );
}

fn print_issued(issued: &IssuedCredential, json: bool) {
    if json {
        print_json(issued);
        return;
    }
    println!("Credential: {}", issued.credential.credential_id);
    println!("Operator: {}", issued.credential.operator_id);
    println!("Scopes: {}", issued.credential.scopes.join(", "));
    if let Some(at) = issued.credential.expires_at {
        println!("Expires: {}", at.to_rfc3339());
    }
    println!("Token (shown once; Compute keeps only its verifier):");
    println!("{}", issued.token);
}

pub async fn auth(command: AuthCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    match command.command {
        AuthCommands::Create {
            operator,
            scopes,
            description,
            expires_in,
            json,
        } => {
            let request = CredentialRequest {
                operator_id: operator,
                scopes,
                description,
                expires_in_seconds: expires_in.as_deref().map(duration_seconds).transpose()?,
            };
            let issued: IssuedCredential = client
                .post("/auth/credentials", Some(&request))
                .await
                .map_err(error)?;
            print_issued(&issued, json);
        }
        AuthCommands::List { json } => {
            let credentials: Vec<CredentialView> =
                client.get("/auth/credentials").await.map_err(error)?;
            if json {
                print_json(&credentials);
            } else {
                for view in &credentials {
                    print_credential(view);
                }
            }
        }
        AuthCommands::Revoke { credential, json } => {
            let view: CredentialView = client
                .post::<(), _>(&format!("/auth/credentials/{credential}/revoke"), None)
                .await
                .map_err(error)?;
            if json {
                print_json(&view);
            } else {
                println!("Revoked {} of {}", view.credential_id, view.operator_id);
            }
        }
        AuthCommands::Rotate {
            credential,
            grace,
            json,
        } => {
            let request = compute_environment::auth::RotateRequest {
                grace_seconds: grace
                    .as_deref()
                    .map(duration_seconds)
                    .transpose()?
                    .unwrap_or(0),
            };
            let issued: IssuedCredential = client
                .post(
                    &format!("/auth/credentials/{credential}/rotate"),
                    Some(&request),
                )
                .await
                .map_err(error)?;
            if !json {
                println!("Rotated {credential}");
            }
            print_issued(&issued, json);
        }
        AuthCommands::Whoami { json } => {
            let principal: Principal = client.get("/auth/whoami").await.map_err(error)?;
            if json {
                print_json(&principal);
            } else {
                println!("Operator: {}", principal.operator_id);
                if let Some(credential) = &principal.credential_id {
                    println!("Credential: {credential}");
                }
                println!(
                    "Scopes: {}",
                    principal
                        .scopes
                        .iter()
                        .map(|scope| scope.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                if principal.development {
                    println!("Admitted without a credential: this daemon is in development mode");
                }
            }
        }
        AuthCommands::Audit {
            operator,
            limit,
            json,
        } => {
            let mut path = format!("/audit?limit={limit}");
            if let Some(operator) = operator {
                path.push_str(&format!("&operator={operator}"));
            }
            let records: Vec<AuditRecord> = client.get(&path).await.map_err(error)?;
            if json {
                print_json(&records);
            } else {
                for record in &records {
                    println!(
                        "{}\t{}\t{}\t{}\t{}{}\t{}",
                        record.at.to_rfc3339(),
                        record.request_id,
                        record.operator_id,
                        record.operation,
                        record.resource,
                        record
                            .resource_id
                            .as_ref()
                            .map(|id| format!("/{id}"))
                            .unwrap_or_default(),
                        record.result
                    );
                }
            }
        }
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct NodeCommand {
    #[command(subcommand)]
    pub command: NodeCommands,
    #[command(flatten)]
    pub daemon: DaemonLocation,
}

#[derive(Subcommand, Debug)]
pub enum NodeCommands {
    /// Which controller runs: version, commit, build, security, runtimes.
    Info {
        #[arg(long)]
        json: bool,
    },
    /// Whether the controller answers, and whether its control plane is
    /// degraded. Needs no credential.
    Health {
        #[arg(long)]
        json: bool,
    },
    /// Reconcile now, fully, and show what the cycle examined and changed.
    Reconcile {
        #[arg(long)]
        json: bool,
    },
    /// Replace the controller with another Compute build, without
    /// restarting or redeploying a workload. The previous controller is
    /// restored if the new one does not become ready.
    Upgrade {
        /// The new Compute executable, on the node.
        artifact: PathBuf,
        /// Refuse unless the artifact has this SHA-256.
        #[arg(long)]
        sha256: Option<String>,
        /// Seconds the new controller has to become ready.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        #[arg(long)]
        json: bool,
    },
    /// Return to the build that ran before the last upgrade.
    Rollback {
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        #[arg(long)]
        json: bool,
    },
    /// The last upgrade or rollback on this node.
    UpgradeStatus {
        #[arg(long)]
        json: bool,
    },
}

pub async fn node(command: NodeCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    match command.command {
        NodeCommands::Info { json } => {
            let info: ControllerInfo = client.get("/info").await.map_err(error)?;
            if json {
                print_json(&info);
            } else {
                print_info(&info);
            }
        }
        NodeCommands::Reconcile { json } => {
            let metrics: compute_environment::ReconcileMetrics = client
                .post::<(), _>("/node/reconcile", None)
                .await
                .map_err(error)?;
            if json {
                print_json(&metrics);
            } else if let Some(cycle) = &metrics.last {
                println!(
                    "Reconciled in {:.1} ms: {} resources examined, {} changed, {} errors",
                    cycle.duration_ms,
                    cycle.resources_examined,
                    cycle.resources_changed,
                    cycle.errors
                );
            }
        }
        NodeCommands::Upgrade {
            artifact,
            sha256,
            timeout,
            json,
        } => {
            let artifact = std::fs::canonicalize(&artifact)?;
            let request = compute_environment::upgrade::UpgradeRequest {
                artifact: artifact.display().to_string(),
                expect_sha256: sha256,
                timeout_seconds: Some(timeout),
            };
            let started: compute_environment::upgrade::UpgradeRecord = client
                .post("/node/upgrade", Some(&request))
                .await
                .map_err(error)?;
            follow_upgrade(&client, started, timeout, json).await?;
        }
        NodeCommands::Rollback { timeout, json } => {
            let started: compute_environment::upgrade::UpgradeRecord = client
                .post(
                    "/node/rollback",
                    Some(&serde_json::json!({ "timeout_seconds": timeout })),
                )
                .await
                .map_err(error)?;
            follow_upgrade(&client, started, timeout, json).await?;
        }
        NodeCommands::UpgradeStatus { json } => {
            let record: Option<compute_environment::upgrade::UpgradeRecord> =
                client.get("/node/upgrade").await.map_err(error)?;
            if json {
                print_json(&record);
            } else {
                match record {
                    Some(record) => print_upgrade(&record),
                    None => println!("No upgrade has run on this node"),
                }
            }
        }
        NodeCommands::Health { json } => {
            let health: serde_json::Value = client.get("/health").await.map_err(error)?;
            if json {
                print_json(&health);
            } else {
                println!(
                    "{} (controller {}, pid {})",
                    health["status"].as_str().unwrap_or("unknown"),
                    health["instance_id"].as_str().unwrap_or_default(),
                    health["pid"]
                );
            }
        }
    }
    Ok(())
}

/// Wait until the hand-over finishes: the API pauses while controllers
/// change, and answers again from the new one (or the restored one).
async fn follow_upgrade(
    client: &compute_environment::client::DaemonClient,
    started: compute_environment::upgrade::UpgradeRecord,
    timeout: u64,
    json: bool,
) -> compute_core::Result<()> {
    if !json {
        println!(
            "{} {}: {} -> {} (workloads keep running)",
            started.kind, started.upgrade_id, started.from.build_id, started.to.build_id
        );
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout + 150);
    loop {
        if let Ok(Some(record)) = client
            .get::<Option<compute_environment::upgrade::UpgradeRecord>>("/node/upgrade")
            .await
            && record.upgrade_id == started.upgrade_id
            && record.is_terminal()
        {
            if json {
                print_json(&record);
            } else {
                print_upgrade(&record);
            }
            return if record.status == "completed" {
                Ok(())
            } else {
                Err(ComputeError::Runtime(format!(
                    "upgrade_failed: {}",
                    record.reason.unwrap_or_default()
                )))
            };
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ComputeError::Runtime(
                "upgrade_failed: no controller reported the upgrade finished".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn print_upgrade(record: &compute_environment::upgrade::UpgradeRecord) {
    println!("Upgrade: {} ({})", record.upgrade_id, record.kind);
    println!("Status: {}", record.status);
    println!("From: {} {}", record.from.version, record.from.build_id);
    println!("To: {} {}", record.to.version, record.to.build_id);
    println!("Workloads kept running: {}", record.units.len());
    if let Some(reason) = &record.reason {
        println!("Reason: {reason}");
    }
    if let (Some(finished), Some(controller)) = (record.finished_at, &record.controller) {
        println!(
            "Finished: {} in {:.1}s, controller {controller}",
            finished.to_rfc3339(),
            (finished - record.started_at).num_milliseconds() as f64 / 1000.0
        );
    }
}

pub(crate) fn print_info(info: &ControllerInfo) {
    let controller = &info.controller;
    println!("Controller: {} (pid {})", info.instance_id, info.pid);
    println!(
        "Version: {} ({}, {} build)",
        controller.version, controller.git_commit, controller.build_profile
    );
    println!("Build ID: {}", controller.build_id);
    println!("Platform: {}", controller.platform);
    println!("Node: {}", info.node_id);
    println!("Started: {}", info.started_at.to_rfc3339());
    let security = &info.security;
    println!(
        "Security: {} ({}); authentication {}; TLS {}",
        match security.mode {
            compute_environment::auth::SecurityMode::Production => "production",
            compute_environment::auth::SecurityMode::Development => "development",
        },
        security.reason,
        if security.authentication_required {
            "required"
        } else {
            "optional"
        },
        if security.tls.enabled {
            format!(
                "on, {} until {}",
                security.tls.fingerprint.clone().unwrap_or_default(),
                security
                    .tls
                    .not_after
                    .map(|at| at.to_rfc3339())
                    .unwrap_or_default()
            )
        } else {
            "off".into()
        }
    );
    println!(
        "Control plane: {} ({} {}){}",
        info.control_plane.mode,
        info.control_plane.state.kind,
        info.control_plane.state.location,
        info.control_plane
            .error
            .as_ref()
            .map(|error| format!(": {error}"))
            .unwrap_or_default()
    );
}
