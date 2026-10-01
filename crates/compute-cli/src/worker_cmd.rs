//! `compute worker`: external workers that run under Compute's lifecycle.
//!
//! A worker is not a runtime and not a target. It is an adapter that turns an
//! outside scheduler's unit of work into an ordinary Compute execution, so it
//! gets admission, cancellation, cleanup, and a receipt for free. GitHub
//! Actions is the first, and it lives in `compute-worker-github`; nothing in
//! Compute's core knows about it.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand};
use compute_core::{ComputeError, ExecutionControl, ExecutionStatus};
use compute_runtime::Compute;
use compute_worker_github::{
    DEFAULT_CREDENTIAL_ENV, DEFAULT_SERVER_URL, RECIPE_NAME, RecipeEvidence, Repository,
    RestGitHubApi, RunnerSpec, RunnerWorker, Secret, WorkerError,
};
use serde_json::json;

#[derive(Args, Debug)]
pub struct WorkerCommand {
    #[command(subcommand)]
    command: WorkerCommands,
}

#[derive(Subcommand, Debug)]
enum WorkerCommands {
    /// Run GitHub Actions runners.
    GithubActions(GithubActionsCommand),
}

#[derive(Args, Debug)]
struct GithubActionsCommand {
    #[command(subcommand)]
    command: GithubActionsCommands,
}

#[derive(Subcommand, Debug)]
enum GithubActionsCommands {
    /// Register one ephemeral runner, let it take exactly one job, and clean
    /// up. The GitHub credential is read from an environment variable; the
    /// registration token is never printed, stored, or put in a receipt.
    Run(RunArgs),
}

#[derive(Args, Debug)]
struct RunArgs {
    /// The repository the runner serves, as OWNER/NAME.
    #[arg(long)]
    repository: String,
    /// The runner release to use (pinned, e.g. 2.331.0).
    #[arg(long)]
    runner_version: String,
    /// SHA-256 of the runner release archive. The archive is never run
    /// unless it matches.
    #[arg(long)]
    runner_sha256: String,
    /// A runner label (repeatable).
    #[arg(long = "label")]
    labels: Vec<String>,
    /// The runner's name on GitHub (generated when absent).
    #[arg(long)]
    name: Option<String>,
    /// Fetch the archive from here instead of the GitHub release URL.
    #[arg(long)]
    download_url: Option<String>,
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    server_url: String,
    #[arg(long, default_value = RestGitHubApi::DEFAULT_API_URL)]
    api_url: String,
    /// The most the runner may run, including waiting for a job.
    #[arg(long, value_parser = crate::parse_duration)]
    timeout: Option<Duration>,
    /// Run the runner's `installdependencies.sh` (needs privileges).
    #[arg(long)]
    install_dependencies: bool,
    /// The environment variable holding a GitHub token that may administer
    /// the repository's runners.
    #[arg(long, default_value = DEFAULT_CREDENTIAL_ENV)]
    token_env: String,
    /// The `github-actions-runner` recipe file this run realizes; its name
    /// and digest are recorded in the report. The file is validated, and
    /// holds no repository or credential.
    #[arg(long)]
    recipe_file: Option<PathBuf>,
    /// Write Compute's execution receipt here.
    #[arg(long)]
    receipt: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

pub async fn command(command: WorkerCommand) -> compute_core::Result<()> {
    match command.command {
        WorkerCommands::GithubActions(command) => match command.command {
            GithubActionsCommands::Run(args) => run(args).await,
        },
    }
}

fn failure(error: WorkerError) -> ComputeError {
    ComputeError::Coded {
        code: error.code().into(),
        message: error.to_string(),
    }
}

fn recipe_evidence(path: &std::path::Path) -> compute_core::Result<RecipeEvidence> {
    let spec: compute_core::RecipeSpec = serde_json::from_slice(&std::fs::read(path)?)?;
    let problems = spec.problems();
    if !problems.is_empty() {
        return Err(ComputeError::InvalidWorkload(format!(
            "invalid recipe: {}",
            problems.join("; ")
        )));
    }
    Ok(RecipeEvidence {
        name: RECIPE_NAME.into(),
        digest: compute_core::recipe_digest(&spec),
    })
}

async fn run(args: RunArgs) -> compute_core::Result<()> {
    let repository = Repository::parse(&args.repository).map_err(failure)?;
    let credential = match std::env::var(&args.token_env) {
        Ok(value) if !value.is_empty() => Secret::new(value),
        _ => {
            return Err(failure(WorkerError::MissingCredential(
                args.token_env.clone(),
            )));
        }
    };
    let mut spec =
        RunnerSpec::new(repository, args.runner_version, args.runner_sha256).map_err(failure)?;
    spec.labels = args.labels;
    spec.name = args.name;
    spec.download_url = args.download_url;
    spec.server_url = args.server_url;
    spec.install_dependencies = args.install_dependencies;
    if let Some(timeout) = args.timeout {
        spec.timeout = timeout;
    }
    if let Some(path) = &args.recipe_file {
        spec.recipe = Some(recipe_evidence(path)?);
    }

    let api = RestGitHubApi::new(args.api_url).map_err(failure)?;
    let worker = RunnerWorker::new(api, Compute::new());

    // An interrupt cancels the execution: Compute ends the runner's whole
    // process group and removes its workspace before this returns.
    let control = ExecutionControl::new();
    let interrupt = control.clone();
    tokio::spawn(async move {
        wait_for_interrupt().await;
        interrupt.cancel();
    });

    let run = worker
        .run(&spec, &credential, &control)
        .await
        .map_err(failure)?;
    if let Some(path) = &args.receipt {
        let receipt = run.result.receipt.as_ref().ok_or_else(|| {
            ComputeError::InvalidReceipt("execution did not produce a receipt".into())
        })?;
        std::fs::write(path, receipt.encoded_bytes()?)?;
    }

    let succeeded = run.result.status == ExecutionStatus::Completed
        && run.result.exit_code.is_none_or(|code| code == 0);
    let exit_code = if succeeded {
        0
    } else {
        run.result.exit_code.filter(|code| *code != 0).unwrap_or(1)
    };
    if args.json {
        let error = (!succeeded).then(|| {
            json!({
                "code": "runner_failed",
                "message": format!(
                    "the runner ended {:?} at stage {}",
                    run.report.status, run.report.stage
                ),
            })
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": succeeded,
                "command": "compute worker github-actions run",
                "exit_code": exit_code,
                "data": { "report": run.report },
                "error": error,
            }))
            .expect("a report serializes")
        );
    } else {
        let report = &run.report;
        println!("runner {} for {}", report.runner.name, report.repository);
        println!(
            "status: {:?} (exit {:?}) at stage {}",
            report.status, report.exit_code, report.stage
        );
        if let Some(name) = &report.job.name {
            println!(
                "job: {name} ({})",
                report.job.result.as_deref().unwrap_or("unknown")
            );
        }
        println!(
            "workspace removed: {}",
            report
                .cleanup
                .workspace_removed
                .map_or("unknown".to_string(), |removed| removed.to_string())
        );
        if !run.result.stderr.text.is_empty() {
            eprint!("{}", run.result.stderr.text);
        }
    }
    if !succeeded {
        std::process::exit(exit_code);
    }
    Ok(())
}

async fn wait_for_interrupt() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
