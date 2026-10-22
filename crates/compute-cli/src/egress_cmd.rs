//! `compute egress`: run the egress gateway (see `compute-egress`).
//!
//! The rules file says where a session may connect and which hosts get a
//! credential; it names where each credential is kept, never its value, so
//! the file can be reviewed and stored like any other configuration (a rules
//! file that holds a value is refused).
//!
//! **Custody is local.** Values come from this process's environment
//! (`value_env`) or an owner-only file (`value_file`) and stay in the
//! gateway's memory. That keeps them out of sessions, rules files, and job
//! requests; it is not production-grade custody, and the gateway says so
//! when it starts. Custody mediated by AuthBoundry is a separate
//! integration.

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
    #[serde(default)]
    value_env: Option<String>,
    /// A file, readable only by its owner, that holds the value.
    #[serde(default)]
    value_file: Option<PathBuf>,
    /// Written before the value, such as `Bearer `.
    #[serde(default)]
    prefix: String,
}

pub async fn egress(command: EgressCommand) -> compute_core::Result<()> {
    let EgressCommands::Serve(args) = command.command;
    let file: PolicyFile = serde_json::from_slice(&std::fs::read(&args.rules)?)
        .map_err(|error| ComputeError::InvalidWorkload(format!("egress rules: {error}")))?;
    let custody = match (
        file.credentials.is_empty(),
        file.credentials.iter().any(|c| c.value_file.is_some()),
    ) {
        (true, _) => "none (no credentials)",
        (false, false) => "local: the gateway's environment",
        (false, true) => "local: the gateway's environment and owner-only files",
    };
    let bindings = file
        .credentials
        .into_iter()
        .map(|reference| {
            let value = resolve(&reference)?;
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
    println!("  credential custody: {custody}");
    if custody.starts_with("local") {
        println!(
            "  local custody keeps values out of sessions, rules and job requests; it is not production-grade custody"
        );
    }
    tokio::signal::ctrl_c().await?;
    for decision in gateway.decisions() {
        eprintln!("{}", serde_json::to_string(&decision).unwrap_or_default());
    }
    Ok(())
}

/// A credential's value, from the one place its reference names.
fn resolve(reference: &CredentialReference) -> compute_core::Result<String> {
    let invalid = |message: String| ComputeError::InvalidWorkload(message);
    match (&reference.value_env, &reference.value_file) {
        (Some(name), None) => std::env::var(name).map_err(|_| {
            invalid(format!(
                "the credential for {} needs {name}, which is not set",
                reference.host
            ))
        }),
        (None, Some(path)) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(path)?.permissions().mode();
                if mode & 0o077 != 0 {
                    return Err(invalid(format!(
                        "the credential file {} is readable by others (mode {:o}); it must be owner-only",
                        path.display(),
                        mode & 0o777
                    )));
                }
            }
            let value = std::fs::read_to_string(path)?;
            let value = value.trim_end_matches(['\r', '\n']).to_owned();
            if value.is_empty() {
                return Err(invalid(format!(
                    "the credential file {} is empty",
                    path.display()
                )));
            }
            Ok(value)
        }
        _ => Err(invalid(format!(
            "the credential for {} names exactly one of value_env or value_file",
            reference.host
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(text: &str) -> Result<PolicyFile, serde_json::Error> {
        serde_json::from_str(text)
    }

    #[test]
    fn a_rules_file_names_where_a_credential_is_never_its_value() {
        let raw =
            r#"{"credentials":[{"host":"api.test","header":"authorization","value":"s3cret"}]}"#;
        assert!(rules(raw).is_err(), "a value in the rules file is refused");
        let both = rules(
            r#"{"credentials":[{"host":"a","header":"x","value_env":"A","value_file":"/f"}]}"#,
        )
        .unwrap();
        assert!(resolve(&both.credentials[0]).is_err());
        let neither = rules(r#"{"credentials":[{"host":"a","header":"x"}]}"#).unwrap();
        assert!(resolve(&neither.credentials[0]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_credential_file_must_be_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        std::fs::write(&path, "s3cret\n").unwrap();
        let reference = CredentialReference {
            host: "api.test".into(),
            header: "authorization".into(),
            value_env: None,
            value_file: Some(path.clone()),
            prefix: String::new(),
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(resolve(&reference).is_err(), "readable by others");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(resolve(&reference).unwrap(), "s3cret");
    }
}
