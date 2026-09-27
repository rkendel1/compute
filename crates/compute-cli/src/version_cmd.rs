//! `compute version`: publish, deploy, promote, and roll back versions of a
//! project. Each is one authorized request to the control plane — the same
//! one its UI makes — followed to its end, step by step.

use std::time::Duration;

use clap::{Args, Subcommand};
use compute_core::{ComputeError, OperationStep, StepStatus};
use compute_environment::client::DaemonClient;
use compute_environment::{
    DeployVersionRequest, PromoteVersionRequest, PromotionPlan, PublishRequest, RollbackRequest,
    SoftwareView,
};
use compute_state::{RolloutRecord, RolloutStatus, VersionRecord, VersionStatus};

use crate::environment_cmd::{DaemonLocation, error, print_json};

#[derive(Args, Debug)]
pub struct VersionCommand {
    #[command(subcommand)]
    command: VersionCommands,
    #[command(flatten)]
    daemon: DaemonLocation,
}

#[derive(Subcommand, Debug)]
enum VersionCommands {
    /// Publish a version: build, tests, checks, and a source package, run in
    /// the environment the project is developed in.
    Publish {
        project: String,
        #[arg(long)]
        environment: String,
        /// The label. Defaults to the next patch version.
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        no_wait: bool,
        #[arg(long)]
        json: bool,
    },
    /// A project's versions, newest first, and where they run.
    List {
        project: String,
        #[arg(long)]
        json: bool,
    },
    /// One version: its source, package, evidence, and rollouts.
    Show {
        project: String,
        version: String,
        #[arg(long)]
        json: bool,
    },
    /// Deploy a version to an environment, in place.
    Deploy {
        project: String,
        version: String,
        #[arg(long)]
        environment: String,
        #[arg(long)]
        no_wait: bool,
        #[arg(long)]
        json: bool,
    },
    /// Promote the version running in one environment to another, after
    /// showing what it would change.
    Promote {
        project: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        no_wait: bool,
        #[arg(long)]
        json: bool,
    },
    /// Roll an environment back to the version before the current one (or
    /// the one named).
    Rollback {
        project: String,
        #[arg(long)]
        environment: String,
        #[arg(long = "to")]
        version: Option<String>,
        #[arg(long)]
        no_wait: bool,
        #[arg(long)]
        json: bool,
    },
}

fn glyph(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Succeeded => "✓",
        StepStatus::Skipped => "–",
        StepStatus::Running => "●",
        StepStatus::Failed => "×",
        StepStatus::Pending => "○",
    }
}

fn print_steps(steps: &[OperationStep]) {
    for step in steps {
        println!(
            "  {} {:<22}{}{}",
            glyph(step.status),
            step.name,
            step.detail.as_deref().unwrap_or_default(),
            step.job_id
                .as_ref()
                .map(|job| format!("  (job {job})"))
                .unwrap_or_default()
        );
    }
}

async fn follow_version(
    client: &DaemonClient,
    project: &str,
    label: &str,
    json: bool,
) -> compute_core::Result<VersionRecord> {
    let mut shown = 0;
    loop {
        let version: VersionRecord = client
            .get(&format!("/software/{project}/versions/{label}"))
            .await
            .map_err(error)?;
        let done = version
            .steps
            .iter()
            .filter(|step| step.status.is_done())
            .count();
        if !json && done != shown {
            shown = done;
            println!("{project} {label}: {}", version.status.as_str());
            print_steps(&version.steps);
        }
        if version.status != VersionStatus::Publishing {
            return Ok(version);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn follow_rollout(
    client: &DaemonClient,
    rollout: &RolloutRecord,
    json: bool,
) -> compute_core::Result<RolloutRecord> {
    let mut shown = String::new();
    loop {
        let current: RolloutRecord = client
            .get(&format!("/rollouts/{}", rollout.rollout_id))
            .await
            .map_err(error)?;
        let summary = serde_json::to_string(&current.steps).unwrap_or_default();
        if !json && summary != shown {
            shown = summary;
            println!(
                "{} {} {} → {}: {}",
                current.kind.as_str(),
                current.project,
                current.version,
                current.environment,
                current.status.as_str()
            );
            print_steps(&current.steps);
        }
        if current.status != RolloutStatus::Applying {
            return Ok(current);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Follow a rollout to its end.
pub async fn follow(
    client: &DaemonClient,
    rollout: RolloutRecord,
    json: bool,
) -> compute_core::Result<()> {
    let finished = follow_rollout(client, &rollout, json).await?;
    finish_rollout(&finished, json)
}

fn finish_rollout(rollout: &RolloutRecord, json: bool) -> compute_core::Result<()> {
    if json {
        print_json(rollout);
    }
    if rollout.status == RolloutStatus::Failed {
        return Err(ComputeError::Runtime(format!(
            "{} {} did not become real in {}: {}",
            rollout.project,
            rollout.version,
            rollout.environment,
            rollout.failure.clone().unwrap_or_default()
        )));
    }
    Ok(())
}

pub async fn version(command: VersionCommand) -> compute_core::Result<()> {
    let client = command.daemon.client()?;
    match command.command {
        VersionCommands::Publish {
            project,
            environment,
            version,
            no_wait,
            json,
        } => {
            let started: VersionRecord = client
                .post(
                    &format!("/software/{project}/versions"),
                    Some(&PublishRequest {
                        environment,
                        version,
                    }),
                )
                .await
                .map_err(error)?;
            if no_wait {
                print_json(&started);
                return Ok(());
            }
            let finished = follow_version(&client, &project, &started.version, json).await?;
            if json {
                print_json(&finished);
            }
            if finished.status == VersionStatus::Failed {
                return Err(ComputeError::Runtime(format!(
                    "{project} {} was not published: {}",
                    finished.version,
                    finished.failure.unwrap_or_default()
                )));
            }
            if !json {
                println!(
                    "Published {project} {} ({}, package {})",
                    finished.version,
                    finished
                        .commit
                        .as_deref()
                        .map(|commit| &commit[..commit.len().min(12)])
                        .unwrap_or("-"),
                    finished.package_digest.as_deref().unwrap_or("-")
                );
            }
        }
        VersionCommands::List { project, json } => {
            let view: SoftwareView = client
                .get(&format!("/software/{project}"))
                .await
                .map_err(error)?;
            if json {
                print_json(&view);
                return Ok(());
            }
            println!("{project}");
            for placement in &view.summary.environments {
                println!(
                    "  {:<16} {:<10} {}",
                    placement.environment,
                    placement.version.as_deref().unwrap_or("(unversioned)"),
                    placement.rollout.map(RolloutStatus::as_str).unwrap_or("")
                );
            }
            println!("\nVersions (next: {})", view.next_version);
            for version in &view.versions {
                let running = view
                    .rollouts
                    .iter()
                    .filter(|rollout| {
                        rollout.version == version.version
                            && rollout.status == RolloutStatus::Active
                    })
                    .map(|rollout| rollout.environment.clone())
                    .collect::<Vec<_>>();
                println!(
                    "  {:<12} {:<10} {:<13} {} {}",
                    version.version,
                    version.status.as_str(),
                    version
                        .commit
                        .as_deref()
                        .map(|commit| &commit[..commit.len().min(12)])
                        .unwrap_or("-"),
                    version.created_at.to_rfc3339(),
                    if running.is_empty() {
                        String::new()
                    } else {
                        format!("● {}", running.join(", "))
                    }
                );
            }
        }
        VersionCommands::Show {
            project,
            version,
            json,
        } => {
            let record: VersionRecord = client
                .get(&format!("/software/{project}/versions/{version}"))
                .await
                .map_err(error)?;
            if json {
                print_json(&record);
                return Ok(());
            }
            println!("{project} {}: {}", record.version, record.status.as_str());
            println!(
                "  from:      {} ({})",
                record.environment, record.created_by
            );
            println!("  commit:    {}", record.commit.as_deref().unwrap_or("-"));
            println!(
                "  package:   {}",
                record.package_digest.as_deref().unwrap_or("-")
            );
            println!("  published: {}", record.created_at.to_rfc3339());
            print_steps(&record.steps);
            if let Some(failure) = &record.failure {
                println!("  failure:   {failure}");
            }
        }
        VersionCommands::Deploy {
            project,
            version,
            environment,
            no_wait,
            json,
        } => {
            let rollout: RolloutRecord = client
                .post(
                    &format!("/software/{project}/deploy"),
                    Some(&DeployVersionRequest {
                        environment,
                        version,
                        expected_generation: None,
                    }),
                )
                .await
                .map_err(error)?;
            if no_wait {
                print_json(&rollout);
                return Ok(());
            }
            let finished = follow_rollout(&client, &rollout, json).await?;
            finish_rollout(&finished, json)?;
        }
        VersionCommands::Promote {
            project,
            from,
            to,
            no_wait,
            json,
        } => {
            let plan: PromotionPlan = client
                .get(&format!(
                    "/software/{project}/promotion?from={from}&to={to}"
                ))
                .await
                .map_err(error)?;
            if !json {
                println!(
                    "Promote {project} {} from {from} ({}) to {to} (now {}):",
                    plan.version,
                    if plan.from_healthy {
                        "healthy"
                    } else {
                        "not healthy"
                    },
                    plan.to_current.as_deref().unwrap_or("nothing")
                );
                for change in &plan.changes {
                    println!("  · {change}");
                }
                if !plan.config_different.is_empty() {
                    println!(
                        "  configuration set differently: {}",
                        plan.config_different.join(", ")
                    );
                }
                if !plan.config_only_in_from.is_empty() {
                    println!(
                        "  configuration only in {from}: {}",
                        plan.config_only_in_from.join(", ")
                    );
                }
            }
            let rollout: RolloutRecord = client
                .post(
                    &format!("/software/{project}/promote"),
                    Some(&PromoteVersionRequest {
                        from,
                        to,
                        expected_generation: Some(plan.expected_generation),
                    }),
                )
                .await
                .map_err(error)?;
            if no_wait {
                print_json(&rollout);
                return Ok(());
            }
            let finished = follow_rollout(&client, &rollout, json).await?;
            finish_rollout(&finished, json)?;
        }
        VersionCommands::Rollback {
            project,
            environment,
            version,
            no_wait,
            json,
        } => {
            let rollout: RolloutRecord = client
                .post(
                    &format!("/software/{project}/rollback"),
                    Some(&RollbackRequest {
                        environment,
                        version,
                    }),
                )
                .await
                .map_err(error)?;
            if no_wait {
                print_json(&rollout);
                return Ok(());
            }
            let finished = follow_rollout(&client, &rollout, json).await?;
            finish_rollout(&finished, json)?;
        }
    }
    Ok(())
}
