//! Running the configured agent service for the Work page, so nothing has to
//! be set by hand.
//!
//! A configured distribution declares its agent runtime in its profile
//! ([`crate::agents`]): an executable, and `serve` as the command that starts
//! its HTTP service. This module starts that service on loopback, on demand,
//! for the project a goal is about, and reports what it is doing. Compute does
//! not know how the agent works; it supplies the environment the service's own
//! contract asks for (`docs/stack-agent.md`):
//!
//! | The service needs | Compute supplies |
//! | --- | --- |
//! | a target to run sessions on | the pool's first remote member and its token file |
//! | the project each session clones | the repository of the environment the goal is for |
//! | its worker executable | the one the profile names |
//! | a model | what the operator saved here, else the daemon's own `CHIP_*` |
//!
//! The model credential is the one thing Compute cannot derive. It is kept in
//! a file only the daemon's user can read, in the node's state directory, and
//! is never returned by the API. Nothing else is stored: the running services
//! are working state, rebuilt on demand.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::EnvironmentError;

const STARTUP: Duration = Duration::from_secs(30);
const LOG_TAIL_BYTES: usize = 2000;

/// The model the agent uses. `api_key` is optional (a local model has none).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSettings {
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
}

impl ModelSettings {
    fn complete(&self) -> bool {
        !self.provider.trim().is_empty()
            && !self.model.trim().is_empty()
            && !self.endpoint.trim().is_empty()
    }

    /// What the page may show: everything but the key.
    fn public(&self, source: &str) -> Value {
        json!({
            "configured": self.complete(),
            "source": source,
            "provider": self.provider,
            "model": self.model,
            "endpoint": self.endpoint,
            "api_key_set": !self.api_key.is_empty(),
        })
    }

    fn variables(&self) -> Vec<(&'static str, String)> {
        let mut variables = vec![
            ("CHIP_PROVIDER", self.provider.clone()),
            ("CHIP_MODEL", self.model.clone()),
            ("CHIP_ENDPOINT", self.endpoint.clone()),
        ];
        if !self.api_key.is_empty() {
            variables.push(("CHIP_API_KEY", self.api_key.clone()));
        }
        variables
    }
}

/// Where the service runs and with what: derived once from the daemon.
#[derive(Debug, Clone)]
pub struct AgentSetup {
    /// The distribution's install root (`COMPUTE_CONFIGURED_HOME`).
    pub home: PathBuf,
    /// The profile's executable, relative to `home`.
    pub executable: PathBuf,
    /// Where the node keeps its own data; the model file and the services'
    /// logs and work records live under `agent/` here.
    pub state_dir: PathBuf,
    /// The pool member sessions run on, and its credential file.
    pub target: Option<(String, PathBuf)>,
}

struct Running {
    child: Child,
    url: String,
}

/// The services this daemon has started, one per project.
pub struct AgentRuntime {
    setup: Option<AgentSetup>,
    running: Mutex<BTreeMap<String, Running>>,
}

fn identity(project: &str) -> String {
    let digest = Sha256::digest(project.as_bytes());
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A Homebrew install lays `compute` and `compute-configured` side by side
/// (`<prefix>/Cellar/<formula>/<version>/…`, linked at `<prefix>/opt/<formula>`),
/// so a daemon started as plain `compute start` still finds the configured
/// install's root without being told. Only an install that has a profile counts.
fn homebrew_sibling() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?.canonicalize().ok()?;
    let cellar = executable
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "Cellar"))?;
    let home = cellar.parent()?.join("opt/compute-configured/libexec");
    home.join("stack.json").is_file().then_some(home)
}

impl AgentRuntime {
    pub fn new(setup: Option<AgentSetup>) -> Arc<Self> {
        Arc::new(Self {
            setup,
            running: Mutex::new(BTreeMap::new()),
        })
    }

    /// The setup a configured installation implies, or `None` on base
    /// Compute, which ships no agent.
    pub fn setup_for(state_dir: &Path, target: Option<(String, PathBuf)>) -> Option<AgentSetup> {
        let home = crate::agents::distribution_home().or_else(homebrew_sibling)?;
        let capabilities = crate::agents::declared(&home);
        let runtime = capabilities.default_runtime()?;
        let executable = PathBuf::from(&runtime.executable);
        home.join(&executable).is_file().then(|| AgentSetup {
            home,
            executable,
            state_dir: state_dir.to_path_buf(),
            target,
        })
    }

    fn directory(setup: &AgentSetup) -> PathBuf {
        setup.state_dir.join("agent")
    }

    fn model_file(setup: &AgentSetup) -> PathBuf {
        Self::directory(setup).join("model.json")
    }

    /// The model in use and where it came from: what was saved here, else the
    /// daemon's own environment.
    pub fn model(&self) -> (ModelSettings, &'static str) {
        if let Some(setup) = &self.setup
            && let Ok(text) = std::fs::read_to_string(Self::model_file(setup))
            && let Ok(saved) = serde_json::from_str::<ModelSettings>(&text)
            && saved.complete()
        {
            return (saved, "saved");
        }
        let from_environment = ModelSettings {
            provider: std::env::var("CHIP_PROVIDER").unwrap_or_default(),
            model: std::env::var("CHIP_MODEL").unwrap_or_default(),
            endpoint: std::env::var("CHIP_ENDPOINT").unwrap_or_default(),
            api_key: std::env::var("CHIP_API_KEY").unwrap_or_default(),
        };
        if from_environment.complete() {
            (from_environment, "environment")
        } else {
            (ModelSettings::default(), "none")
        }
    }

    /// Save the model, readable by the daemon's user only, and stop the
    /// services so the next goal starts them with it.
    pub async fn set_model(&self, settings: ModelSettings) -> Result<(), EnvironmentError> {
        let setup = self
            .setup
            .as_ref()
            .ok_or_else(|| EnvironmentError::Invalid("this installation ships no agent".into()))?;
        if !settings.complete() {
            return Err(EnvironmentError::Invalid(
                "a provider, a model and an endpoint are required".into(),
            ));
        }
        let directory = Self::directory(setup);
        std::fs::create_dir_all(&directory)?;
        let file = Self::model_file(setup);
        // Written to a private file first, then moved into place, so the key is
        // never readable by anyone else, even briefly.
        let staged = directory.join(".model.json.new");
        {
            use std::io::Write;
            #[cfg(unix)]
            use std::os::unix::fs::OpenOptionsExt;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut handle = options.open(&staged)?;
            handle.write_all(serde_json::to_string(&settings)?.as_bytes())?;
        }
        std::fs::rename(&staged, &file)?;
        self.stop_all().await;
        Ok(())
    }

    pub async fn stop_all(&self) {
        let mut running = self.running.lock().await;
        for (_, mut service) in std::mem::take(&mut *running) {
            let _ = service.child.kill().await;
        }
    }

    fn log_path(setup: &AgentSetup, key: &str) -> PathBuf {
        Self::directory(setup).join(key).join("agent.log")
    }

    fn log_tail(setup: &AgentSetup, key: &str) -> String {
        let text = std::fs::read_to_string(Self::log_path(setup, key)).unwrap_or_default();
        let start = text.len().saturating_sub(LOG_TAIL_BYTES);
        let start = (start..=text.len())
            .find(|index| text.is_char_boundary(*index))
            .unwrap_or(text.len());
        text[start..].trim().to_owned()
    }

    /// What the page needs to say: whether this installation ships an agent,
    /// whether the model is set, and whether a service is running for the
    /// project.
    pub async fn status(&self, project: Option<&str>) -> Value {
        let Some(setup) = &self.setup else {
            return json!({ "native": false });
        };
        let (model, source) = self.model();
        let mut running = self.running.lock().await;
        let service = project.and_then(|project| {
            let key = identity(project);
            let alive = running
                .get_mut(&key)
                .is_some_and(|service| matches!(service.child.try_wait(), Ok(None)));
            alive.then(|| running[&key].url.clone())
        });
        json!({
            "native": true,
            "project": project,
            "running": service.is_some(),
            "url": service,
            "target": setup.target.as_ref().map(|(endpoint, _)| endpoint),
            "model": model.public(source),
        })
    }

    /// The address of the service for a project, starting it if it is not
    /// running. Fails with what is missing or with the service's own words.
    pub async fn ensure(&self, project: &str) -> Result<String, EnvironmentError> {
        let setup = self.setup.as_ref().ok_or_else(|| {
            EnvironmentError::RuntimeUnavailable("this installation ships no agent".into())
        })?;
        let (model, _) = self.model();
        if !model.complete() {
            return Err(EnvironmentError::Invalid(
                "the agent has no model yet: set one (a provider, a model and an endpoint) on the Work page".into(),
            ));
        }
        let (endpoint, token_file) = setup.target.clone().ok_or_else(|| {
            EnvironmentError::RuntimeUnavailable(
                "this controller has no computer host with a credential file to run the agent's sessions on".into(),
            )
        })?;
        let key = identity(project);
        let mut running = self.running.lock().await;
        if let Some(service) = running.get_mut(&key) {
            if matches!(service.child.try_wait(), Ok(None)) {
                return Ok(service.url.clone());
            }
            running.remove(&key);
        }

        let directory = Self::directory(setup).join(&key);
        std::fs::create_dir_all(&directory)?;
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            listener.local_addr()?.port()
        };
        let log = std::fs::File::create(Self::log_path(setup, &key))?;
        let worker = setup.home.join(&setup.executable);
        let mut command = Command::new(&worker);
        // The service starts from an empty environment: only what it and its
        // Compute client need, the way the distribution's own launcher does.
        command.env_clear();
        for name in ["PATH", "HOME", "TMPDIR", "LANG", "COMPUTE_HOME"] {
            if let Ok(value) = std::env::var(name) {
                command.env(name, value);
            }
        }
        command
            .env("COMPUTE_STACK_AGENT_WORKER", &worker)
            .env("COMPUTE_STACK_AGENT_TARGET", &endpoint)
            .env("COMPUTE_STACK_AGENT_TARGET_TOKEN_FILE", &token_file)
            .env("COMPUTE_STACK_AGENT_PROJECT", project)
            .env(
                "COMPUTE_STACK_AGENT_COMMAND_PATH",
                std::env::var("PATH").unwrap_or_default(),
            )
            .env(
                "COMPUTE_STACK_AGENT_STATE_DB",
                directory.join("works.feltdb"),
            );
        for (name, value) in model.variables() {
            command.env(name, value);
        }
        command
            .args(["serve", "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            EnvironmentError::RuntimeUnavailable(format!(
                "could not start the agent ({}): {error}",
                worker.display()
            ))
        })?;
        let url = format!("http://127.0.0.1:{port}");

        // Ready when it answers; failed when it exits first or never does.
        let deadline = tokio::time::Instant::now() + STARTUP;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(EnvironmentError::RuntimeUnavailable(format!(
                    "the agent exited as it started ({status}): {}",
                    Self::log_tail(setup, &key)
                )));
            }
            if crate::agent_service::status_of(&url).await {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                let _ = child.kill().await;
                return Err(EnvironmentError::RuntimeUnavailable(format!(
                    "the agent did not answer within {}s: {}",
                    STARTUP.as_secs(),
                    Self::log_tail(setup, &key)
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        running.insert(
            key,
            Running {
                child,
                url: url.clone(),
            },
        );
        Ok(url)
    }
}
