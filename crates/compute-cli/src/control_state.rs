//! Choosing the daemon's control-state backend, and `compute control-plane`.
//!
//! ```toml
//! # compute.toml
//! [state]
//! backend = "feltdb"            # file (default), memory, or feltdb
//!
//! [state.feltdb]
//! url = "https://feltdb.example.com"
//! application = "app_..."       # from `compute control-plane provision`
//! environment = "production"
//! token_env = "COMPUTE_FELTDB_TOKEN"
//! ca_file = "/etc/compute/feltdb-ca.pem"   # optional: a private CA
//!
//! [state.file]
//! path = "/var/lib/compute/control-state.json"
//! artifacts = "/var/lib/compute/artifacts"
//! ```
//!
//! There is no implicit fallback: a daemon configured for FeltDB that
//! cannot reach it does not start, and never creates local state instead.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Subcommand};
use compute_core::ComputeError;
use compute_state::{ArtifactStore, ControlState, StateArtifacts, StateStore};
use compute_state_feltdb::{FeltDbConfig, FeltDbState, ProvisionRequest, provision};
use serde::Deserialize;

pub const DEFAULT_TOKEN_ENV: &str = "COMPUTE_FELTDB_TOKEN";

#[derive(Args, Debug, Clone, Default)]
pub struct StateOptions {
    /// Control-state backend: file, memory, or feltdb. Defaults to
    /// `[state] backend` in compute.toml, then file.
    #[arg(long = "state")]
    pub backend: Option<String>,
    /// Managed FeltDB URL. Defaults to `[state.feltdb] url`.
    #[arg(long)]
    pub feltdb_url: Option<String>,
    /// The Compute application in FeltDB. Defaults to
    /// `[state.feltdb] application`.
    #[arg(long)]
    pub feltdb_application: Option<String>,
    /// The FeltDB environment holding this control plane's state.
    /// Defaults to `[state.feltdb] environment`, then production.
    #[arg(long)]
    pub feltdb_environment: Option<String>,
    /// Environment variable holding the FeltDB API key. Defaults to
    /// `[state.feltdb] token_env`, then COMPUTE_FELTDB_TOKEN.
    #[arg(long)]
    pub feltdb_token_env: Option<String>,
    /// PEM certificate authority for a FeltDB under a private CA. Defaults
    /// to `[state.feltdb] ca_file`.
    #[arg(long)]
    pub feltdb_ca_file: Option<PathBuf>,
    /// Configuration file with a `[state]` section. Defaults to
    /// $COMPUTE_CONFIG, then ./compute.toml when present.
    #[arg(long = "config")]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    state: Option<StateSection>,
    #[serde(default)]
    network: Option<NetworkSection>,
    #[serde(default)]
    release: Option<ReleaseSection>,
    #[serde(default)]
    api: Option<ApiSection>,
}

/// `[api]`: how the Compute API is secured.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiSection {
    /// `production` requires TLS and operator credentials.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub tls_cert: Option<PathBuf>,
    #[serde(default)]
    pub tls_key: Option<PathBuf>,
}

/// `[network]`: endpoints, ingress, DNS providers, and certificates.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSection {
    #[serde(default)]
    pub endpoint_address: Option<std::net::IpAddr>,
    #[serde(default)]
    pub ingress_http: Option<std::net::SocketAddr>,
    #[serde(default)]
    pub ingress_https: Option<std::net::SocketAddr>,
    #[serde(default)]
    pub public_ipv4: Option<String>,
    #[serde(default)]
    pub public_ipv6: Option<String>,
    #[serde(default)]
    pub secrets_dir: Option<PathBuf>,
    #[serde(default)]
    pub dns_interval_seconds: Option<u64>,
    #[serde(default)]
    pub certificate_retry_seconds: Option<u64>,
    #[serde(default)]
    pub acme: Option<compute_environment::AcmeConfig>,
    #[serde(default)]
    pub dns: std::collections::BTreeMap<String, compute_environment::DnsProviderConfig>,
}

/// `[release]`: how releases move traffic.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseSection {
    #[serde(default)]
    pub instance_port_range: Option<String>,
    #[serde(default)]
    pub drain_timeout_ms: Option<u64>,
    #[serde(default)]
    pub switch_timeout_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateSection {
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    file: Option<FileSection>,
    #[serde(default)]
    feltdb: Option<FeltDbSection>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSection {
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    artifacts: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeltDbSection {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    application: Option<String>,
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    token_env: Option<String>,
    #[serde(default)]
    ca_file: Option<PathBuf>,
}

/// A resolved backend, ready for the daemon.
pub struct Backend {
    pub state: Arc<dyn StateStore>,
    pub artifacts: Arc<dyn ArtifactStore>,
}

fn invalid(message: impl Into<String>) -> ComputeError {
    ComputeError::InvalidWorkload(message.into())
}

impl StateOptions {
    fn config_path(&self) -> Option<PathBuf> {
        self.config
            .clone()
            .or_else(|| std::env::var_os("COMPUTE_CONFIG").map(PathBuf::from))
            .or_else(|| {
                Path::new(crate::admission::LOCAL_CONFIG)
                    .is_file()
                    .then(|| crate::admission::LOCAL_CONFIG.into())
            })
    }

    fn file(&self) -> compute_core::Result<(Option<PathBuf>, ConfigFile)> {
        let Some(path) = self.config_path() else {
            return Ok((None, ConfigFile::default()));
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|error| invalid(format!("cannot read {}: {error}", path.display())))?;
        let file: ConfigFile = toml::from_str(&text)
            .map_err(|error| invalid(format!("invalid {}: {error}", path.display())))?;
        Ok((Some(path), file))
    }

    fn section(&self) -> compute_core::Result<(Option<PathBuf>, StateSection)> {
        let (path, file) = self.file()?;
        Ok((path, file.state.unwrap_or_default()))
    }

    /// The `[network]` and `[release]` sections, with relative paths
    /// resolved against the configuration file.
    pub fn node(&self) -> compute_core::Result<(NetworkSection, ReleaseSection)> {
        let (path, file) = self.file()?;
        let mut network = file.network.unwrap_or_default();
        let base = path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let resolve = |relative: &mut PathBuf| {
            if relative.is_relative() {
                *relative = base.join(&*relative);
            }
        };
        if let Some(secrets) = &mut network.secrets_dir {
            resolve(secrets);
        }
        if let Some(acme) = &mut network.acme
            && let Some(ca_file) = &mut acme.ca_file
        {
            resolve(ca_file);
        }
        for provider in network.dns.values_mut() {
            if let compute_environment::DnsProviderConfig::File { path, .. } = provider {
                resolve(path);
            }
        }
        Ok((network, file.release.unwrap_or_default()))
    }

    /// The `[api]` section, with paths resolved against the file.
    pub fn api(&self) -> compute_core::Result<ApiSection> {
        let (path, file) = self.file()?;
        let mut api = file.api.unwrap_or_default();
        let base = path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        for path in [&mut api.tls_cert, &mut api.tls_key].into_iter().flatten() {
            if path.is_relative() {
                *path = base.join(&*path);
            }
        }
        Ok(api)
    }

    /// The FeltDB CA certificate, when one is configured.
    pub fn feltdb_ca(&self) -> compute_core::Result<Option<Vec<u8>>> {
        let (config_path, section) = self.section()?;
        let path = self.feltdb_ca_file.clone().or_else(|| {
            section
                .feltdb
                .and_then(|feltdb| feltdb.ca_file)
                .map(|path| match &config_path {
                    Some(config) if path.is_relative() => config
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new("."))
                        .join(path),
                    _ => path,
                })
        });
        path.map(|path| {
            std::fs::read(&path)
                .map_err(|error| invalid(format!("cannot read {}: {error}", path.display())))
        })
        .transpose()
    }

    /// The FeltDB connection settings: flags, then configuration.
    pub fn feltdb(&self) -> compute_core::Result<(Option<String>, Option<String>, String, String)> {
        let (_, section) = self.section()?;
        let feltdb = section.feltdb.unwrap_or_default();
        Ok((
            self.feltdb_url.clone().or(feltdb.url),
            self.feltdb_application.clone().or(feltdb.application),
            self.feltdb_environment
                .clone()
                .or(feltdb.environment)
                .unwrap_or_else(|| "production".into()),
            self.feltdb_token_env
                .clone()
                .or(feltdb.token_env)
                .unwrap_or_else(|| DEFAULT_TOKEN_ENV.into()),
        ))
    }

    /// Flags that reproduce this choice in a detached daemon.
    pub fn arguments(&self) -> Vec<String> {
        let mut arguments = vec![];
        let mut push = |flag: &str, value: &Option<String>| {
            if let Some(value) = value {
                arguments.push(flag.to_string());
                arguments.push(value.clone());
            }
        };
        push("--state", &self.backend);
        push("--feltdb-url", &self.feltdb_url);
        push("--feltdb-application", &self.feltdb_application);
        push("--feltdb-environment", &self.feltdb_environment);
        push("--feltdb-token-env", &self.feltdb_token_env);
        push(
            "--feltdb-ca-file",
            &self
                .feltdb_ca_file
                .as_ref()
                .map(|path| path.display().to_string()),
        );
        if let Some(config) = &self.config {
            arguments.push("--config".into());
            arguments.push(config.display().to_string());
        }
        arguments
    }

    /// Open the configured backend. Fails closed.
    pub async fn open(&self, state_dir: &Path) -> compute_core::Result<Backend> {
        let (config_path, section) = self.section()?;
        let relative = |path: PathBuf| match &config_path {
            Some(config) if path.is_relative() => config
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .join(path),
            _ => path,
        };
        let backend = self
            .backend
            .clone()
            .or(section.backend.clone())
            .unwrap_or_else(|| "file".into());
        match backend.as_str() {
            "file" => {
                let file = section.file.unwrap_or_default();
                let path = file
                    .path
                    .map(relative)
                    .unwrap_or_else(|| state_dir.join("control-state.json"));
                let artifacts = file
                    .artifacts
                    .map(relative)
                    .unwrap_or_else(|| state_dir.join("artifacts"));
                let state = compute_state_file::FileState::open(&path)
                    .map_err(|error| invalid(error.to_string()))?;
                Ok(Backend {
                    state: Arc::new(state),
                    artifacts: Arc::new(compute_state_file::DirectoryArtifacts::new(artifacts)),
                })
            }
            "memory" => {
                let state: Arc<dyn StateStore> = Arc::new(compute_state_memory::MemoryState::new());
                Ok(Backend {
                    artifacts: Arc::new(StateArtifacts::new(ControlState::new(state.clone()))),
                    state,
                })
            }
            "feltdb" => {
                let (url, application, environment, token_env) = self.feltdb()?;
                let url = url.ok_or_else(|| {
                    invalid("the feltdb backend needs a URL: --feltdb-url or [state.feltdb] url")
                })?;
                let application = application.ok_or_else(|| {
                    invalid(
                        "the feltdb backend needs the Compute application: --feltdb-application or [state.feltdb] application (from `compute control-plane provision`)",
                    )
                })?;
                let token = std::env::var(&token_env)
                    .map_err(|_| invalid(format!("{token_env} must hold the FeltDB API key")))?;
                let state: Arc<dyn StateStore> = Arc::new(
                    FeltDbState::connect(FeltDbConfig {
                        url,
                        token,
                        application_id: application,
                        environment,
                        ca_certificate: self.feltdb_ca()?,
                    })
                    .await
                    .map_err(|error| {
                        ComputeError::Runtime(format!(
                            "refusing to start without the configured control state: {error}"
                        ))
                    })?,
                );
                Ok(Backend {
                    artifacts: Arc::new(StateArtifacts::new(ControlState::new(state.clone()))),
                    state,
                })
            }
            other => Err(invalid(format!(
                "unknown state backend {other}; use file, memory, or feltdb"
            ))),
        }
    }
}

// ---- compute control-plane ------------------------------------------------

#[derive(Args, Debug)]
pub struct ControlPlaneCommand {
    #[command(subcommand)]
    pub command: ControlPlaneCommands,
}

#[derive(Subcommand, Debug)]
pub enum ControlPlaneCommands {
    /// Install the Compute control model in Managed FeltDB. Idempotent:
    /// an existing tenant and Compute application are reused.
    Provision {
        #[command(flatten)]
        state: StateOptions,
        /// The FeltDB tenant to create or reuse.
        #[arg(long, default_value = "compute")]
        tenant_name: String,
        /// Use this tenant ID instead of finding one by name.
        #[arg(long)]
        tenant: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Upgrade the configured Compute application to this version's model.
    Upgrade {
        #[command(flatten)]
        state: StateOptions,
        #[arg(long)]
        json: bool,
    },
}

pub async fn control_plane(command: ControlPlaneCommand) -> compute_core::Result<()> {
    let (state, tenant, tenant_name, json, upgrade) = match command.command {
        ControlPlaneCommands::Provision {
            state,
            tenant_name,
            tenant,
            json,
        } => (state, tenant, tenant_name, json, false),
        ControlPlaneCommands::Upgrade { state, json } => (state, None, String::new(), json, true),
    };
    let (url, application, environment, token_env) = state.feltdb()?;
    let url = url.ok_or_else(|| invalid("--feltdb-url or [state.feltdb] url is required"))?;
    let token = std::env::var(&token_env)
        .map_err(|_| invalid(format!("{token_env} must hold the FeltDB API key")))?;
    if upgrade && application.is_none() {
        return Err(invalid(
            "upgrade needs the Compute application: --feltdb-application or [state.feltdb] application",
        ));
    }
    let provisioned = provision(ProvisionRequest {
        url: url.clone(),
        token,
        application_id: application,
        tenant_id: tenant,
        tenant_name,
        environment: environment.clone(),
        ca_certificate: state.feltdb_ca()?,
    })
    .await
    .map_err(|error| ComputeError::Runtime(error.to_string()))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&provisioned).expect("serializable")
        );
        return Ok(());
    }
    println!(
        "Compute control model {} in FeltDB ({}).",
        if provisioned.changed {
            "installed"
        } else {
            "already current"
        },
        compute_state::STATE_VERSION
    );
    println!("Tenant: {}", provisioned.tenant_id);
    println!("Application: {}", provisioned.application_id);
    println!("Revision: {}", provisioned.revision_id);
    println!(
        "\nAdd to compute.toml:\n\n[state]\nbackend = \"feltdb\"\n\n[state.feltdb]\nurl = \"{url}\"\napplication = \"{}\"\nenvironment = \"{environment}\"\ntoken_env = \"{token_env}\"",
        provisioned.application_id
    );
    Ok(())
}
