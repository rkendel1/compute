//! `compute cloudflare`: the Cloudflare provider's own operations.
//!
//! Deployment, invocation, and deletion are separate commands with separate
//! evidence. Running a portable workload on a deployed module goes through
//! the provider boundary like any other provider (`compute run --provider`,
//! `compute pool run`); these commands manage what that needs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand};
use compute_core::{ComputeError, Result};
use compute_provider::ComputeProvider;
use compute_provider_cloudflare::{CloudflareProfile, CloudflareProvider, Invocation};
use serde::Serialize;

use crate::pool::PoolLocation;

#[derive(Args, Debug)]
pub struct CloudflareCommand {
    #[command(subcommand)]
    pub command: CloudflareCommands,
    #[command(flatten)]
    pub location: PoolLocation,
}

#[derive(Subcommand, Debug)]
pub enum CloudflareCommands {
    /// Show a Cloudflare execution profile: what it runs, what it refuses,
    /// and the plan limits Cloudflare enforces. Needs no account.
    Profile {
        #[arg(default_value = compute_provider_cloudflare::WORKERS_FREE)]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Write a pool file with a cloudflare member. It names the variable that
    /// holds the API token, never the token, and an existing file is never
    /// overwritten.
    Init {
        /// The Cloudflare account identifier (32 hexadecimal characters).
        #[arg(long)]
        account_id: String,
        /// Where to write the pool file.
        #[arg(long)]
        output: PathBuf,
        /// The member's identifier in the pool.
        #[arg(long, default_value = "cloudflare")]
        provider: String,
        /// The environment variable that will hold the API token.
        #[arg(long, default_value = compute_provider_cloudflare::config::DEFAULT_TOKEN_ENV)]
        token_env: String,
    },
    /// Check a cloudflare provider: its configuration, its credential, and
    /// its account. `--offline` checks the configuration only.
    Validate {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Deploy a WASI module as a Worker. Idempotent; prints a deployment
    /// receipt, which proves Cloudflare accepted the Worker and nothing more.
    Deploy {
        #[arg(long)]
        provider: String,
        module: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Whether a module's Worker is deployed, and with which deployment.
    Status {
        #[arg(long)]
        provider: String,
        module: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Invoke a deployed module once and print its outcome and invocation
    /// record. Never deploys.
    Invoke {
        #[arg(long)]
        provider: String,
        module: PathBuf,
        /// An argument after argv[0]; repeat for more.
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        /// `NAME=VALUE`, or `NAME` to pass this process's value of NAME (so
        /// a secret never needs to be written on the command line).
        #[arg(long = "env")]
        env: Vec<String>,
        /// A file whose bytes are the module's standard input.
        #[arg(long)]
        stdin: Option<PathBuf>,
        #[arg(long, value_parser = crate::parse_duration)]
        timeout: Option<Duration>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a Worker Compute deployed: by module, or by Worker name.
    Delete {
        #[arg(long)]
        provider: String,
        /// The module whose Worker to delete.
        #[arg(long, conflicts_with = "worker", required_unless_present = "worker")]
        module: Option<PathBuf>,
        /// The Worker's name (`<prefix><32 hex>`).
        #[arg(long)]
        worker: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List the Workers this provider's prefix names on the account.
    List {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        json: bool,
    },
}

fn provider_error(error: compute_provider::ProviderError) -> ComputeError {
    let code = serde_json::to_value(error.kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    ComputeError::Coded {
        message: format!("{code}: {}", error.message),
        code,
    }
}

fn read_module(path: &PathBuf) -> Result<Vec<u8>> {
    std::fs::read(path)
        .map_err(|error| ComputeError::InvalidWorkload(format!("{}: {error}", path.display())))
}

fn print_json(value: &impl Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("serializable output")
    );
}

fn environment(values: &[String]) -> Result<BTreeMap<String, String>> {
    values
        .iter()
        .map(|value| match value.split_once('=') {
            Some((name, value)) => Ok((name.to_owned(), value.to_owned())),
            None => std::env::var(value)
                .map(|found| (value.clone(), found))
                .map_err(|_| {
                    ComputeError::InvalidWorkload(format!(
                        "--env {value}: the variable is not set in this environment"
                    ))
                }),
        })
        .collect()
}

#[derive(Serialize)]
struct Validation<'a> {
    provider_id: &'a str,
    profile: &'a str,
    account_id: &'a str,
    configuration: &'static str,
    credential: String,
    credential_present: bool,
    capabilities: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<compute_provider_cloudflare::AccountCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_error: Option<String>,
}

pub async fn cloudflare(command: CloudflareCommand) -> Result<()> {
    let location = command.location;
    match command.command {
        CloudflareCommands::Profile { name, json } => {
            let profile = CloudflareProfile::named(&name).ok_or_else(|| {
                ComputeError::InvalidWorkload(format!(
                    "unknown Cloudflare profile {name:?}; known profiles: {}",
                    CloudflareProfile::names().join(", ")
                ))
            })?;
            if json {
                print_json(&profile);
            } else {
                print_profile(&profile);
            }
        }
        CloudflareCommands::Init {
            account_id,
            output,
            provider,
            token_env,
        } => init(&account_id, &output, &provider, &token_env)?,
        CloudflareCommands::Validate {
            provider,
            offline,
            json,
        } => {
            let cloudflare = location.cloudflare(&provider)?;
            // Capabilities are the profile's; reporting them needs no
            // account.
            cloudflare.capabilities().await.map_err(provider_error)?;
            let mut validation = Validation {
                provider_id: &provider,
                profile: &cloudflare.profile().name,
                account_id: &cloudflare.settings().account_id,
                configuration: "valid",
                credential: cloudflare.credential_source().to_string(),
                credential_present: cloudflare.has_credential(),
                capabilities: "reported",
                account: None,
                account_error: None,
            };
            let mut failure = None;
            if !offline {
                match cloudflare.validate_account().await {
                    Ok(account) => validation.account = Some(account),
                    Err(error) => {
                        validation.account_error = Some(error.message.clone());
                        failure = Some(error);
                    }
                }
            }
            if json {
                print_json(&validation);
            } else {
                print_validation(&validation, offline);
            }
            if let Some(error) = failure {
                return Err(provider_error(error));
            }
        }
        CloudflareCommands::Deploy {
            provider,
            module,
            json,
        } => {
            let cloudflare = location.cloudflare(&provider)?;
            let receipt = cloudflare
                .deploy(&read_module(&module)?)
                .await
                .map_err(provider_error)?;
            if json {
                print_json(&receipt);
            } else {
                println!(
                    "{} {} on {} ({})",
                    match receipt.action {
                        compute_provider_cloudflare::DeploymentAction::Deployed => "Deployed",
                        compute_provider_cloudflare::DeploymentAction::AlreadyDeployed => {
                            "Already deployed"
                        }
                    },
                    receipt.worker,
                    provider,
                    receipt.profile
                );
                println!("Deployment: {}", receipt.deployment_id);
                println!("Workload:   {}", receipt.workload_digest);
                println!("URL:        {}", receipt.url);
                println!("Receipt:    {}", receipt.receipt_hash);
                println!("{}", receipt.note);
            }
        }
        CloudflareCommands::Status {
            provider,
            module,
            json,
        } => {
            let cloudflare = location.cloudflare(&provider)?;
            let status = cloudflare
                .status(&read_module(&module)?)
                .await
                .map_err(provider_error)?;
            if json {
                print_json(&status);
            } else {
                println!("{}: {}", status.worker, status.state);
                println!("Deployment: {}", status.deployment_id);
                println!("Workload:   {}", status.workload_digest);
            }
        }
        CloudflareCommands::Invoke {
            provider,
            module,
            args,
            env,
            stdin,
            timeout,
            json,
        } => {
            let cloudflare = location.cloudflare(&provider)?;
            let invocation = Invocation {
                args,
                env: environment(&env)?,
                stdin: match stdin {
                    Some(path) => read_module(&path)?,
                    None => vec![],
                },
                timeout,
            };
            let (invoked, record) = cloudflare
                .invoke(&read_module(&module)?, invocation, None)
                .await
                .map_err(provider_error)?;
            let result = invoked.result(&record.execution_id, None, None);
            if json {
                print_json(&serde_json::json!({ "result": result, "invocation": record }));
            } else {
                print!("{}", result.stdout.text);
                eprint!("{}", result.stderr.text);
                eprintln!(
                    "[{}] {:?} exit {} ({} ms)",
                    record.execution_id,
                    record.outcome,
                    result
                        .exit_code
                        .map_or_else(|| "-".into(), |code| code.to_string()),
                    result.duration.as_millis()
                );
            }
            if result.status != compute_core::ExecutionStatus::Completed
                || result.exit_code != Some(0)
            {
                std::process::exit(result.exit_code.filter(|code| *code != 0).unwrap_or(1));
            }
        }
        CloudflareCommands::Delete {
            provider,
            module,
            worker,
            json,
        } => {
            let cloudflare = location.cloudflare(&provider)?;
            let worker = match (worker, module) {
                (Some(worker), _) => worker,
                (None, Some(module)) => {
                    cloudflare
                        .plan(&read_module(&module)?)
                        .map_err(provider_error)?
                        .worker
                }
                (None, None) => unreachable!("clap requires one"),
            };
            let record = cloudflare.delete(&worker).await.map_err(provider_error)?;
            if json {
                print_json(&record);
            } else {
                println!("{}: {:?}", record.worker, record.result);
            }
        }
        CloudflareCommands::List { provider, json } => {
            let cloudflare: CloudflareProvider = location.cloudflare(&provider)?;
            let workers = cloudflare.list().await.map_err(provider_error)?;
            if json {
                print_json(&workers);
            } else if workers.is_empty() {
                println!("No Compute Workers on provider {provider}.");
            } else {
                for worker in workers {
                    println!(
                        "{}  {}",
                        worker.worker,
                        worker.modified_on.unwrap_or_default()
                    );
                }
            }
        }
    }
    Ok(())
}

/// The pool file `compute cloudflare init` writes: this machine, and the
/// Cloudflare member below it in priority.
fn pool_file(account_id: &str, provider: &str, token_env: &str) -> String {
    format!(
        r#"# Written by `compute cloudflare init`. See docs/cloudflare.md.
# The API token is never written here: {token_env} names the variable
# that holds it.

[providers.local]
kind = "local"
priority = 100

[providers.{provider}]
kind = "cloudflare"
token_env = "{token_env}"
priority = -100

[providers.{provider}.cloudflare]
account_id = "{account_id}"
profile = "{profile}"
"#,
        profile = compute_provider_cloudflare::WORKERS_FREE,
    )
}

fn init(account_id: &str, output: &Path, provider: &str, token_env: &str) -> Result<()> {
    let text = pool_file(account_id, provider, token_env);
    // Validate before writing, so nothing invalid is ever left behind.
    compute_placement::PoolConfig::parse(&text)
        .map_err(|error| ComputeError::InvalidWorkload(error.to_string()))?;
    if output.exists() {
        println!(
            "{} exists and was left unchanged; Compute never overwrites a pool file",
            output.display()
        );
        return Ok(());
    }
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| {
            ComputeError::InvalidWorkload(format!("{}: {error}", parent.display()))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|error| ComputeError::InvalidWorkload(format!("{}: {error}", output.display())))?;
    std::io::Write::write_all(&mut file, text.as_bytes())
        .map_err(|error| ComputeError::InvalidWorkload(format!("{}: {error}", output.display())))?;
    println!("Wrote {}", output.display());
    Ok(())
}

fn print_profile(profile: &CloudflareProfile) {
    let limits = &profile.limits;
    println!("{} — {} on {}", profile.name, profile.product, profile.plan);
    println!("Runs:     short-lived, request-driven WASI modules");
    println!("Refuses:  {}", profile.unsupported().join(", "));
    println!(
        "Limits (enforced by Cloudflare; checked {}):",
        profile.limits_checked
    );
    println!("  CPU per invocation   {} ms", limits.cpu_ms_per_invocation);
    println!("  Requests per day     {}", limits.requests_per_day);
    println!(
        "  Memory per isolate   {} MiB",
        limits.memory_bytes / (1024 * 1024)
    );
    println!(
        "  Worker size          {} MiB",
        limits.worker_size_bytes / (1024 * 1024)
    );
    println!("  Workers per account  {}", limits.workers_per_account);
    println!("Source:   {}", profile.source);
    for note in &profile.notes {
        println!("Note:     {note}");
    }
}

fn print_validation(validation: &Validation<'_>, offline: bool) {
    println!(
        "Provider {} (cloudflare, {})",
        validation.provider_id, validation.profile
    );
    println!("  Configuration  {}", validation.configuration);
    println!("  Capabilities   {}", validation.capabilities);
    println!(
        "  Credential     {} ({})",
        if validation.credential_present {
            "present"
        } else {
            "missing"
        },
        validation.credential
    );
    match (&validation.account, &validation.account_error) {
        (Some(account), _) => println!(
            "  Account        {} (workers.dev subdomain {})",
            account.account_id, account.workers_dev_subdomain
        ),
        (None, Some(error)) => println!("  Account        not verified: {error}"),
        (None, None) if offline => println!("  Account        not checked (--offline)"),
        (None, None) => {}
    }
}
