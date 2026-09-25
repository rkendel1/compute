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

/// The controller's side of `compute doctor`: every item the node's
/// operator needs to know is well, or why it is not. A controller that
/// is not running, or a credential that is missing, is a finding, not an
/// error.
pub async fn controller_diagnosis(location: &DaemonLocation) -> serde_json::Value {
    use compute_environment::EnvironmentError;
    use serde_json::json;
    let endpoint = location.endpoint();
    let client = match location.client() {
        Ok(client) => client,
        Err(failure) => {
            return json!({ "endpoint": endpoint, "reachable": false, "error": failure.to_string() });
        }
    };
    let health = match client.get::<serde_json::Value>("/health").await {
        Ok(health) => health,
        Err(failure) => {
            return json!({
                "endpoint": endpoint,
                "reachable": false,
                "error": failure.to_string(),
                "remediation": "start the controller with `compute start`",
            });
        }
    };
    let info = match client.get::<ControllerInfo>("/info").await {
        Ok(info) => info,
        Err(failure) => {
            let authentication = match &failure {
                EnvironmentError::Unauthorized(_) => format!(
                    "a credential is required: set ${} to an operator token with compute.read",
                    location.token_env
                ),
                EnvironmentError::Forbidden(_) => {
                    "the credential lacks the compute.read scope".to_string()
                }
                _ => failure.to_string(),
            };
            return json!({
                "endpoint": endpoint,
                "reachable": true,
                "status": health["status"],
                "authentication": authentication,
            });
        }
    };
    let runtimes = serde_json::from_value::<compute_core::RuntimeInventory>(info.runtimes.clone())
        .map(|inventory| {
            inventory
                .runtimes
                .iter()
                .map(|runtime| {
                    (
                        runtime.id.as_str().to_string(),
                        json!(if runtime.available && runtime.compatible {
                            "available"
                        } else {
                            "unavailable"
                        }),
                    )
                })
                .collect::<serde_json::Map<_, _>>()
        })
        .unwrap_or_default();
    let isolation = info
        .isolation
        .as_ref()
        .map(|report| {
            report
                .profiles
                .iter()
                .map(|support| {
                    (
                        support.profile.as_str().to_string(),
                        match &support.enforcement {
                            Some(plan) => json!({
                                "filesystem": plan.filesystem,
                                "network": plan.network,
                                "memory": plan.memory,
                                "cpu": plan.cpu,
                                "process": plan.process,
                            }),
                            None => json!({
                                "unsupported": support.refusal.as_ref().map(|r| r.message.clone()),
                            }),
                        },
                    )
                })
                .collect::<serde_json::Map<_, _>>()
        })
        .unwrap_or_default();
    let security = &info.security;
    let control = &info.control_plane;
    json!({
        "endpoint": endpoint,
        "reachable": true,
        "status": health["status"],
        "controller": {
            "version": info.controller.version,
            "git_commit": info.controller.git_commit,
            "build_id": info.controller.build_id,
            "platform": info.controller.platform,
            "instance_id": info.instance_id,
            "pid": info.pid,
            "started_at": info.started_at,
        },
        "data_plane": {
            "kind": info.data_plane.info.as_ref().map(|plane| plane.kind.clone()),
            "independent": info.data_plane.independent,
            "status": if info.data_plane.error.is_some() { "unreachable" } else { "ok" },
            "error": info.data_plane.error,
            "pid": info.data_plane.info.as_ref().map(|plane| plane.pid),
            "units": info.data_plane.info.as_ref().map(|plane| plane.units),
        },
        "control_plane": {
            "mode": control.mode,
            "error": control.error,
            "last_reconciled_at": control.last_reconciled_at,
        },
        "state": {
            "kind": control.state.kind,
            "location": control.state.location,
            "connected": control.mode == "normal",
        },
        "workloads": {
            "total": info.workloads.total,
            "running": info.workloads.running,
            "failed": info.workloads.failed,
            "unhealthy": info.workloads.unhealthy,
        },
        "endpoints": {
            "routed": info.workloads.endpoints,
            "errors": info.workloads.endpoint_errors,
        },
        "tls": security.tls,
        "authentication": {
            "mode": security.mode,
            "required": security.authentication_required,
            "active_credentials": security.active_credentials,
            "reason": security.reason,
        },
        "runtimes": runtimes,
        "isolation": isolation,
        "upgrade": info.upgrade.as_ref().map(|record| json!({
            "upgrade_id": record.upgrade_id,
            "kind": record.kind,
            "status": record.status,
            "to": record.to.version,
            "reason": record.reason,
        })),
    })
}

/// Print the controller diagnosis for people.
pub fn print_diagnosis(diagnosis: &serde_json::Value) {
    let text = |value: &serde_json::Value| match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => "-".into(),
        other => other.to_string(),
    };
    println!("\nController ({})", text(&diagnosis["endpoint"]));
    if diagnosis["reachable"] != true {
        println!("  not reachable: {}", text(&diagnosis["error"]));
        if let Some(remediation) = diagnosis["remediation"].as_str() {
            println!("  Remediation: {remediation}");
        }
        return;
    }
    if diagnosis["controller"].is_null() {
        println!("  status: {}", text(&diagnosis["status"]));
        println!("  authentication: {}", text(&diagnosis["authentication"]));
        return;
    }
    let controller = &diagnosis["controller"];
    println!(
        "  version: {} ({}), build {}",
        text(&controller["version"]),
        text(&controller["git_commit"]),
        text(&controller["build_id"])
    );
    let plane = &diagnosis["data_plane"];
    println!(
        "  data plane: {} {} (pid {}, {} units, independent {})",
        text(&plane["status"]),
        text(&plane["kind"]),
        text(&plane["pid"]),
        text(&plane["units"]),
        text(&plane["independent"])
    );
    let control = &diagnosis["control_plane"];
    println!(
        "  control plane: {}{}",
        text(&control["mode"]),
        control["error"]
            .as_str()
            .map(|error| format!(" ({error})"))
            .unwrap_or_default()
    );
    let state = &diagnosis["state"];
    println!(
        "  state: {} at {} ({})",
        text(&state["kind"]),
        text(&state["location"]),
        if state["connected"] == true {
            "connected"
        } else {
            "unreachable"
        }
    );
    let workloads = &diagnosis["workloads"];
    println!(
        "  workloads: {} ({} running, {} failed, {} unhealthy)",
        text(&workloads["total"]),
        text(&workloads["running"]),
        text(&workloads["failed"]),
        workloads["unhealthy"].as_array().map_or(0, Vec::len)
    );
    for unhealthy in workloads["unhealthy"].as_array().into_iter().flatten() {
        println!("    unhealthy: {}", text(unhealthy));
    }
    let endpoints = &diagnosis["endpoints"];
    println!(
        "  endpoints: {} routed, {} failing",
        text(&endpoints["routed"]),
        endpoints["errors"]
            .as_object()
            .map_or(0, serde_json::Map::len)
    );
    let tls = &diagnosis["tls"];
    println!(
        "  TLS: {}",
        if tls["enabled"] == true {
            format!("on until {}", text(&tls["not_after"]))
        } else {
            "off".into()
        }
    );
    let authentication = &diagnosis["authentication"];
    println!(
        "  authentication: {} mode, {} ({} active credentials)",
        text(&authentication["mode"]),
        if authentication["required"] == true {
            "required"
        } else {
            "not required"
        },
        text(&authentication["active_credentials"])
    );
    if let Some(runtimes) = diagnosis["runtimes"].as_object() {
        let available = runtimes
            .iter()
            .filter(|(_, status)| *status == "available")
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        println!("  runtimes: {}", available.join(", "));
    }
    if let Some(isolation) = diagnosis["isolation"].as_object() {
        for (profile, support) in isolation {
            if let Some(reason) = support.get("unsupported") {
                println!("  isolation {profile}: unsupported ({})", text(reason));
            } else {
                println!(
                    "  isolation {profile}: filesystem {}, network {}, memory {}, cpu {}, process {}",
                    text(&support["filesystem"]),
                    text(&support["network"]),
                    text(&support["memory"]),
                    text(&support["cpu"]),
                    text(&support["process"])
                );
            }
        }
    }
    match diagnosis["upgrade"].as_object() {
        Some(upgrade) => println!(
            "  upgrade: {} {} to {}",
            text(&upgrade["kind"]),
            text(&upgrade["status"]),
            text(&upgrade["to"])
        ),
        None => println!("  upgrade: none has run"),
    }
}
