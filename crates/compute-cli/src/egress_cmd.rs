//! `compute egress`: run the egress gateway (see `compute-egress`).
//!
//! The rules file says where a session may connect and which hosts get a
//! credential; it names each credential's *variable*, never its value, so
//! the file can be reviewed and stored like any other configuration. The
//! values come from this process's environment and stay in the gateway.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use compute_core::ComputeError;
use serde::Deserialize;

#[derive(Args, Debug)]
pub struct EgressCommand {
    #[command(subcommand)]
    command: EgressCommands,
}

#[derive(Subcommand, Debug)]
enum EgressCommands {
    /// Serve an egress gateway: sessions reach the network only through it.
    Serve(ServeArgs),
}

#[derive(Args, Debug)]
struct ServeArgs {
    /// The gateway's rules (JSON): `allow`, `allow_private`, `credentials`.
    #[arg(long)]
    rules: PathBuf,
    #[arg(long, default_value = "127.0.0.1:3128")]
    listen: SocketAddr,
    /// Write the gateway's CA certificate here, for sessions to trust. It
    /// can sign only for hosts that have a credential.
    #[arg(long)]
    ca_out: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default)]
    allow: Vec<compute_egress::HostRule>,
    #[serde(default)]
    allow_private: bool,
    #[serde(default)]
    credentials: Vec<CredentialReference>,
    /// Fixed addresses for names, instead of DNS.
    #[serde(default)]
    resolve: BTreeMap<String, Vec<SocketAddr>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialReference {
    host: String,
    header: String,
    /// The environment variable that holds the value.
    value_env: String,
    /// Written before the value, such as `Bearer `.
    #[serde(default)]
    prefix: String,
}

pub async fn egress(command: EgressCommand) -> compute_core::Result<()> {
    let EgressCommands::Serve(args) = command.command;
    let file: PolicyFile = serde_json::from_slice(&std::fs::read(&args.rules)?)
        .map_err(|error| ComputeError::InvalidWorkload(format!("egress rules: {error}")))?;
    let bindings = file
        .credentials
        .into_iter()
        .map(|reference| {
            let value = std::env::var(&reference.value_env).map_err(|_| {
                ComputeError::InvalidWorkload(format!(
                    "the credential for {} needs {}, which is not set",
                    reference.host, reference.value_env
                ))
            })?;
            Ok(compute_egress::CredentialBinding {
                host: reference.host,
                header: reference.header,
                value: format!("{}{value}", reference.prefix),
            })
        })
        .collect::<compute_core::Result<Vec<_>>>()?;
    let gateway = compute_egress::Gateway::start(
        args.listen,
        compute_egress::GatewayConfig {
            policy: compute_egress::EgressPolicy {
                allow: file.allow,
                allow_private: file.allow_private,
            },
            bindings,
            resolve: file.resolve,
            upstream_roots: None,
        },
    )
    .await
    .map_err(|error| ComputeError::InvalidWorkload(error.to_string()))?;
    if let Some(path) = &args.ca_out {
        std::fs::write(path, gateway.ca_pem())?;
    }
    println!(
        "Compute egress gateway on http://{} (HTTPS_PROXY / HTTP_PROXY)",
        gateway.address()
    );
    tokio::signal::ctrl_c().await?;
    for decision in gateway.decisions() {
        eprintln!("{}", serde_json::to_string(&decision).unwrap_or_default());
    }
    Ok(())
}
