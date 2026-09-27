//! Environments on a computer.
//!
//! ```text
//! Environment (desired: computer spec, contents)     ── operators write
//!     │
//!     ▼  placement over the daemon's pool: a target, never a provider name
//! Computer (lifecycle, target, session, observed)     ── this controller writes
//!     │
//!     ▼  compute.remote@1 sessions on the target
//! the target's SessionManager and SessionProvider     ── build the machine
//!     │
//!     ▼  durable jobs inside the machine
//! repositories · packages · processes                 ── changed in place
//! ```
//!
//! Every lifecycle step is written to durable state before the target is
//! asked to act, and a target's answer is applied only if the computer is
//! still where the step left it (a fenced write). A target never decides
//! anything: it provisions, runs jobs, and tears down. Changing what a
//! computer holds is a change to desired state, reconciled by jobs run in
//! the running computer; it never provisions a replacement. Only changing
//! the computer's requirements does, and that is an explicit replacement.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use compute_core::{
    ComputerFailure, ComputerLifecycle, ComputerRequirements, ComputerSpec, ComputerStatus,
    EnvironmentContents, ObservedBuild, ObservedContents, ObservedPackage, ObservedProcess,
    ObservedRepository, OperationEvidence, PackageSpec, ProcessDesired, ProcessSpec, ProcessState,
    ProjectSpec, RepositorySpec, RetiredSession, SessionCommand, SessionStatus, fingerprint,
};
use compute_placement::{
    AdmissionContext, DiscoveryMode, PlacementOutcome, PlacementPolicy, PlacementReport,
    PlacementRequirements, dispatch, place_with_policy,
};
use compute_policy::ExecutionContract;
use compute_provider::{ProviderErrorKind, RemoteProvider, SessionCreateRequest};
use compute_state::events;
use compute_state::{ComputerRecord, EnvironmentRecord, Stored, ids};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Change, Daemon, Scope};
use crate::EnvironmentError;
use crate::model::*;
use crate::status::*;

/// Ephemeral computers live an hour unless asked otherwise.
pub(crate) const DEFAULT_TTL: u64 = 60 * 60;
/// A target keeps an ephemeral session this long past the computer's own
/// expiry, so Compute, not the target, decides when it ends.
const TARGET_GRACE: u64 = 5 * 60;
const POLL: Duration = Duration::from_millis(150);
const BACKOFF: Duration = Duration::from_secs(2);

/// What a driver does next.
enum Step {
    Continue,
    Wait(Duration),
    Done,
}

/// A command run in an environment's computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputerExec {
    pub environment: String,
    pub target: String,
    pub session_id: String,
    pub job_id: String,
    pub execution_id: String,
    pub status: compute_core::JobStatus,
}

/// A durable job run in an environment's computer, and its result once it
/// has one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputerJob {
    pub job: compute_core::ExecutionJob,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<compute_core::JobResult>,
}

/// The next change the computer needs to hold what the environment asks for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Action {
    SyncRepository(RepositorySpec, String),
    RemoveRepository(String),
    InstallPackage(PackageSpec, String),
    ForgetPackage(String),
    Build(ProjectSpec, String),
    ForgetBuild(String),
    StopProcess { name: String, forget: bool },
    StartProcess(ProcessSpec, String),
}

fn commit_of(repository: &str, observed: &ObservedContents) -> Option<String> {
    observed
        .repositories
        .get(repository)
        .and_then(|repository| repository.commit.clone())
}

/// A project build's fingerprint: its command, its repository's commit,
/// and the configuration it sees. Any of them changing builds again.
fn build_fingerprint(
    project: &ProjectSpec,
    observed: &ObservedContents,
    config: &BTreeMap<String, String>,
) -> String {
    fingerprint(&(
        &project.build,
        &project.repository,
        commit_of(&project.repository, observed),
        config,
    ))
}

/// The environment a process, build, or command sees: the environment's
/// configuration, then its own.
pub(crate) fn process_env(
    process: &ProcessSpec,
    config: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = config.clone();
    if let Some(port) = process.port {
        env.insert("PORT".into(), port.to_string());
    }
    env.extend(process.env.clone());
    env
}

/// The desired fingerprint of a process: its spec, the commit of the
/// repository it runs from, and the environment configuration it sees. Any
/// of them changing restarts it.
fn process_fingerprint(
    process: &ProcessSpec,
    contents: &EnvironmentContents,
    observed: &ObservedContents,
    config: &BTreeMap<String, String>,
) -> String {
    let commit = process
        .repository
        .as_ref()
        .and_then(|name| commit_of(name, observed));
    // A new build of the repository it runs from restarts it too.
    let builds = process
        .repository
        .as_ref()
        .map(|repository| {
            contents
                .projects
                .iter()
                .filter(|project| &project.repository == repository && !project.build.is_empty())
                .map(|project| build_fingerprint(project, observed, config))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if builds.is_empty() {
        fingerprint(&(process, commit, config))
    } else {
        fingerprint(&(process, commit, config, builds))
    }
}

fn package_fingerprint(package: &PackageSpec, observed: &ObservedContents) -> String {
    let commit = package
        .repository
        .as_ref()
        .and_then(|name| observed.repositories.get(name))
        .and_then(|repository| repository.commit.clone());
    fingerprint(&(package, commit))
}

/// Plan the next action, in order: repositories, packages, processes to
/// stop, processes to start. A failed attempt at an unchanged item is not
/// retried until the item changes or a reconcile is requested.
pub(crate) fn plan(
    contents: &EnvironmentContents,
    observed: &ObservedContents,
    config: &BTreeMap<String, String>,
) -> Option<Action> {
    for repository in &contents.repositories {
        let wanted = fingerprint(repository);
        if observed
            .repositories
            .get(&repository.name)
            .is_none_or(|seen| seen.fingerprint != wanted)
        {
            return Some(Action::SyncRepository(repository.clone(), wanted));
        }
    }
    for name in observed.repositories.keys() {
        if !contents.repositories.iter().any(|spec| &spec.name == name) {
            return Some(Action::RemoveRepository(name.clone()));
        }
    }
    for package in &contents.packages {
        let wanted = package_fingerprint(package, observed);
        if observed
            .packages
            .get(&package.name)
            .is_none_or(|seen| seen.fingerprint != wanted)
        {
            return Some(Action::InstallPackage(package.clone(), wanted));
        }
    }
    for name in observed.packages.keys() {
        if !contents.packages.iter().any(|spec| &spec.name == name) {
            return Some(Action::ForgetPackage(name.clone()));
        }
    }
    for project in contents
        .projects
        .iter()
        .filter(|project| !project.build.is_empty())
    {
        let wanted = build_fingerprint(project, observed, config);
        if observed
            .builds
            .get(&project.name)
            .is_none_or(|seen| seen.fingerprint != wanted)
        {
            return Some(Action::Build(project.clone(), wanted));
        }
    }
    for name in observed.builds.keys() {
        if !contents
            .projects
            .iter()
            .any(|spec| &spec.name == name && !spec.build.is_empty())
        {
            return Some(Action::ForgetBuild(name.clone()));
        }
    }
    // A repository whose current build failed keeps what runs from it as
    // it is: a failed release never takes down the running one.
    let broken = |repository: &Option<String>| {
        repository.as_ref().is_some_and(|repository| {
            contents.projects.iter().any(|project| {
                &project.repository == repository
                    && observed
                        .builds
                        .get(&project.name)
                        .is_some_and(|seen| seen.evidence.outcome != "succeeded")
            })
        })
    };
    for (name, seen) in &observed.processes {
        match contents.processes.iter().find(|spec| &spec.name == name) {
            None => {
                return Some(Action::StopProcess {
                    name: name.clone(),
                    forget: true,
                });
            }
            Some(spec)
                if spec.desired == ProcessDesired::Stopped
                    && seen.state == ProcessState::Running =>
            {
                return Some(Action::StopProcess {
                    name: name.clone(),
                    forget: false,
                });
            }
            Some(_) => {}
        }
    }
    for process in &contents.processes {
        if process.desired != ProcessDesired::Running {
            continue;
        }
        let wanted = process_fingerprint(process, contents, observed, config);
        let current = observed.processes.get(&process.name);
        if broken(&process.repository) {
            continue;
        }
        let settled = current.is_some_and(|seen| {
            seen.fingerprint == wanted
                && matches!(seen.state, ProcessState::Running | ProcessState::Failed)
        });
        if !settled {
            return Some(Action::StartProcess(process.clone(), wanted));
        }
    }
    None
}

// The commands a controller runs in a computer. Each runs from the
// computer's workspace as a durable job; arguments are passed as
// positional parameters, never interpolated into the script.

const SYNC_REPOSITORY: &str = r#"set -eu
dir="repos/$1"; url="$2"; revision="$3"
mkdir -p repos
if [ ! -d "$dir/.git" ]; then
  rm -rf "$dir"
  git clone --quiet "$url" "$dir"
fi
cd "$dir"
git remote set-url origin "$url"
git fetch --quiet --tags --force origin
if git rev-parse --verify --quiet "refs/remotes/origin/$revision^{commit}" >/dev/null; then
  target="refs/remotes/origin/$revision"
else
  target="$revision"
fi
git -c advice.detachedHead=false checkout --quiet --force --detach "$target"
git rev-parse HEAD
"#;

const REMOVE_REPOSITORY: &str = r#"rm -rf "repos/$1""#;

pub(crate) const INSTALL_PACKAGE: &str = r#"set -eu
if [ -n "$1" ]; then cd "repos/$1"; fi
shift
exec "$@"
"#;

const START_PROCESS: &str = r#"set -eu
name="$1"; repository="$2"; shift 2
root="$PWD"
mkdir -p .compute/processes
pidfile="$root/.compute/processes/$name.pid"
log="$root/.compute/processes/$name.log"
if [ -f "$pidfile" ]; then
  old="$(cat "$pidfile")"
  kill -TERM "-$old" 2>/dev/null || kill -TERM "$old" 2>/dev/null || true
  i=0
  while kill -0 "$old" 2>/dev/null && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
  kill -KILL "-$old" 2>/dev/null || true
  rm -f "$pidfile"
fi
if [ -n "$repository" ]; then cd "repos/$repository"; fi
setsid "$@" >"$log" 2>&1 </dev/null &
echo "$!" >"$pidfile"
sleep 0.3
pid="$(cat "$pidfile")"
if kill -0 "$pid" 2>/dev/null; then
  echo "$pid"
else
  echo "the process exited as it started:" >&2
  tail -n 20 "$log" >&2
  exit 1
fi
"#;

const STOP_PROCESS: &str = r#"pidfile=".compute/processes/$1.pid"
if [ -f "$pidfile" ]; then
  pid="$(cat "$pidfile")"
  kill -TERM "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
  i=0
  while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
  kill -KILL "-$pid" 2>/dev/null || true
  rm -f "$pidfile"
fi
"#;

const PROBE_PROCESSES: &str = r#"for pidfile in .compute/processes/*.pid; do
  [ -e "$pidfile" ] || continue
  name="$(basename "$pidfile" .pid)"
  pid="$(cat "$pidfile")"
  if kill -0 "$pid" 2>/dev/null && ! grep -q '^State:.*Z' "/proc/$pid/status" 2>/dev/null; then
    echo "$name running $pid"
  else
    echo "$name exited"
  fi
done
"#;

const PROCESS_LOG: &str = r#"tail -n "$2" ".compute/processes/$1.log" 2>/dev/null || true"#;

pub(crate) fn script(script: &str, arguments: impl IntoIterator<Item = String>) -> SessionCommand {
    let mut command = vec![
        "sh".to_owned(),
        "-c".into(),
        script.into(),
        "compute".into(),
    ];
    command.extend(arguments);
    SessionCommand::new(command)
}

impl Daemon {
    // ---- Operations: every one is authorized by the API's scope and here
    // bound to the environment's owner. --------------------------------

    /// Create an environment on a computer. Placement runs first: a
    /// computer no target can host is refused and nothing is written.
    pub async fn create_computer_environment(
        self: &Arc<Self>,
        definition: ComputerEnvironmentDefinition,
        operator: &str,
    ) -> Result<EnvironmentView, EnvironmentError> {
        validate_name("environment", &definition.name)?;
        validate_env("environment", &definition.env)?;
        if let Some(policy) = &definition.policy {
            policy
                .validate()
                .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
        }
        definition.computer.requirements.validate()?;
        definition.contents.validate()?;
        if let Some(target) = &definition.computer.target
            && self.pool.member(target).is_none()
        {
            return Err(EnvironmentError::Invalid(format!(
                "target {target} is not in the daemon's pool"
            )));
        }
        if definition.computer.lifecycle == ComputerLifecycle::Persistent
            && definition.computer.ttl_seconds.is_some()
        {
            return Err(EnvironmentError::Invalid(
                "a persistent computer has no TTL".into(),
            ));
        }
        if definition.computer.ttl_seconds == Some(0) {
            return Err(EnvironmentError::Invalid(
                "a computer's TTL must be greater than zero".into(),
            ));
        }
        self.refresh().await?;
        if self
            .inner
            .lock()
            .await
            .desired
            .environments
            .contains_key(&definition.name)
        {
            return Err(EnvironmentError::Conflict(format!(
                "environment {} already exists",
                definition.name
            )));
        }
        let created_at = Utc::now();
        let ttl_seconds = (definition.computer.lifecycle == ComputerLifecycle::Ephemeral)
            .then(|| definition.computer.ttl_seconds.unwrap_or(DEFAULT_TTL));
        let spec = ComputerSpec {
            lifecycle: definition.computer.lifecycle,
            requirements: definition.computer.requirements.clone(),
            target: definition.computer.target.clone(),
            ttl_seconds,
            expires_at: ttl_seconds.map(|ttl| {
                created_at + chrono::Duration::seconds(i64::try_from(ttl).unwrap_or(i64::MAX))
            }),
            generation: 1,
            destroy_requested_at: None,
        };
        let mut contents = definition.contents.clone();
        contents.generation = 1;
        let record = EnvironmentRecord {
            name: definition.name.clone(),
            desired_state: definition.desired_state,
            config: definition.env.clone(),
            policy: definition
                .policy
                .clone()
                .map(compute_policy::Policy::canonical)
                .map(|policy| serde_json::to_value(policy).expect("policies serialize")),
            provider: None,
            created_at,
            owner: Some(operator.to_owned()),
            computer: Some(spec.clone()),
            contents: Some(contents),
        };
        // Refused here, with every target's reasons, when nothing can host it.
        let report = self.place_computer(&record, &spec).await?;
        let id = format!(
            "env_{}",
            compute_state::short_digest(&[
                &definition.name,
                &created_at
                    .timestamp_nanos_opt()
                    .unwrap_or_default()
                    .to_string(),
                &self.instance_id,
            ])
        );
        let computer = ComputerRecord {
            environment_id: id.clone(),
            environment: definition.name.clone(),
            owner: operator.to_owned(),
            lifecycle: spec.lifecycle,
            status: ComputerStatus::Pending,
            generation: 1,
            spec_generation: spec.generation,
            target: None,
            placement_id: None,
            session_id: None,
            reference: None,
            provider_kind: None,
            provider_resource: None,
            capabilities: None,
            connection: None,
            retired: vec![],
            observed: ObservedContents::default(),
            failure: None,
            created_at,
            updated_at: created_at,
            ready_at: None,
            ended_at: None,
            expires_at: spec.expires_at,
        };
        let change = Change::new().with(|batch| {
            batch
                .create(&id, &record)
                .create(&ids::computer(&id), &computer)
        });
        let change = self.event(
            change,
            events::ENVIRONMENT_CREATED,
            Scope::environment(&definition.name),
            format!("Environment {} created", definition.name),
            json!({ "environment_id": id }),
        );
        let change = self.event(
            change,
            events::COMPUTER_REQUESTED,
            Scope::environment(&definition.name),
            format!(
                "A {} computer was requested for {}",
                spec.lifecycle.as_str(),
                definition.name
            ),
            json!({
                "environment_id": id,
                "owner": operator,
                "requirements": spec.requirements,
                "candidates": report.compatible_providers,
                "expires_at": spec.expires_at,
            }),
        );
        self.apply(change).await?;
        self.changed().await;
        self.environment(&definition.name).await
    }

    /// The computer's view, for any operator that may read environments.
    pub async fn computer(&self, environment: &str) -> Result<ComputerView, EnvironmentError> {
        self.refresh_for_read().await?;
        let record = self
            .inner
            .lock()
            .await
            .desired
            .environment(environment)
            .cloned()
            .ok_or_else(|| EnvironmentError::NotFound(format!("environment {environment}")))?;
        self.computer_view_of(&record).await.ok_or_else(|| {
            EnvironmentError::NotFound(format!("environment {environment} has no computer"))
        })
    }

    pub(crate) async fn computer_view_of(
        &self,
        record: &Stored<EnvironmentRecord>,
    ) -> Option<ComputerView> {
        let spec = record.value.computer.clone()?;
        let computer = self
            .inner
            .lock()
            .await
            .desired
            .computers
            .get(&record.id)
            .cloned()?;
        let desired = record.value.contents.clone().unwrap_or_default();
        let value = computer.value;
        let converged = value.status == ComputerStatus::Running
            && value.spec_generation == spec.generation
            && value.observed.converged_generation == desired.generation
            && plan(&desired, &value.observed, &record.value.config).is_none();
        let machine = match (&value.target, &value.session_id) {
            (Some(target), Some(session_id)) => Some(MachineView {
                target: target.clone(),
                session_id: session_id.clone(),
                provider_kind: value.provider_kind.clone(),
                resource: value.provider_resource.clone(),
            }),
            _ => None,
        };
        let host = value
            .target
            .as_ref()
            .and_then(|target| self.target_host(target));
        let endpoints = desired
            .processes
            .iter()
            .filter_map(|process| {
                let port = process.port?;
                Some(ProcessEndpoint {
                    process: process.name.clone(),
                    port,
                    url: host.as_ref().map(|host| format!("http://{host}:{port}")),
                    serving: value.status == ComputerStatus::Running
                        && value
                            .observed
                            .processes
                            .get(&process.name)
                            .is_some_and(|seen| seen.state == ProcessState::Running),
                })
            })
            .collect();
        Some(ComputerView {
            environment: record.value.name.clone(),
            environment_id: record.id.clone(),
            owner: value.owner,
            lifecycle: value.lifecycle,
            requested_lifecycle: spec.lifecycle,
            machine,
            config: record.value.config.clone(),
            endpoints,
            status: value.status,
            requirements: spec.requirements,
            spec_generation: spec.generation,
            running_generation: value.spec_generation,
            target: value.target,
            requested_target: spec.target,
            placement_id: value.placement_id,
            session_id: value.session_id,
            provider_kind: value.provider_kind,
            capabilities: value.capabilities,
            connection: value.connection,
            desired,
            observed: value.observed,
            converged,
            failure: value.failure,
            generation: value.generation,
            created_at: value.created_at,
            ready_at: value.ready_at,
            expires_at: spec.expires_at,
            ended_at: value.ended_at,
        })
    }

    /// The environment, if it has a computer and `operator` owns it.
    pub(crate) async fn owned_environment(
        &self,
        environment: &str,
        operator: &str,
    ) -> Result<Stored<EnvironmentRecord>, EnvironmentError> {
        self.refresh().await?;
        let record = self
            .inner
            .lock()
            .await
            .desired
            .environment(environment)
            .cloned()
            .ok_or_else(|| EnvironmentError::NotFound(format!("environment {environment}")))?;
        if record.value.computer.is_none() {
            return Err(EnvironmentError::Invalid(format!(
                "environment {environment} has no computer"
            )));
        }
        if record.value.owner.as_deref() != Some(operator) {
            return Err(EnvironmentError::Forbidden(format!(
                "environment {environment} belongs to another principal"
            )));
        }
        Ok(record)
    }

    /// Whether `operator` may change an environment's lifecycle: anyone with
    /// the scope for an environment without a computer; only its owner for
    /// one with a computer.
    pub async fn authorize_environment(
        &self,
        environment: &str,
        operator: &str,
    ) -> Result<(), EnvironmentError> {
        self.refresh_for_read().await?;
        let owner = {
            let inner = self.inner.lock().await;
            let Some(record) = inner.desired.environment(environment) else {
                return Ok(());
            };
            if record.value.computer.is_none() {
                return Ok(());
            }
            record.value.owner.clone()
        };
        if owner.as_deref() == Some(operator) {
            Ok(())
        } else {
            Err(EnvironmentError::Forbidden(format!(
                "environment {environment} belongs to another principal"
            )))
        }
    }

    /// Whether the environment's computer has ended, so it can no longer be
    /// changed.
    pub(crate) async fn require_live(
        &self,
        record: &Stored<EnvironmentRecord>,
    ) -> Result<(), EnvironmentError> {
        let spec = record
            .value
            .computer
            .as_ref()
            .expect("owned environments have a computer");
        let status = self
            .inner
            .lock()
            .await
            .desired
            .computers
            .get(&record.id)
            .map(|computer| computer.value.status);
        if spec.destroy_requested_at.is_some() || status.is_some_and(ComputerStatus::is_terminal) {
            return Err(EnvironmentError::Conflict(format!(
                "environment {}'s computer has ended ({}); it cannot be changed",
                record.value.name,
                status.map_or("destroying", ComputerStatus::as_str)
            )));
        }
        Ok(())
    }

    /// Change an environment's desired contents. The change and the event
    /// that records it commit together; the computer changes in place.
    pub async fn change_contents(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        description: String,
        change: impl Fn(&mut EnvironmentContents) -> Result<(), EnvironmentError>,
    ) -> Result<ComputerView, EnvironmentError> {
        self.change_environment(environment, operator, description, None, |value| {
            let mut contents = value.contents.clone().unwrap_or_default();
            change(&mut contents)?;
            value.contents = Some(contents);
            Ok(())
        })
        .await
    }

    /// Change what an environment asks of its computer: its contents, its
    /// configuration, or how long it lives. One fenced write, with the
    /// event that records it; every change is a new contents generation,
    /// so a change made from a stale view is refused. Never a replacement.
    pub(crate) async fn change_environment(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        description: String,
        event: Option<(&'static str, serde_json::Value)>,
        change: impl Fn(&mut EnvironmentRecord) -> Result<(), EnvironmentError>,
    ) -> Result<ComputerView, EnvironmentError> {
        self.change_environment_with(
            environment,
            operator,
            description,
            event,
            change,
            |batch, _, _| Ok(batch),
        )
        .await
    }

    /// [`Self::change_environment`], with more writes (a rollout's record)
    /// committed in the same transaction, given the environment and the
    /// contents generation the change produces.
    pub(crate) async fn change_environment_with(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        description: String,
        event: Option<(&'static str, serde_json::Value)>,
        change: impl Fn(&mut EnvironmentRecord) -> Result<(), EnvironmentError>,
        extra: impl Fn(Change, &Stored<EnvironmentRecord>, u64) -> Result<Change, EnvironmentError>,
    ) -> Result<ComputerView, EnvironmentError> {
        // A computer's controller writes its own record, never this one;
        // a lost race with another operator is retried on fresh state.
        for attempt in 0.. {
            let record = self.owned_environment(environment, operator).await?;
            self.require_live(&record).await?;
            let before = record.value.contents.clone().unwrap_or_default();
            let mut value = record.value.clone();
            change(&mut value)?;
            let mut contents = value.contents.clone().unwrap_or_default();
            contents.generation = before.generation + 1;
            contents.validate()?;
            validate_env("environment", &value.config)?;
            value.contents = Some(contents.clone());
            let lifecycle = value
                .computer
                .as_ref()
                .map(|spec| json!({ "lifecycle": spec.lifecycle, "expires_at": spec.expires_at }));
            let batch_change = Change::new().with(|batch| batch.replace(&record, &value));
            let batch_change = self.event(
                batch_change,
                events::CONTENTS_CHANGED,
                Scope::environment(&record.value.name),
                format!("{}: {description}", record.value.name),
                json!({
                    "environment_id": record.id,
                    "operator": operator,
                    "generation": contents.generation,
                    "description": description,
                    "contents": contents,
                    // Configuration values stay out of events.
                    "config_keys": value.config.keys().collect::<Vec<_>>(),
                    "lifecycle": lifecycle,
                }),
            );
            let batch_change = match &event {
                Some((kind, data)) => self.event(
                    batch_change,
                    kind,
                    Scope::environment(&record.value.name),
                    format!("{}: {description}", record.value.name),
                    data.clone(),
                ),
                None => batch_change,
            };
            let batch_change = extra(batch_change, &record, contents.generation)?;
            match self.apply(batch_change).await {
                Ok(()) => break,
                Err(EnvironmentError::Conflict(_)) if attempt < 4 => continue,
                Err(error) => return Err(error),
            }
        }
        self.computer_wake.notify_waiters();
        self.changed().await;
        self.computer(environment).await
    }

    /// Replace the desired contents at once (what GO sends), optionally
    /// only if they are still at the generation the caller saw, with the
    /// configuration and lifetime in the same change.
    pub async fn set_contents(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        update: ContentsUpdate,
    ) -> Result<ComputerView, EnvironmentError> {
        update.contents.validate()?;
        if let Some(config) = &update.config {
            validate_env("environment", config)?;
        }
        if let Some(lifecycle) = &update.lifecycle {
            self.check_lifecycle(environment, operator, lifecycle)
                .await?;
        }
        let expected = update.expected_generation;
        let mut changed = vec!["contents".to_owned()];
        if update.config.is_some() {
            changed.push("configuration".into());
        }
        if let Some(lifecycle) = &update.lifecycle {
            changed.push(format!("lifetime {}", lifecycle_label(lifecycle)));
        }
        self.change_environment(
            environment,
            operator,
            format!("{} replaced", changed.join(", ")),
            None,
            move |value| {
                let current = value.contents.clone().unwrap_or_default();
                if let Some(expected) = expected
                    && expected != current.generation
                {
                    return Err(EnvironmentError::Conflict(format!(
                        "the environment changed since you loaded it (generation {expected}, now {})",
                        current.generation
                    )));
                }
                value.contents = Some(update.contents.clone());
                if let Some(config) = &update.config {
                    value.config = config.clone();
                }
                if let Some(lifecycle) = &update.lifecycle {
                    let spec = value
                        .computer
                        .as_mut()
                        .expect("owned environments have a computer");
                    apply_lifecycle(spec, lifecycle);
                }
                Ok(())
            },
        )
        .await
    }

    /// Change how long an environment lives, in place.
    pub async fn set_lifecycle(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        lifecycle: LifecycleChange,
    ) -> Result<ComputerView, EnvironmentError> {
        self.check_lifecycle(environment, operator, &lifecycle)
            .await?;
        self.change_environment(
            environment,
            operator,
            format!("lifetime {}", lifecycle_label(&lifecycle)),
            None,
            move |value| {
                apply_lifecycle(
                    value
                        .computer
                        .as_mut()
                        .expect("owned environments have a computer"),
                    &lifecycle,
                );
                Ok(())
            },
        )
        .await
    }

    /// A lifetime the environment's machine can have: keeping or extending
    /// one past its target's own expiry needs a target that can claim it.
    async fn check_lifecycle(
        &self,
        environment: &str,
        operator: &str,
        lifecycle: &LifecycleChange,
    ) -> Result<(), EnvironmentError> {
        if lifecycle.ttl_seconds == Some(0) {
            return Err(EnvironmentError::Invalid(
                "a temporary environment's TTL must be greater than zero".into(),
            ));
        }
        if lifecycle.lifecycle == ComputerLifecycle::Persistent && lifecycle.ttl_seconds.is_some() {
            return Err(EnvironmentError::Invalid(
                "an environment that is kept has no TTL".into(),
            ));
        }
        let record = self.owned_environment(environment, operator).await?;
        let spec = record
            .value
            .computer
            .as_ref()
            .expect("owned environments have a computer");
        let unchanged = spec.lifecycle == lifecycle.lifecycle
            && lifecycle.lifecycle == ComputerLifecycle::Persistent;
        if unchanged {
            return Ok(());
        }
        let claim = self
            .stored_computer(&record.id)
            .await
            .and_then(|computer| computer.value.capabilities)
            .is_none_or(|capabilities| capabilities.claim);
        if !claim {
            return Err(EnvironmentError::Invalid(format!(
                "environment {environment}'s target cannot claim its machine, so Compute cannot \
                 change how long it lives; replace it on a target that can"
            )));
        }
        Ok(())
    }

    pub async fn upsert_repository(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        repository: RepositorySpec,
    ) -> Result<ComputerView, EnvironmentError> {
        let description = format!("repository {} at {}", repository.name, repository.revision);
        self.change_contents(environment, operator, description, move |contents| {
            upsert(&mut contents.repositories, repository.clone(), |spec| {
                &spec.name
            });
            Ok(())
        })
        .await
    }

    pub async fn upsert_package(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        package: PackageSpec,
    ) -> Result<ComputerView, EnvironmentError> {
        let description = format!("package {}", package.name);
        self.change_contents(environment, operator, description, move |contents| {
            upsert(&mut contents.packages, package.clone(), |spec| &spec.name);
            Ok(())
        })
        .await
    }

    pub async fn upsert_process(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        process: ProcessSpec,
    ) -> Result<ComputerView, EnvironmentError> {
        let description = format!("{} {}", process.kind.as_str(), process.name);
        self.change_contents(environment, operator, description, move |contents| {
            upsert(&mut contents.processes, process.clone(), |spec| &spec.name);
            Ok(())
        })
        .await
    }

    /// Remove a repository, package, or process by kind and name.
    pub async fn remove_content(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        kind: &str,
        name: &str,
    ) -> Result<ComputerView, EnvironmentError> {
        let kind = kind.to_owned();
        let name = name.to_owned();
        let description = format!("{kind} {name} removed");
        self.change_contents(environment, operator, description, move |contents| {
            let removed = match kind.as_str() {
                "repositories" => remove(&mut contents.repositories, &name, |spec| &spec.name),
                "packages" => remove(&mut contents.packages, &name, |spec| &spec.name),
                "processes" => remove(&mut contents.processes, &name, |spec| &spec.name),
                "projects" => remove(&mut contents.projects, &name, |spec| &spec.name),
                _ => return Err(EnvironmentError::NoRoute(kind.clone())),
            };
            if removed {
                Ok(())
            } else {
                Err(EnvironmentError::NotFound(format!("{kind} {name}")))
            }
        })
        .await
    }

    pub async fn set_process(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        name: &str,
        desired: ProcessDesired,
    ) -> Result<ComputerView, EnvironmentError> {
        let name = name.to_owned();
        let description = format!(
            "process {name} {}",
            match desired {
                ProcessDesired::Running => "started",
                ProcessDesired::Stopped => "stopped",
            }
        );
        self.change_contents(environment, operator, description, move |contents| {
            let process = contents
                .processes
                .iter_mut()
                .find(|process| process.name == name)
                .ok_or_else(|| EnvironmentError::NotFound(format!("process {name}")))?;
            process.desired = desired;
            Ok(())
        })
        .await
    }

    /// Reconcile now: check every process, and retry what failed.
    pub async fn reconcile_computer(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
    ) -> Result<ComputerView, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        self.require_live(&record).await?;
        let change = self.event(
            Change::new(),
            events::ENVIRONMENT_RECONCILE_REQUESTED,
            Scope::environment(&record.value.name),
            format!("Reconciliation of {} requested", record.value.name),
            json!({ "environment_id": record.id }),
        );
        self.apply(change).await?;
        // A failed item is retried on request: forget its failed attempt.
        if let Some(computer) = self.stored_computer(&record.id).await {
            let mut value = computer.value.clone();
            value
                .observed
                .repositories
                .retain(|_, seen| seen.evidence.outcome == "succeeded");
            value
                .observed
                .packages
                .retain(|_, seen| seen.evidence.outcome == "succeeded");
            value
                .observed
                .builds
                .retain(|_, seen| seen.evidence.outcome == "succeeded");
            value
                .observed
                .processes
                .retain(|_, seen| seen.state != ProcessState::Failed);
            value.observed.observed_at = None;
            if value != computer.value {
                value.generation += 1;
                value.updated_at = Utc::now();
                self.apply(Change::new().with(|batch| batch.replace(&computer, &value)))
                    .await?;
            }
        }
        self.computer_wake.notify_waiters();
        self.changed().await;
        self.computer(environment).await
    }

    /// Replace the computer with one that meets new requirements. This is
    /// the one change that provisions a new machine, so it is explicit.
    pub async fn replace_computer(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        requirements: ComputerRequirements,
    ) -> Result<ComputerView, EnvironmentError> {
        requirements.validate()?;
        let record = self.owned_environment(environment, operator).await?;
        self.require_live(&record).await?;
        let mut value = record.value.clone();
        let spec = value
            .computer
            .as_mut()
            .expect("owned environments have a computer");
        spec.requirements = requirements.clone();
        spec.generation += 1;
        let spec = spec.clone();
        self.place_computer(&value, &spec).await?;
        let change = Change::new().with(|batch| batch.replace(&record, &value));
        let change = self.event(
            change,
            events::COMPUTER_REPLACING,
            Scope::environment(&record.value.name),
            format!(
                "{}'s computer will be replaced to meet new requirements",
                record.value.name
            ),
            json!({
                "environment_id": record.id,
                "generation": spec.generation,
                "requirements": requirements,
            }),
        );
        self.apply(change).await?;
        self.computer_wake.notify_waiters();
        self.changed().await;
        self.computer(environment).await
    }

    /// Destroy the computer. The environment and the computer's record stay
    /// as evidence.
    pub async fn destroy_computer(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
    ) -> Result<ComputerView, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        let spec = record
            .value
            .computer
            .as_ref()
            .expect("owned environments have a computer");
        if spec.destroy_requested_at.is_none() {
            let mut value = record.value.clone();
            value
                .computer
                .as_mut()
                .expect("owned environments have a computer")
                .destroy_requested_at = Some(Utc::now());
            let change = Change::new().with(|batch| batch.replace(&record, &value));
            let change = self.event(
                change,
                events::COMPUTER_DESTROYING,
                Scope::environment(&record.value.name),
                format!("{}'s computer will be destroyed", record.value.name),
                json!({ "environment_id": record.id }),
            );
            self.apply(change).await?;
        }
        self.computer_wake.notify_waiters();
        self.changed().await;
        self.computer(environment).await
    }

    /// The target client and session of a running computer.
    pub(crate) async fn running(
        &self,
        record: &Stored<EnvironmentRecord>,
    ) -> Result<(ComputerRecord, Arc<RemoteProvider>, String), EnvironmentError> {
        let computer = self
            .stored_computer(&record.id)
            .await
            .ok_or_else(|| {
                EnvironmentError::Conflict("the computer is not provisioned yet".into())
            })?
            .value;
        if computer.status != ComputerStatus::Running {
            return Err(EnvironmentError::Conflict(format!(
                "environment {}'s computer is {}",
                record.value.name, computer.status
            )));
        }
        let target = computer
            .target
            .clone()
            .expect("a running computer has a target");
        let session_id = computer
            .session_id
            .clone()
            .expect("a running computer has a session");
        let client = self.target_client(&target)?;
        Ok((computer, client, session_id))
    }

    /// Run a command in the computer as a durable job.
    pub async fn computer_exec(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        command: SessionCommand,
    ) -> Result<ComputerExec, EnvironmentError> {
        command.validate()?;
        let record = self.owned_environment(environment, operator).await?;
        let summary = format!(
            "{operator} ran {} in {}",
            command.command.first().cloned().unwrap_or_default(),
            record.value.name
        );
        self.exec_in(
            &record,
            command,
            events::ENVIRONMENT_EXEC,
            summary,
            json!({}),
        )
        .await
    }

    /// Submit a command to the environment's running computer as a durable
    /// job, with the environment's configuration, and record who ran what.
    /// Every piece of environment work goes through here: never the
    /// daemon's own node.
    pub(crate) async fn exec_in(
        self: &Arc<Self>,
        record: &Stored<EnvironmentRecord>,
        mut command: SessionCommand,
        kind: &str,
        summary: String,
        detail: serde_json::Value,
    ) -> Result<ComputerExec, EnvironmentError> {
        let (computer, client, session_id) = self.running(record).await?;
        for (key, value) in &record.value.config {
            command
                .env
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
        let submission = client
            .session_exec(&session_id, &command)
            .await
            .map_err(target_error)?;
        let mut data = json!({
            "environment_id": record.id,
            "target": computer.target,
            "session_id": session_id,
            "machine": computer.provider_resource,
            "job_id": submission.job_id,
            "execution_id": submission.execution_id,
        });
        if let (Some(data), serde_json::Value::Object(detail)) = (data.as_object_mut(), detail) {
            data.extend(detail);
        }
        let change = self.event(
            Change::new(),
            kind,
            Scope::environment(&record.value.name).execution(&submission.execution_id),
            summary,
            data,
        );
        self.apply(change).await?;
        Ok(ComputerExec {
            environment: record.value.name.clone(),
            target: computer.target.unwrap_or_default(),
            session_id,
            job_id: submission.job_id.0,
            execution_id: submission.execution_id,
            status: submission.status,
        })
    }

    /// A job run in the computer, current or earlier: only jobs of this
    /// environment's sessions.
    pub async fn computer_job(
        &self,
        environment: &str,
        operator: &str,
        job_id: &str,
    ) -> Result<ComputerJob, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        let computer = self
            .stored_computer(&record.id)
            .await
            .ok_or_else(|| EnvironmentError::NotFound(format!("job {job_id}")))?
            .value;
        let target = computer
            .target
            .clone()
            .ok_or_else(|| EnvironmentError::NotFound(format!("job {job_id}")))?;
        let client = self.target_client(&target)?;
        let job = client.job_status(job_id).await.map_err(target_error)?;
        let sessions = computer
            .session_id
            .iter()
            .chain(computer.retired.iter().map(|retired| &retired.session_id))
            .collect::<Vec<_>>();
        if !job
            .session_id
            .as_ref()
            .is_some_and(|session| sessions.contains(&&session.0))
        {
            return Err(EnvironmentError::NotFound(format!(
                "job {job_id} in environment {environment}"
            )));
        }
        let result = if job.status.is_terminal() {
            client.job_result(job_id).await.ok()
        } else {
            None
        };
        Ok(ComputerJob { job, result })
    }

    /// The computer's output: every job it ran, or one process's log.
    pub async fn computer_logs(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        process: Option<&str>,
        lines: usize,
    ) -> Result<serde_json::Value, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        let (_, client, session_id) = self.running(&record).await?;
        match process {
            None => Ok(serde_json::to_value(
                client
                    .session_logs(&session_id)
                    .await
                    .map_err(target_error)?,
            )?),
            Some(name) => {
                // A process's log is read by a job too: evidence of who
                // read what.
                let (evidence, output) = self
                    .run_in_computer(
                        &client,
                        &session_id,
                        script(PROCESS_LOG, [name.to_owned(), lines.max(1).to_string()]),
                        Duration::from_secs(30),
                    )
                    .await;
                Ok(json!({ "process": name, "log": output, "evidence": evidence }))
            }
        }
    }

    pub async fn computer_connect(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
    ) -> Result<compute_core::SessionConnectionGrant, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        let (computer, client, session_id) = self.running(&record).await?;
        let grant = client
            .connect_session(&session_id)
            .await
            .map_err(target_error)?;
        let change = self.event(
            Change::new(),
            events::ENVIRONMENT_CONNECTED,
            Scope::environment(&record.value.name),
            format!("{operator} connected to {}", record.value.name),
            json!({
                "environment_id": record.id,
                "target": computer.target,
                "mode": grant.connection.mode.as_str(),
            }),
        );
        self.apply(change).await?;
        Ok(grant)
    }

    // ---- Placement -------------------------------------------------------

    /// Place a computer on a target of the daemon's pool. It fails with
    /// every target's reasons when none can host it.
    async fn place_computer(
        &self,
        environment: &EnvironmentRecord,
        spec: &ComputerSpec,
    ) -> Result<PlacementReport, EnvironmentError> {
        self.place_on(environment, spec, spec.target.as_deref())
            .await
            .map(|(report, _)| report)
    }

    async fn place_on(
        &self,
        environment: &EnvironmentRecord,
        spec: &ComputerSpec,
        target: Option<&str>,
    ) -> Result<(PlacementReport, SessionCreateRequest), EnvironmentError> {
        let (requirements, create) =
            PlacementRequirements::for_computer(&spec.requirements, spec.lifecycle)
                .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
        let bundle = create.environment().map_err(target_error)?;
        let contract = ExecutionContract::from_bundle(&bundle, Some(requirements.isolation))
            .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
        let context = AdmissionContext::new(&self.policy_sources(environment)?, contract);
        let records = {
            let mut cache = self.cache.lock().await;
            self.pool
                .capabilities(&mut cache, DiscoveryMode::Refresh, target, Utc::now())
                .await
        };
        let policy = target
            .map(|id| PlacementPolicy::Provider(id.to_owned()))
            .unwrap_or_default();
        let report = place_with_policy(
            &self.pool.configs(),
            self.pool.policy(),
            &records,
            &requirements,
            &context,
            policy,
        );
        if report.outcome != PlacementOutcome::Placed {
            let reasons = report
                .providers
                .iter()
                .map(|provider| {
                    format!(
                        "{}: {}",
                        provider.provider_id,
                        provider
                            .reasons
                            .iter()
                            .map(|reason| reason.code.as_str())
                            .chain(provider.error.iter().map(|error| error.message.clone()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(EnvironmentError::Invalid(format!(
                "no target can host this computer ({}){}",
                report
                    .failure
                    .as_ref()
                    .map(|failure| failure.code.as_str())
                    .unwrap_or("placement_failed"),
                if reasons.is_empty() {
                    String::new()
                } else {
                    format!(": {reasons}")
                }
            )));
        }
        Ok((report, create))
    }

    /// The host a target's endpoint names: where its computers' endpoints
    /// listen.
    pub(crate) fn target_host(&self, target: &str) -> Option<String> {
        let endpoint = self.pool.configs().get(target)?.endpoint.clone()?;
        let rest = endpoint
            .split_once("://")
            .map_or(endpoint.as_str(), |(_, rest)| rest);
        let authority = rest.split('/').next()?;
        let host = match authority.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => host,
            _ => authority,
        };
        (!host.is_empty()).then(|| host.to_owned())
    }

    pub(crate) fn target_client(
        &self,
        target: &str,
    ) -> Result<Arc<RemoteProvider>, EnvironmentError> {
        self.pool
            .member(target)
            .and_then(|member| member.jobs.clone())
            .ok_or_else(|| {
                EnvironmentError::RuntimeUnavailable(format!(
                    "target {target} is not a remote member of the daemon's pool"
                ))
            })
    }

    // ---- The controller ----------------------------------------------------

    /// The computer record after reading back this controller's writes.
    async fn fresh_computer(&self, environment_id: &str) -> Option<Stored<ComputerRecord>> {
        let _ = self.refresh_targeted().await;
        self.stored_computer(environment_id).await
    }

    pub(crate) async fn stored_computer(
        &self,
        environment_id: &str,
    ) -> Option<Stored<ComputerRecord>> {
        self.inner
            .lock()
            .await
            .desired
            .computers
            .get(environment_id)
            .cloned()
    }

    /// Start a driver for every computer that is not terminal, and sweep
    /// targets for orphans now and then. Called by every reconcile cycle,
    /// so drivers are rebuilt from durable state after a restart.
    pub(crate) async fn drive_computers(self: &Arc<Self>) {
        let live = {
            let inner = self.inner.lock().await;
            inner
                .desired
                .environments
                .values()
                .filter(|record| record.value.computer.is_some())
                .filter(|record| {
                    inner
                        .desired
                        .computers
                        .get(&record.id)
                        .is_none_or(|computer| {
                            !computer.value.status.is_terminal()
                                || !computer.value.retired.is_empty()
                        })
                })
                .map(|record| record.id.clone())
                .collect::<Vec<_>>()
        };
        for environment_id in live {
            if !self
                .computer_drivers
                .lock()
                .expect("drivers")
                .insert(environment_id.clone())
            {
                continue;
            }
            let daemon = self.clone();
            tokio::spawn(async move {
                daemon.drive_computer(&environment_id).await;
                daemon
                    .computer_drivers
                    .lock()
                    .expect("drivers")
                    .remove(&environment_id);
            });
        }
        let due = {
            let mut sweep = self.orphan_sweep.lock().expect("sweep");
            let due = sweep.is_none_or(|at| at.elapsed() > Duration::from_secs(60));
            if due {
                *sweep = Some(std::time::Instant::now());
            }
            due
        };
        if due {
            let daemon = self.clone();
            tokio::spawn(async move { daemon.sweep_orphans().await });
        }
    }

    async fn drive_computer(self: &Arc<Self>, environment_id: &str) {
        loop {
            if self.is_shutting_down() {
                return;
            }
            let step = match self.computer_step(environment_id).await {
                Ok(step) => step,
                Err(_) => Step::Wait(BACKOFF),
            };
            match step {
                Step::Continue => {}
                Step::Done => return,
                Step::Wait(duration) => {
                    let wake = self.computer_wake.notified();
                    tokio::select! {
                        _ = wake => {}
                        _ = tokio::time::sleep(duration) => {}
                    }
                }
            }
        }
    }

    /// Write the computer's next state, fenced on the version the step
    /// read: a step whose premise changed writes nothing.
    async fn advance(
        &self,
        stored: &Stored<ComputerRecord>,
        environment: &str,
        value: ComputerRecord,
        event: Option<(&str, String, serde_json::Value)>,
    ) -> Result<(), EnvironmentError> {
        if self.is_shutting_down() {
            return Err(EnvironmentError::ControllerUnavailable(
                "the controller is stopping".into(),
            ));
        }
        let mut value = value;
        value.generation = stored.value.generation + 1;
        value.updated_at = Utc::now();
        let mut change = Change::new().with(|batch| batch.replace(stored, &value));
        if let Some((kind, message, data)) = event {
            change = self.event(change, kind, Scope::environment(environment), message, data);
        }
        self.apply(change).await
    }

    async fn computer_step(
        self: &Arc<Self>,
        environment_id: &str,
    ) -> Result<Step, EnvironmentError> {
        self.refresh_targeted().await?;
        let (record, stored) = {
            let inner = self.inner.lock().await;
            let Some(record) = inner
                .desired
                .environments
                .values()
                .find(|record| record.id == environment_id)
                .cloned()
            else {
                return Ok(Step::Done);
            };
            (record, inner.desired.computers.get(environment_id).cloned())
        };
        let Some(spec) = record.value.computer.clone() else {
            return Ok(Step::Done);
        };
        let name = record.value.name.clone();
        let Some(stored) = stored else {
            return Ok(Step::Wait(BACKOFF));
        };
        let computer = stored.value.clone();
        if computer.status.is_terminal() {
            // Work sessions in it end with it.
            self.end_sessions_of(&record, computer.status).await?;
            // Earlier sessions of a failed or ended computer are still torn
            // down.
            return if self.retire(&stored, &name).await? {
                Ok(Step::Done)
            } else {
                Ok(Step::Wait(BACKOFF))
            };
        }
        let now = Utc::now();
        let expired = spec.lifecycle == ComputerLifecycle::Ephemeral
            && spec.expires_at.is_some_and(|at| at <= now);
        if spec.destroy_requested_at.is_some() || expired {
            return self.teardown(&stored, &name, expired).await;
        }
        match computer.status {
            ComputerStatus::Pending => self.place_step(&record, &spec, &stored).await,
            ComputerStatus::Provisioning => self.provision_step(&record, &spec, &stored).await,
            ComputerStatus::Running => self.running_step(&record, &spec, &stored).await,
            ComputerStatus::Stopping => self.stop_step(&stored, &name).await,
            ComputerStatus::Stopped => {
                // A target that cannot resume this computer is not asked
                // again: it stays stopped until it is replaced or destroyed.
                let unresumable = computer
                    .failure
                    .as_ref()
                    .is_some_and(|failure| failure.code == "resume_unsupported");
                if record.value.desired_state == DesiredState::Running && !unresumable {
                    let mut value = computer.clone();
                    value.status = ComputerStatus::Resuming;
                    self.advance(&stored, &name, value, None).await?;
                    Ok(Step::Continue)
                } else {
                    Ok(Step::Wait(Duration::from_secs(10)))
                }
            }
            ComputerStatus::Resuming => self.resume_step(&stored, &name).await,
            _ => Ok(Step::Done),
        }
    }

    async fn place_step(
        &self,
        record: &Stored<EnvironmentRecord>,
        spec: &ComputerSpec,
        stored: &Stored<ComputerRecord>,
    ) -> Result<Step, EnvironmentError> {
        let name = &record.value.name;
        match self
            .place_on(&record.value, spec, spec.target.as_deref())
            .await
        {
            Ok((report, _)) => {
                let selected = report.selected.as_ref().expect("placed");
                let mut value = stored.value.clone();
                value.status = ComputerStatus::Provisioning;
                value.target = Some(selected.provider_id.clone());
                value.placement_id = Some(report.placement_id.clone());
                value.spec_generation = spec.generation;
                value.reference = Some(format!("{}:{}", stored.id, spec.generation));
                value.failure = None;
                self.advance(
                    stored,
                    name,
                    value,
                    Some((
                        events::COMPUTER_PLACED,
                        format!("{name}'s computer was placed on {}", selected.provider_id),
                        json!({
                            "environment_id": record.id,
                            "target": selected.provider_id,
                            "placement_id": report.placement_id,
                        }),
                    )),
                )
                .await?;
                Ok(Step::Continue)
            }
            Err(error) => {
                // Targets come and go: placement is retried, and the reason
                // is kept on the record.
                self.record_failure(
                    stored,
                    name,
                    "placement",
                    "placement_failed",
                    &error.to_string(),
                    true,
                    None,
                )
                .await?;
                Ok(Step::Wait(Duration::from_secs(5)))
            }
        }
    }

    async fn provision_step(
        &self,
        record: &Stored<EnvironmentRecord>,
        spec: &ComputerSpec,
        stored: &Stored<ComputerRecord>,
    ) -> Result<Step, EnvironmentError> {
        let name = &record.value.name;
        let computer = &stored.value;
        let target = computer
            .target
            .clone()
            .expect("provisioning computers are placed");
        let client = self.target_client(&target)?;
        let Some(session_id) = computer.session_id.clone() else {
            // Keyed by the computer's reference, so repeating this after a
            // restart finds the same session instead of making another.
            let (report, mut create) = match self.place_on(&record.value, spec, Some(&target)).await
            {
                Ok(placed) => placed,
                Err(error) => {
                    self.record_failure(
                        stored,
                        name,
                        "placement",
                        "target_incompatible",
                        &error.to_string(),
                        true,
                        Some(&target),
                    )
                    .await?;
                    return Ok(Step::Wait(Duration::from_secs(5)));
                }
            };
            create.spec.reference = computer.reference.clone();
            if spec.lifecycle == ComputerLifecycle::Ephemeral {
                let remaining = spec
                    .expires_at
                    .map(|at| (at - Utc::now()).num_seconds().max(1) as u64)
                    .unwrap_or(DEFAULT_TTL);
                create.spec.ttl_seconds = Some(remaining + TARGET_GRACE);
            }
            match dispatch::create_session(&self.pool, &report, create).await {
                Ok(placed) => {
                    let mut value = computer.clone();
                    value.session_id = Some(placed.session.session_id.0.clone());
                    value.provider_kind = Some(placed.session.provider_kind.clone());
                    value.provider_resource = placed.session.provider_session_id.clone();
                    value.failure = None;
                    self.advance(
                        stored,
                        name,
                        value,
                        Some((
                            events::COMPUTER_PROVISIONED,
                            format!("{name}'s computer is being provisioned on {target}"),
                            json!({
                                "environment_id": record.id,
                                "target": target,
                                "session_id": placed.session.session_id,
                                "job_id": placed.session.job_id,
                                "execution_id": placed.session.execution_id,
                            }),
                        )),
                    )
                    .await?;
                    return Ok(Step::Continue);
                }
                Err(error) => {
                    let retryable = matches!(
                        error.code,
                        compute_placement::DispatchErrorCode::ProviderUnavailable
                    );
                    self.record_failure(
                        stored,
                        name,
                        "provisioning",
                        "target_rejected",
                        &error.to_string(),
                        retryable,
                        Some(&target),
                    )
                    .await?;
                    if retryable {
                        return Ok(Step::Wait(BACKOFF));
                    }
                    return self.fail_computer(stored, name).await;
                }
            }
        };
        match client.session(&session_id).await {
            Ok(session) => match session.status {
                SessionStatus::Ready | SessionStatus::Running => {
                    let mut value = computer.clone();
                    value.status = ComputerStatus::Running;
                    value.capabilities = Some(session.capabilities);
                    value.connection = session.connection.clone();
                    value.provider_kind = Some(session.provider_kind.clone());
                    value.provider_resource = session.provider_session_id.clone();
                    value.ready_at = Some(Utc::now());
                    value.failure = None;
                    self.advance(
                        stored,
                        name,
                        value,
                        Some((
                            events::COMPUTER_RUNNING,
                            format!("{name}'s computer is running on {target}"),
                            json!({
                                "environment_id": record.id,
                                "target": target,
                                "session_id": session_id,
                                "provider_kind": session.provider_kind,
                                "capabilities": session.capabilities,
                            }),
                        )),
                    )
                    .await?;
                    Ok(Step::Continue)
                }
                status if status.is_terminal() => {
                    let (code, message) = session
                        .failure
                        .map(|failure| (failure.code, failure.message))
                        .unwrap_or_else(|| {
                            ("session_ended".into(), format!("the session is {status}"))
                        });
                    self.record_failure(
                        stored,
                        name,
                        "provisioning",
                        &code,
                        &message,
                        false,
                        Some(&target),
                    )
                    .await?;
                    let refreshed = self.fresh_computer(&record.id).await;
                    self.fail_computer(refreshed.as_ref().unwrap_or(stored), name)
                        .await
                }
                _ => Ok(Step::Wait(POLL)),
            },
            Err(error) => self.target_unreachable(stored, name, &target, error).await,
        }
    }

    async fn running_step(
        self: &Arc<Self>,
        record: &Stored<EnvironmentRecord>,
        spec: &ComputerSpec,
        stored: &Stored<ComputerRecord>,
    ) -> Result<Step, EnvironmentError> {
        let name = &record.value.name;
        let computer = &stored.value;
        // Sessions of an earlier generation go once this one runs.
        if !computer.retired.is_empty() {
            self.retire(stored, name).await?;
            return Ok(Step::Continue);
        }
        // New requirements: a replacement, provisioned alongside, never an
        // ordinary change.
        if spec.generation > computer.spec_generation {
            let mut value = computer.clone();
            if let (Some(target), Some(session_id)) = (&computer.target, &computer.session_id) {
                value.retired.push(RetiredSession {
                    target: target.clone(),
                    session_id: session_id.clone(),
                    spec_generation: computer.spec_generation,
                });
            }
            value.status = ComputerStatus::Pending;
            value.session_id = None;
            value.target = None;
            value.reference = None;
            value.capabilities = None;
            value.connection = None;
            value.observed = ObservedContents::default();
            value.ready_at = None;
            self.advance(
                stored,
                name,
                value,
                Some((
                    events::COMPUTER_REPLACING,
                    format!("{name}'s computer is being replaced"),
                    json!({
                        "environment_id": record.id,
                        "from_generation": computer.spec_generation,
                        "to_generation": spec.generation,
                        "retired_session": computer.session_id,
                    }),
                )),
            )
            .await?;
            return Ok(Step::Continue);
        }
        if record.value.desired_state == DesiredState::Stopped {
            let mut value = computer.clone();
            value.status = ComputerStatus::Stopping;
            self.advance(
                stored,
                name,
                value,
                Some((
                    events::COMPUTER_STOPPING,
                    format!("{name}'s computer is stopping"),
                    json!({ "environment_id": record.id }),
                )),
            )
            .await?;
            return Ok(Step::Continue);
        }
        let target = computer
            .target
            .clone()
            .expect("a running computer has a target");
        let session_id = computer
            .session_id
            .clone()
            .expect("a running computer has a session");
        let client = self.target_client(&target)?;
        // A new lifetime is applied in place: Compute takes over the
        // machine's expiry from the target (a claim), then keeps or ends it
        // itself.
        // Compared to the second: a datetime field and the spec's JSON may
        // keep different precision.
        let seconds = |at: Option<chrono::DateTime<Utc>>| at.map(|at| at.timestamp());
        if computer.lifecycle != spec.lifecycle
            || seconds(computer.expires_at) != seconds(spec.expires_at)
        {
            let claimed = match client.session(&session_id).await {
                Ok(session) => session.ownership == compute_core::SessionOwnership::Claimed,
                Err(error) => return self.target_unreachable(stored, name, &target, error).await,
            };
            if !claimed && let Err(error) = client.claim_session(&session_id).await {
                self.record_failure(
                    stored,
                    name,
                    "lifecycle",
                    "claim_failed",
                    &error.message,
                    true,
                    Some(&target),
                )
                .await?;
                return Ok(Step::Wait(BACKOFF));
            }
            let mut value = computer.clone();
            value.lifecycle = spec.lifecycle;
            value.expires_at = spec.expires_at;
            self.advance(
                stored,
                name,
                value,
                Some((
                    events::COMPUTER_LIFECYCLE_CHANGED,
                    format!(
                        "{name}'s machine is {}, in place",
                        match spec.lifecycle {
                            ComputerLifecycle::Persistent => "kept until destroyed".to_owned(),
                            ComputerLifecycle::Ephemeral => format!(
                                "temporary until {}",
                                spec.expires_at
                                    .map(|at| at.to_rfc3339())
                                    .unwrap_or_default()
                            ),
                        }
                    ),
                    json!({
                        "environment_id": record.id,
                        "lifecycle": spec.lifecycle,
                        "expires_at": spec.expires_at,
                        "session_id": session_id,
                    }),
                )),
            )
            .await?;
            return Ok(Step::Continue);
        }
        let contents = record.value.contents.clone().unwrap_or_default();
        let config = &record.value.config;
        if let Some(action) = plan(&contents, &computer.observed, config) {
            self.apply_action(record, stored, &client, &session_id, action)
                .await?;
            return Ok(Step::Continue);
        }
        let mut value = computer.clone();
        let mut event = None;
        if value.observed.converged_generation != contents.generation {
            value.observed.converged_generation = contents.generation;
            event = Some((
                events::CONTENTS_CONVERGED,
                format!(
                    "{name}'s computer holds contents generation {}, changed in place",
                    contents.generation
                ),
                json!({
                    "environment_id": record.id,
                    "generation": contents.generation,
                    "target": target,
                    "session_id": session_id,
                }),
            ));
        }
        // Processes are checked for drift now and then.
        let probe_due = !value.observed.processes.is_empty()
            && value.observed.observed_at.is_none_or(|at| {
                (Utc::now() - at).to_std().unwrap_or_default() >= self.config.computer_probe
            });
        if probe_due {
            let (evidence, output) = self
                .run_in_computer(
                    &client,
                    &session_id,
                    script(PROBE_PROCESSES, []),
                    Duration::from_secs(30),
                )
                .await;
            if evidence.outcome == "succeeded" {
                let seen = output
                    .lines()
                    .filter_map(|line| {
                        let mut parts = line.split_whitespace();
                        Some((parts.next()?.to_owned(), parts.next()? == "running"))
                    })
                    .collect::<BTreeMap<_, _>>();
                for (process, observed) in value.observed.processes.iter_mut() {
                    if observed.state == ProcessState::Running
                        && !seen.get(process).copied().unwrap_or(false)
                    {
                        observed.state = ProcessState::Exited;
                        observed.evidence = evidence.clone();
                    }
                }
            }
            value.observed.observed_at = Some(Utc::now());
        }
        if value != *computer {
            self.advance(stored, name, value, event).await?;
            return Ok(Step::Continue);
        }
        let wait = value
            .observed
            .observed_at
            .map(|at| {
                self.config
                    .computer_probe
                    .saturating_sub((Utc::now() - at).to_std().unwrap_or_default())
            })
            .unwrap_or(self.config.computer_probe)
            .max(POLL);
        Ok(Step::Wait(wait))
    }

    /// Apply one action as a durable job in the running computer and
    /// record what it proved.
    async fn apply_action(
        &self,
        record: &Stored<EnvironmentRecord>,
        stored: &Stored<ComputerRecord>,
        client: &RemoteProvider,
        session_id: &str,
        action: Action,
    ) -> Result<(), EnvironmentError> {
        let name = &record.value.name;
        let mut value = stored.value.clone();
        let (item, kind, evidence) = match action {
            Action::SyncRepository(repository, wanted) => {
                let (evidence, output) = self
                    .run_in_computer(
                        client,
                        session_id,
                        script(
                            SYNC_REPOSITORY,
                            [
                                repository.name.clone(),
                                repository.url.clone(),
                                repository.revision.clone(),
                            ],
                        ),
                        Duration::from_secs(30 * 60),
                    )
                    .await;
                let commit = (evidence.outcome == "succeeded")
                    .then(|| output.lines().last().map(str::trim).map(str::to_owned))
                    .flatten();
                value.observed.repositories.insert(
                    repository.name.clone(),
                    ObservedRepository {
                        revision: repository.revision.clone(),
                        commit: commit.clone(),
                        fingerprint: wanted,
                        evidence: evidence.clone(),
                    },
                );
                (
                    format!(
                        "repository {} at {}{}",
                        repository.name,
                        repository.revision,
                        commit
                            .map(|commit| format!(" ({commit})"))
                            .unwrap_or_default()
                    ),
                    "repository",
                    evidence,
                )
            }
            Action::RemoveRepository(repository) => {
                let (evidence, _) = self
                    .run_in_computer(
                        client,
                        session_id,
                        script(REMOVE_REPOSITORY, [repository.clone()]),
                        Duration::from_secs(300),
                    )
                    .await;
                if evidence.outcome == "succeeded" {
                    value.observed.repositories.remove(&repository);
                }
                (
                    format!("repository {repository} removed"),
                    "repository",
                    evidence,
                )
            }
            Action::InstallPackage(package, wanted) => {
                let mut arguments = vec![package.repository.clone().unwrap_or_default()];
                arguments.extend(package.install.iter().cloned());
                let (evidence, _) = self
                    .run_in_computer(
                        client,
                        session_id,
                        script(INSTALL_PACKAGE, arguments),
                        Duration::from_secs(60 * 60),
                    )
                    .await;
                value.observed.packages.insert(
                    package.name.clone(),
                    ObservedPackage {
                        fingerprint: wanted,
                        evidence: evidence.clone(),
                    },
                );
                (format!("package {}", package.name), "package", evidence)
            }
            Action::ForgetPackage(package) => {
                value.observed.packages.remove(&package);
                self.advance(stored, name, value, None).await?;
                return Ok(());
            }
            Action::Build(project, wanted) => {
                let mut arguments = vec![project.repository.clone()];
                arguments.extend(project.build.iter().cloned());
                let mut command = script(INSTALL_PACKAGE, arguments);
                command.env = record.value.config.clone();
                let (evidence, _) = self
                    .run_in_computer_command(
                        client,
                        session_id,
                        command,
                        Duration::from_secs(60 * 60),
                    )
                    .await;
                let commit = commit_of(&project.repository, &value.observed);
                value.observed.builds.insert(
                    project.name.clone(),
                    ObservedBuild {
                        commit: commit.clone(),
                        fingerprint: wanted,
                        evidence: evidence.clone(),
                    },
                );
                (
                    format!(
                        "project {} built{}",
                        project.name,
                        commit
                            .map(|commit| format!(" at {}", &commit[..commit.len().min(12)]))
                            .unwrap_or_default()
                    ),
                    "build",
                    evidence,
                )
            }
            Action::ForgetBuild(project) => {
                value.observed.builds.remove(&project);
                self.advance(stored, name, value, None).await?;
                return Ok(());
            }
            Action::StopProcess {
                name: process,
                forget,
            } => {
                let (evidence, _) = self
                    .run_in_computer(
                        client,
                        session_id,
                        script(STOP_PROCESS, [process.clone()]),
                        Duration::from_secs(60),
                    )
                    .await;
                if evidence.outcome == "succeeded" {
                    if forget {
                        value.observed.processes.remove(&process);
                    } else if let Some(observed) = value.observed.processes.get_mut(&process) {
                        observed.state = ProcessState::Stopped;
                        observed.pid = None;
                        observed.evidence = evidence.clone();
                    }
                }
                (format!("process {process} stopped"), "process", evidence)
            }
            Action::StartProcess(process, wanted) => {
                let mut arguments = vec![
                    process.name.clone(),
                    process.repository.clone().unwrap_or_default(),
                ];
                arguments.extend(process.command.iter().cloned());
                let mut command = script(START_PROCESS, arguments);
                command.env = process_env(&process, &record.value.config);
                let (evidence, output) = self
                    .run_in_computer_command(client, session_id, command, Duration::from_secs(60))
                    .await;
                let succeeded = evidence.outcome == "succeeded";
                value.observed.processes.insert(
                    process.name.clone(),
                    ObservedProcess {
                        state: if succeeded {
                            ProcessState::Running
                        } else {
                            ProcessState::Failed
                        },
                        fingerprint: wanted,
                        pid: succeeded
                            .then(|| {
                                output
                                    .lines()
                                    .last()
                                    .and_then(|line| line.trim().parse().ok())
                            })
                            .flatten(),
                        evidence: evidence.clone(),
                    },
                );
                (
                    format!("{} {} started", process.kind.as_str(), process.name),
                    "process",
                    evidence,
                )
            }
        };
        let succeeded = evidence.outcome == "succeeded";
        if !succeeded {
            value.failure = Some(ComputerFailure {
                phase: "reconciliation".into(),
                code: "item_failed".into(),
                message: format!(
                    "{item}: {}",
                    evidence
                        .error
                        .clone()
                        .unwrap_or_else(|| evidence.outcome.clone())
                ),
                retryable: true,
                target: value.target.clone(),
                at: Utc::now(),
            });
        } else if value
            .failure
            .as_ref()
            .is_some_and(|failure| failure.phase == "reconciliation")
        {
            value.failure = None;
        }
        self.advance(
            stored,
            name,
            value.clone(),
            Some((
                if succeeded {
                    events::CONTENTS_APPLIED
                } else {
                    events::CONTENTS_FAILED
                },
                format!(
                    "{name}: {item}{} in place",
                    if succeeded { "" } else { " failed" }
                ),
                json!({
                    "environment_id": record.id,
                    "kind": kind,
                    "item": item,
                    "target": value.target,
                    "session_id": session_id,
                    "job_id": evidence.job_id,
                    "execution_id": evidence.execution_id,
                    "outcome": evidence.outcome,
                    "error": evidence.error,
                }),
            )),
        )
        .await
    }

    async fn run_in_computer(
        &self,
        client: &RemoteProvider,
        session_id: &str,
        command: SessionCommand,
        timeout: Duration,
    ) -> (OperationEvidence, String) {
        self.run_in_computer_command(client, session_id, command, timeout)
            .await
    }

    /// Run one controller command in the computer as a durable job and wait
    /// for it. The evidence names the job whatever happened.
    pub(crate) async fn run_in_computer_command(
        &self,
        client: &RemoteProvider,
        session_id: &str,
        mut command: SessionCommand,
        timeout: Duration,
    ) -> (OperationEvidence, String) {
        command.timeout = Some(timeout);
        let failed = |job_id: String, execution_id: String, error: String| {
            (
                OperationEvidence {
                    job_id,
                    execution_id,
                    outcome: "failed".into(),
                    at: Utc::now(),
                    error: Some(error),
                },
                String::new(),
            )
        };
        let submission = match client.session_exec(session_id, &command).await {
            Ok(submission) => submission,
            Err(error) => return failed(String::new(), String::new(), error.to_string()),
        };
        let job_id = submission.job_id.0.clone();
        let mut delay = Duration::from_millis(50);
        let job = loop {
            match client.job_status(&job_id).await {
                Ok(job) if job.status.is_terminal() => break job,
                Ok(_) => {}
                Err(error) if error.kind == ProviderErrorKind::UnknownJob => {
                    return failed(job_id, submission.execution_id, error.to_string());
                }
                Err(_) => {}
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(500));
        };
        let result = client.job_result(&job_id).await.ok();
        let succeeded = job.status == compute_core::JobStatus::Succeeded;
        let stdout = result
            .as_ref()
            .map(|result| result.result.stdout.text.clone())
            .unwrap_or_default();
        let error = (!succeeded).then(|| {
            result
                .as_ref()
                .map(|result| result.result.stderr.text.trim().to_owned())
                .filter(|stderr| !stderr.is_empty())
                .or(job.failure.clone())
                .unwrap_or_else(|| format!("{:?}", job.status))
        });
        (
            OperationEvidence {
                job_id,
                execution_id: job.execution_id.unwrap_or(submission.execution_id),
                outcome: if succeeded { "succeeded" } else { "failed" }.into(),
                at: Utc::now(),
                error,
            },
            stdout,
        )
    }

    async fn stop_step(
        &self,
        stored: &Stored<ComputerRecord>,
        name: &str,
    ) -> Result<Step, EnvironmentError> {
        let computer = &stored.value;
        let (Some(target), Some(session_id)) =
            (computer.target.clone(), computer.session_id.clone())
        else {
            return self.fail_computer(stored, name).await;
        };
        let client = self.target_client(&target)?;
        // Processes are stopped as the durable job they are, then the
        // session: its active jobs are cancelled and its machine suspended.
        let mut value = computer.clone();
        for (process, observed) in value.observed.processes.iter_mut() {
            if observed.state == ProcessState::Running {
                let (evidence, _) = self
                    .run_in_computer(
                        &client,
                        &session_id,
                        script(STOP_PROCESS, [process.clone()]),
                        Duration::from_secs(60),
                    )
                    .await;
                observed.state = ProcessState::Stopped;
                observed.pid = None;
                observed.evidence = evidence;
            }
        }
        match client.stop_session(&session_id).await {
            Ok(_) => {}
            Err(error) if error.kind == ProviderErrorKind::SessionConflict => {}
            Err(error) => return self.target_unreachable(stored, name, &target, error).await,
        }
        value.status = ComputerStatus::Stopped;
        value.failure = None;
        self.advance(
            stored,
            name,
            value,
            Some((
                events::COMPUTER_STOPPED,
                format!("{name}'s computer stopped"),
                json!({ "environment_id": stored.value.environment_id, "target": target, "session_id": session_id }),
            )),
        )
        .await?;
        Ok(Step::Continue)
    }

    async fn resume_step(
        &self,
        stored: &Stored<ComputerRecord>,
        name: &str,
    ) -> Result<Step, EnvironmentError> {
        let computer = &stored.value;
        let (Some(target), Some(session_id)) =
            (computer.target.clone(), computer.session_id.clone())
        else {
            return self.fail_computer(stored, name).await;
        };
        let client = self.target_client(&target)?;
        match client.resume_session(&session_id).await {
            Ok(_) => {}
            Err(error) if error.kind == ProviderErrorKind::UnknownSession => {
                self.record_failure(
                    stored,
                    name,
                    "resuming",
                    "environment_lost",
                    &error.message,
                    false,
                    Some(&target),
                )
                .await?;
                let refreshed = self.fresh_computer(&computer.environment_id).await;
                return self
                    .fail_computer(refreshed.as_ref().unwrap_or(stored), name)
                    .await;
            }
            Err(error) if error.kind == ProviderErrorKind::OperationUnsupported => {
                // The target cannot resume this machine: say so, and keep
                // it stopped. Nothing is recreated in its place.
                self.record_failure(
                    stored,
                    name,
                    "resuming",
                    "resume_unsupported",
                    &error.message,
                    false,
                    Some(&target),
                )
                .await?;
                let mut value = self
                    .fresh_computer(&computer.environment_id)
                    .await
                    .unwrap_or(stored.clone());
                let fenced = value.clone();
                value.value.status = ComputerStatus::Stopped;
                self.advance(&fenced, name, value.value, None).await?;
                return Ok(Step::Wait(Duration::from_secs(60)));
            }
            Err(error) => return self.target_unreachable(stored, name, &target, error).await,
        }
        let mut value = computer.clone();
        value.status = ComputerStatus::Running;
        value.failure = None;
        self.advance(
            stored,
            name,
            value,
            Some((
                events::COMPUTER_RESUMED,
                format!("{name}'s computer resumed"),
                json!({ "environment_id": stored.value.environment_id, "target": target, "session_id": session_id }),
            )),
        )
        .await?;
        Ok(Step::Continue)
    }

    /// Destroy (or expire) the computer: every session it had goes, and the
    /// record stays.
    async fn teardown(
        &self,
        stored: &Stored<ComputerRecord>,
        name: &str,
        expired: bool,
    ) -> Result<Step, EnvironmentError> {
        let computer = &stored.value;
        if computer.status != ComputerStatus::Destroying {
            let mut value = computer.clone();
            value.status = ComputerStatus::Destroying;
            self.advance(
                stored,
                name,
                value,
                Some((
                    events::COMPUTER_DESTROYING,
                    format!(
                        "{name}'s computer is being {}",
                        if expired { "expired" } else { "destroyed" }
                    ),
                    json!({ "environment_id": computer.environment_id, "expired": expired }),
                )),
            )
            .await?;
            return Ok(Step::Continue);
        }
        let mut sessions = computer
            .retired
            .iter()
            .map(|retired| (retired.target.clone(), Some(retired.session_id.clone())))
            .collect::<Vec<_>>();
        if let Some(target) = &computer.target {
            sessions.push((target.clone(), computer.session_id.clone()));
        }
        for (target, session_id) in sessions {
            let client = self.target_client(&target)?;
            let session_id = match session_id {
                Some(session_id) => Some(session_id),
                // Provisioning may have created a session this record never
                // learned of: find it by the computer's reference.
                None => match &computer.reference {
                    Some(reference) => client
                        .sessions()
                        .await
                        .map_err(target_error)?
                        .into_iter()
                        .find(|session| {
                            session.reference.as_ref() == Some(reference)
                                && !session.status.is_terminal()
                        })
                        .map(|session| session.session_id.0),
                    None => None,
                },
            };
            if let Some(session_id) = session_id {
                match client.destroy_session(&session_id).await {
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind,
                            ProviderErrorKind::UnknownSession | ProviderErrorKind::SessionConflict
                        ) => {}
                    Err(error) => {
                        self.record_failure(
                            stored,
                            name,
                            "teardown",
                            "target_unavailable",
                            &error.message,
                            true,
                            Some(&target),
                        )
                        .await?;
                        return Ok(Step::Wait(BACKOFF));
                    }
                }
            }
        }
        let mut value = self
            .stored_computer(&computer.environment_id)
            .await
            .unwrap_or_else(|| stored.clone());
        let fenced = value.clone();
        value.value.status = if expired {
            ComputerStatus::Expired
        } else {
            ComputerStatus::Destroyed
        };
        value.value.retired.clear();
        value.value.failure = None;
        value.value.ended_at = Some(Utc::now());
        for observed in value.value.observed.processes.values_mut() {
            observed.state = ProcessState::Stopped;
            observed.pid = None;
        }
        self.advance(
            &fenced,
            name,
            value.value,
            Some((
                if expired {
                    events::COMPUTER_EXPIRED
                } else {
                    events::COMPUTER_DESTROYED
                },
                format!(
                    "{name}'s computer was {}; its record remains",
                    if expired { "expired" } else { "destroyed" }
                ),
                json!({ "environment_id": computer.environment_id }),
            )),
        )
        .await?;
        Ok(Step::Done)
    }

    /// Tear down earlier sessions. Returns whether none remain.
    async fn retire(
        &self,
        stored: &Stored<ComputerRecord>,
        name: &str,
    ) -> Result<bool, EnvironmentError> {
        if stored.value.retired.is_empty() {
            return Ok(true);
        }
        let mut value = stored.value.clone();
        let mut kept = vec![];
        for retired in &stored.value.retired {
            let done = match self.target_client(&retired.target) {
                Ok(client) => match client.destroy_session(&retired.session_id).await {
                    Ok(_) => true,
                    Err(error) => matches!(
                        error.kind,
                        ProviderErrorKind::UnknownSession | ProviderErrorKind::SessionConflict
                    ),
                },
                Err(_) => false,
            };
            if !done {
                kept.push(retired.clone());
            }
        }
        value.retired = kept;
        let none_left = value.retired.is_empty();
        if value != stored.value {
            self.advance(stored, name, value, None).await?;
        }
        Ok(none_left)
    }

    /// Mark the computer failed and tear down what it had. Terminal.
    async fn fail_computer(
        &self,
        stored: &Stored<ComputerRecord>,
        name: &str,
    ) -> Result<Step, EnvironmentError> {
        let computer = &stored.value;
        if let (Some(target), Some(session_id)) = (&computer.target, &computer.session_id)
            && let Ok(client) = self.target_client(target)
        {
            let _ = client.destroy_session(session_id).await;
        }
        let mut value = computer.clone();
        value.status = ComputerStatus::Failed;
        value.ended_at = Some(Utc::now());
        self.advance(
            stored,
            name,
            value,
            Some((
                events::COMPUTER_FAILED,
                format!(
                    "{name}'s computer failed: {}",
                    computer
                        .failure
                        .as_ref()
                        .map(|failure| format!("{} ({})", failure.message, failure.code))
                        .unwrap_or_default()
                ),
                json!({ "environment_id": computer.environment_id, "failure": computer.failure }),
            )),
        )
        .await?;
        Ok(Step::Done)
    }

    /// The target did not answer as expected. A target that lost the
    /// session makes the computer fail; anything else is retried.
    async fn target_unreachable(
        &self,
        stored: &Stored<ComputerRecord>,
        name: &str,
        target: &str,
        error: compute_provider::ProviderError,
    ) -> Result<Step, EnvironmentError> {
        if error.kind == ProviderErrorKind::UnknownSession {
            self.record_failure(
                stored,
                name,
                "reconciliation",
                "environment_lost",
                &error.message,
                false,
                Some(target),
            )
            .await?;
            let refreshed = self.fresh_computer(&stored.value.environment_id).await;
            let change = self.event(
                Change::new(),
                events::COMPUTER_ENVIRONMENT_LOST,
                Scope::environment(name),
                format!("target {target} no longer has {name}'s computer"),
                json!({ "environment_id": stored.value.environment_id, "target": target }),
            );
            self.apply(change).await?;
            return self
                .fail_computer(refreshed.as_ref().unwrap_or(stored), name)
                .await;
        }
        self.record_failure(
            stored,
            name,
            "reconciliation",
            "target_unavailable",
            &error.to_string(),
            true,
            Some(target),
        )
        .await?;
        Ok(Step::Wait(BACKOFF))
    }

    /// Record a failure on the computer. The same failure again writes
    /// nothing, so retries do not grow the record.
    #[allow(clippy::too_many_arguments)]
    async fn record_failure(
        &self,
        stored: &Stored<ComputerRecord>,
        name: &str,
        phase: &str,
        code: &str,
        message: &str,
        retryable: bool,
        target: Option<&str>,
    ) -> Result<(), EnvironmentError> {
        if stored.value.failure.as_ref().is_some_and(|failure| {
            failure.phase == phase && failure.code == code && failure.message == message
        }) {
            return Ok(());
        }
        let mut value = stored.value.clone();
        value.failure = Some(ComputerFailure {
            phase: phase.into(),
            code: code.into(),
            message: message.into(),
            retryable,
            target: target.map(str::to_owned),
            at: Utc::now(),
        });
        self.advance(stored, name, value, None).await
    }

    /// Destroy sessions on targets that no live computer owns: what a
    /// provider built after Compute stopped wanting it.
    async fn sweep_orphans(self: Arc<Self>) {
        let (computers, targets) = {
            let inner = self.inner.lock().await;
            let computers = inner
                .desired
                .computers
                .values()
                .map(|computer| (computer.id.clone(), computer.value.clone()))
                .collect::<BTreeMap<_, _>>();
            let targets = computers
                .values()
                .filter_map(|computer| computer.target.clone())
                .chain(computers.values().flat_map(|computer| {
                    computer
                        .retired
                        .iter()
                        .map(|retired| retired.target.clone())
                }))
                .collect::<std::collections::BTreeSet<_>>();
            (computers, targets)
        };
        for target in targets {
            let Ok(client) = self.target_client(&target) else {
                continue;
            };
            let Ok(sessions) = client.sessions().await else {
                continue;
            };
            for session in sessions {
                if session.status.is_terminal() {
                    continue;
                }
                let Some(reference) = &session.reference else {
                    continue;
                };
                let Some((computer_id, _)) = reference.split_once(':') else {
                    continue;
                };
                if !computer_id.starts_with("cmp_") {
                    continue;
                }
                let owned = computers.get(computer_id).is_some_and(|computer| {
                    !computer.status.is_terminal()
                        && (computer.reference.as_ref() == Some(reference)
                            || computer.session_id.as_ref() == Some(&session.session_id.0)
                            || computer
                                .retired
                                .iter()
                                .any(|retired| retired.session_id == session.session_id.0))
                });
                if owned || self.is_shutting_down() {
                    continue;
                }
                if client.destroy_session(&session.session_id.0).await.is_ok() {
                    let environment = computers
                        .get(computer_id)
                        .map(|computer| computer.environment.clone());
                    let change = self.event(
                        Change::new(),
                        events::COMPUTER_ORPHAN_DESTROYED,
                        environment
                            .as_deref()
                            .map(Scope::environment)
                            .unwrap_or_default(),
                        format!("an orphaned computer session on {target} was destroyed"),
                        json!({
                            "target": target,
                            "session_id": session.session_id,
                            "reference": reference,
                        }),
                    );
                    let _ = self.apply(change).await;
                }
            }
        }
    }
}

fn lifecycle_label(lifecycle: &LifecycleChange) -> String {
    match lifecycle.lifecycle {
        ComputerLifecycle::Persistent => "kept until destroyed".into(),
        ComputerLifecycle::Ephemeral => format!(
            "temporary ({}s)",
            lifecycle.ttl_seconds.unwrap_or(DEFAULT_TTL)
        ),
    }
}

fn apply_lifecycle(spec: &mut ComputerSpec, lifecycle: &LifecycleChange) {
    spec.lifecycle = lifecycle.lifecycle;
    match lifecycle.lifecycle {
        ComputerLifecycle::Persistent => {
            spec.ttl_seconds = None;
            spec.expires_at = None;
        }
        ComputerLifecycle::Ephemeral => {
            let ttl = lifecycle.ttl_seconds.unwrap_or(DEFAULT_TTL);
            spec.ttl_seconds = Some(ttl);
            spec.expires_at = Some(Utc::now() + chrono::Duration::seconds(ttl as i64));
        }
    }
}

fn upsert<T>(items: &mut Vec<T>, item: T, name: impl Fn(&T) -> &String) {
    match items
        .iter_mut()
        .find(|existing| name(existing) == name(&item))
    {
        Some(existing) => *existing = item,
        None => items.push(item),
    }
}

fn remove<T>(items: &mut Vec<T>, wanted: &str, name: impl Fn(&T) -> &String) -> bool {
    let before = items.len();
    items.retain(|item| name(item) != wanted);
    items.len() != before
}

fn target_error(error: compute_provider::ProviderError) -> EnvironmentError {
    match error.kind {
        ProviderErrorKind::UnknownSession | ProviderErrorKind::UnknownJob => {
            EnvironmentError::NotFound(error.message)
        }
        ProviderErrorKind::SessionConflict => EnvironmentError::Conflict(error.message),
        ProviderErrorKind::Unauthorized => EnvironmentError::Forbidden(error.message),
        ProviderErrorKind::AdmissionDenied => EnvironmentError::Denied(error.message),
        ProviderErrorKind::OperationUnsupported
        | ProviderErrorKind::CapabilityMismatch
        | ProviderErrorKind::ArtifactInvalid
        | ProviderErrorKind::PolicyRejected => EnvironmentError::Invalid(error.message),
        _ => EnvironmentError::RuntimeUnavailable(error.to_string()),
    }
}

impl Daemon {
    /// Whether an environment has a computer.
    pub async fn has_computer(&self, environment: &str) -> Result<bool, EnvironmentError> {
        self.refresh_for_read().await?;
        Ok(self
            .inner
            .lock()
            .await
            .desired
            .environment(environment)
            .is_some_and(|record| record.value.computer.is_some()))
    }

    /// The pool's members as targets: what each can host.
    pub async fn targets(&self) -> Vec<compute_placement::ComputeTarget> {
        let records = {
            let mut cache = self.cache.lock().await;
            self.pool
                .capabilities(&mut cache, DiscoveryMode::PreferCache, None, Utc::now())
                .await
        };
        let configs = self.pool.configs();
        records
            .iter()
            .filter_map(|record| {
                configs.get(&record.provider_id).map(|config| {
                    compute_placement::ComputeTarget::from_record(record, config.kind)
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence() -> OperationEvidence {
        OperationEvidence {
            job_id: "job_1".into(),
            execution_id: "exec_1".into(),
            outcome: "succeeded".into(),
            at: Utc::now(),
            error: None,
        }
    }

    fn contents() -> EnvironmentContents {
        EnvironmentContents {
            repositories: vec![RepositorySpec {
                name: "app".into(),
                url: "/srv/app.git".into(),
                revision: "main".into(),
                sync: 0,
            }],
            packages: vec![],
            processes: vec![ProcessSpec {
                name: "api".into(),
                kind: compute_core::ProcessKind::Application,
                command: vec!["./serve".into()],
                repository: Some("app".into()),
                env: BTreeMap::new(),
                desired: ProcessDesired::Running,
                port: None,
                restart: 0,
            }],
            projects: vec![],
            generation: 1,
        }
    }

    #[test]
    fn the_plan_converges_in_order_and_restarts_on_a_new_commit() {
        let desired = contents();
        let config = BTreeMap::new();
        let mut observed = ObservedContents::default();
        let Some(Action::SyncRepository(repository, fingerprint)) =
            plan(&desired, &observed, &config)
        else {
            panic!("repositories first");
        };
        observed.repositories.insert(
            repository.name.clone(),
            ObservedRepository {
                revision: "main".into(),
                commit: Some("aaa".into()),
                fingerprint,
                evidence: evidence(),
            },
        );
        let Some(Action::StartProcess(process, started)) = plan(&desired, &observed, &config)
        else {
            panic!("then processes");
        };
        observed.processes.insert(
            process.name.clone(),
            ObservedProcess {
                state: ProcessState::Running,
                fingerprint: started.clone(),
                pid: Some(1),
                evidence: evidence(),
            },
        );
        assert_eq!(plan(&desired, &observed, &config), None, "converged");
        // The repository moves to another commit: the process restarts, in
        // place.
        observed.repositories.get_mut("app").unwrap().commit = Some("bbb".into());
        match plan(&desired, &observed, &config) {
            Some(Action::StartProcess(_, fingerprint)) => assert_ne!(fingerprint, started),
            other => panic!("expected a restart, planned {other:?}"),
        }
    }

    #[test]
    fn stopping_removing_and_failures_are_planned_without_loops() {
        let mut desired = contents();
        let config = BTreeMap::new();
        let mut observed = ObservedContents::default();
        let repository_fingerprint = fingerprint(&desired.repositories[0]);
        observed.repositories.insert(
            "app".into(),
            ObservedRepository {
                revision: "main".into(),
                commit: None,
                fingerprint: repository_fingerprint,
                evidence: OperationEvidence {
                    outcome: "failed".into(),
                    ..evidence()
                },
            },
        );
        // A failed attempt at an unchanged item is not retried by itself.
        let fingerprint = process_fingerprint(&desired.processes[0], &desired, &observed, &config);
        observed.processes.insert(
            "api".into(),
            ObservedProcess {
                state: ProcessState::Failed,
                fingerprint,
                pid: None,
                evidence: evidence(),
            },
        );
        assert_eq!(plan(&desired, &observed, &config), None);
        observed.processes.get_mut("api").unwrap().state = ProcessState::Running;
        desired.processes[0].desired = ProcessDesired::Stopped;
        assert_eq!(
            plan(&desired, &observed, &config),
            Some(Action::StopProcess {
                name: "api".into(),
                forget: false
            })
        );
        desired.processes.clear();
        assert_eq!(
            plan(&desired, &observed, &config),
            Some(Action::StopProcess {
                name: "api".into(),
                forget: true
            })
        );
        observed.processes.clear();
        desired.repositories.clear();
        assert_eq!(
            plan(&desired, &observed, &config),
            Some(Action::RemoveRepository("app".into()))
        );
    }

    #[test]
    fn a_project_build_runs_before_what_runs_from_its_repository() {
        let mut desired = contents();
        desired.projects.push(ProjectSpec {
            name: "app".into(),
            repository: "app".into(),
            build: vec!["make".into()],
            test: vec![],
            commands: BTreeMap::new(),
            checks: vec![],
        });
        let config = BTreeMap::new();
        let mut observed = ObservedContents::default();
        let Some(Action::SyncRepository(_, wanted)) = plan(&desired, &observed, &config) else {
            panic!("the repository first");
        };
        observed.repositories.insert(
            "app".into(),
            ObservedRepository {
                revision: "main".into(),
                commit: Some("aaa".into()),
                fingerprint: wanted,
                evidence: evidence(),
            },
        );
        let Some(Action::Build(project, built)) = plan(&desired, &observed, &config) else {
            panic!("then the build");
        };
        assert_eq!(project.name, "app");
        let mut failed = evidence();
        failed.outcome = "failed".into();
        observed.builds.insert(
            "app".into(),
            ObservedBuild {
                commit: Some("aaa".into()),
                fingerprint: built.clone(),
                evidence: failed,
            },
        );
        // A failed build starts nothing from its repository, and is not
        // retried until something changes.
        assert_eq!(plan(&desired, &observed, &config), None);
        observed.builds.get_mut("app").unwrap().evidence = evidence();
        let Some(Action::StartProcess(process, started)) = plan(&desired, &observed, &config)
        else {
            panic!("then the application");
        };
        assert_eq!(process.name, "api");
        observed.processes.insert(
            "api".into(),
            ObservedProcess {
                state: ProcessState::Running,
                fingerprint: started,
                pid: Some(1),
                evidence: evidence(),
            },
        );
        assert_eq!(plan(&desired, &observed, &config), None);
        // New configuration: build again, then restart.
        let config = BTreeMap::from([("MODE".to_owned(), "x".to_owned())]);
        assert!(matches!(
            plan(&desired, &observed, &config),
            Some(Action::Build(..))
        ));
        // The process sees the configuration, its port, and its own env.
        let mut process = desired.processes[0].clone();
        process.port = Some(8080);
        process.env.insert("MODE".into(), "own".into());
        let env = process_env(&process, &config);
        assert_eq!(env["PORT"], "8080");
        assert_eq!(env["MODE"], "own");
    }
}
