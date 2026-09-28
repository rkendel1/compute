//! Software: projects as the user sees them — inspected, assembled,
//! published as versions, deployed, promoted, and rolled back.
//!
//! ```text
//! propose       a job in the computer inspects the source → an assembly to accept
//! publish       source · build · tests · checks · package   → an immutable Version
//! deploy        a Version → the environment's desired state  → a Rollout, reconciled
//! promote       the Version active in one environment → another
//! rollback      an earlier Version → the environment
//! ```
//!
//! Every operation is a durable record with observable steps, driven by the
//! controller and resumed after a restart; every command runs in an
//! environment's computer. A deployment never provisions a machine: it
//! changes what the existing computer holds.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use compute_core::{
    OperationStep, PackageSpec, ProcessDesired, ProcessKind, ProcessSpec, ProjectAssembly,
    ProjectProposal, ProjectSpec, RepositorySpec, StepStatus,
};
use compute_state::{
    Collection, Query, RolloutKind, RolloutRecord, RolloutStatus, Stored, VersionRecord,
    VersionStatus, events, ids,
};
use serde_json::json;

use super::computers::{INSTALL_PACKAGE, script};
use super::{Change, Daemon, Scope};
use crate::EnvironmentError;
use crate::model::*;
use crate::status::*;

const LIST_LIMIT: usize = 100;
/// How long a rollout may take to become real before it fails.
const ROLLOUT_DEADLINE: Duration = Duration::from_secs(30 * 60);
/// How long a rollout's health check may take once the processes run.
const HEALTH_DEADLINE: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(250);

/// Clone a source into a scratch directory in the computer, report what is
/// there, and remove it. Arguments: URL, revision (may be empty), a nonce.
const INSPECT: &str = r#"set -eu
root="$PWD"
dir="$root/.compute/inspect/$3"
mkdir -p "$root/.compute/inspect"
trap 'cd "$root"; rm -rf "$dir"' EXIT
git clone --quiet -- "$1" "$dir" >&2
cd "$dir"
if [ -n "$2" ]; then git -c advice.detachedHead=false checkout --quiet "$2" >&2; fi
echo "COMMIT $(git rev-parse HEAD)"
echo "BRANCH $(git rev-parse --abbrev-ref HEAD)"
for f in * .env.example .env.sample; do if [ -e "$f" ]; then echo "FILE $f"; fi; done
if [ -f Procfile ]; then sed 's/^/PROCFILE /' Procfile; fi
if [ -f Makefile ]; then grep -E '^[A-Za-z0-9_-]+:' Makefile | sed 's/:.*//; s/^/MAKE /'; fi
if [ -f package.json ]; then echo "PACKAGE_JSON_BEGIN"; head -c 65536 package.json; echo; echo "PACKAGE_JSON_END"; fi
for f in .env.example .env.sample; do if [ -f "$f" ]; then grep -E '^[A-Za-z_][A-Za-z0-9_]*=' "$f" | sed 's/^/ENV /'; fi; done
for f in docker-compose.yml docker-compose.yaml compose.yml compose.yaml; do if [ -f "$f" ]; then grep -Eio 'image: *[a-z0-9./_:-]+' "$f" | sed 's/^/IMAGE /'; fi; done
true
"#;

/// The digest of a commit's source package, refusing a checkout that is
/// not exactly that commit. Arguments: repository, commit.
const PACKAGE: &str = r#"set -eu
cd "repos/$1"
head="$(git rev-parse HEAD)"
if [ "$head" != "$2" ]; then echo "the checkout is at $head, not $2" >&2; exit 1; fi
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  echo "the checkout has uncommitted changes; commit them to the repository and release that revision" >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  git archive --format=tar "$2" | sha256sum | cut -d' ' -f1
else
  git archive --format=tar "$2" | shasum -a 256 | cut -d' ' -f1
fi
"#;

/// A commit as people read it; any other revision as it is.
fn abbreviated(revision: &str) -> String {
    if revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        revision[..12].to_owned()
    } else {
        revision.to_owned()
    }
}

fn shell(command: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), command.into()]
}

/// A project's name from its source: the last path segment, without `.git`.
fn name_from(url: &str) -> String {
    let last = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("project")
        .trim_end_matches(".git");
    let name: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches('-').to_owned();
    if name.is_empty() {
        "project".into()
    } else {
        name
    }
}

/// Build a proposal from what the inspection found. Pure, so what Compute
/// proposes for a kind of project is plain to see and to test.
pub(crate) fn propose(
    output: &str,
    url: &str,
    revision: Option<&str>,
    name: Option<&str>,
    taken_ports: &BTreeSet<u16>,
) -> ProjectProposal {
    let name = name.map(str::to_owned).unwrap_or_else(|| name_from(url));
    let mut files = BTreeSet::new();
    let mut procfile = vec![];
    let mut make = BTreeSet::new();
    let mut env = BTreeMap::new();
    let mut images = vec![];
    let mut branch = None;
    let mut package_json = String::new();
    let mut in_package = false;
    for line in output.lines() {
        if in_package {
            if line == "PACKAGE_JSON_END" {
                in_package = false;
            } else {
                package_json.push_str(line);
                package_json.push('\n');
            }
            continue;
        }
        let (tag, rest) = line.split_once(' ').unwrap_or((line, ""));
        match tag {
            "FILE" => {
                files.insert(rest.to_owned());
            }
            "BRANCH" if rest != "HEAD" => branch = Some(rest.to_owned()),
            "PROCFILE" => {
                if let Some((process, command)) = rest.split_once(':')
                    && !process.trim().is_empty()
                    && !command.trim().is_empty()
                {
                    procfile.push((process.trim().to_owned(), command.trim().to_owned()));
                }
            }
            "MAKE" => {
                make.insert(rest.to_owned());
            }
            "ENV" => {
                if let Some((key, value)) = rest.split_once('=') {
                    env.insert(key.to_owned(), value.trim_matches('"').to_owned());
                }
            }
            "IMAGE" => images.push(rest.to_ascii_lowercase()),
            "PACKAGE_JSON_BEGIN" => in_package = true,
            _ => {}
        }
    }
    let package: Option<serde_json::Value> = serde_json::from_str(&package_json).ok();
    let scripts = package
        .as_ref()
        .and_then(|package| package.get("scripts"))
        .and_then(|scripts| scripts.as_object())
        .cloned()
        .unwrap_or_default();
    let has = |file: &str| files.contains(file);
    let mut notes = vec![];
    let mut evidence = files.iter().cloned().collect::<Vec<_>>();
    evidence.truncate(40);
    let repository = RepositorySpec {
        name: name.clone(),
        url: url.to_owned(),
        revision: revision
            .map(str::to_owned)
            .or(branch)
            .unwrap_or_else(|| "main".into()),
        sync: 0,
    };
    let mut packages = vec![];
    let mut build = vec![];
    let mut test = vec![];
    let mut commands = BTreeMap::new();
    let mut checks = vec![];
    let mut start: Option<String> = None;
    let mut default_port = 8080;
    let runtime;
    if package.is_some() {
        runtime = Some("node".to_owned());
        default_port = 3000;
        packages.push(PackageSpec {
            name: format!("{name}-dependencies"),
            install: shell(if has("package-lock.json") {
                "npm ci"
            } else {
                "npm install"
            }),
            repository: Some(name.clone()),
        });
        if scripts.contains_key("build") {
            build = shell("npm run build");
        }
        if scripts.contains_key("test") {
            test = shell("npm test");
        }
        for check in ["lint", "typecheck", "check"] {
            if scripts.contains_key(check) {
                commands.insert(check.to_owned(), shell(&format!("npm run {check}")));
                checks.push(check.to_owned());
            }
        }
        if scripts.contains_key("dev") {
            commands.insert("dev".into(), shell("npm run dev"));
        }
        if scripts.contains_key("start") {
            start = Some("npm start".into());
        }
    } else if has("requirements.txt") || has("pyproject.toml") {
        runtime = Some("python".to_owned());
        default_port = 8000;
        packages.push(PackageSpec {
            name: format!("{name}-dependencies"),
            install: shell(if has("requirements.txt") {
                "pip install -r requirements.txt"
            } else {
                "pip install ."
            }),
            repository: Some(name.clone()),
        });
        if has("tests") || has("test") {
            test = shell("python -m pytest");
        }
        for entry in ["app.py", "main.py", "manage.py"] {
            if has(entry) {
                start = Some(if entry == "manage.py" {
                    "python manage.py runserver 0.0.0.0:$PORT".into()
                } else {
                    format!("python {entry}")
                });
                break;
            }
        }
    } else if has("go.mod") {
        runtime = Some("go".to_owned());
        build = shell("go build ./...");
        test = shell("go test ./...");
        commands.insert("vet".into(), shell("go vet ./..."));
        checks.push("vet".into());
        start = Some("go run .".into());
    } else if has("Cargo.toml") {
        runtime = Some("rust".to_owned());
        build = shell("cargo build --release");
        test = shell("cargo test");
        start = Some("cargo run --release".into());
    } else if has("Makefile") {
        runtime = Some("make".to_owned());
    } else {
        runtime = None;
    }
    // A Makefile's conventional targets win over guesses.
    if has("Makefile") {
        if make.contains("build") {
            build = shell("make build");
        }
        if make.contains("test") {
            test = shell("make test");
        }
        for check in ["lint", "typecheck", "check"] {
            if make.contains(check) && !commands.contains_key(check) {
                commands.insert(check.to_owned(), shell(&format!("make {check}")));
                checks.push(check.to_owned());
            }
        }
        if make.contains("run") && start.is_none() && procfile.is_empty() {
            start = Some("make run".into());
        }
    }
    let mut port = default_port;
    let mut next_port = || {
        while taken_ports.contains(&port) {
            port += 1;
        }
        let chosen = port;
        port += 1;
        chosen
    };
    let mut processes = vec![];
    if procfile.is_empty() {
        if let Some(command) = start {
            processes.push(ProcessSpec {
                name: name.clone(),
                kind: ProcessKind::Application,
                command: shell(&command),
                repository: Some(name.clone()),
                env: BTreeMap::new(),
                desired: ProcessDesired::Running,
                port: Some(next_port()),
                restart: 0,
                readiness: None,
                restart_policy: Default::default(),
                max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
            });
        } else {
            notes.push(
                "No start command was found (a Procfile, a package.json `start` script, or a \
                 Makefile `run` target). Add one to run the project."
                    .into(),
            );
        }
    } else {
        for (process, command) in &procfile {
            let web = process == "web";
            processes.push(ProcessSpec {
                name: if web {
                    name.clone()
                } else {
                    format!("{name}-{process}")
                },
                kind: if web {
                    ProcessKind::Application
                } else {
                    ProcessKind::Process
                },
                command: shell(command),
                repository: Some(name.clone()),
                env: BTreeMap::new(),
                desired: ProcessDesired::Running,
                port: web.then(&mut next_port),
                restart: 0,
                readiness: None,
                restart_policy: Default::default(),
                max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
            });
        }
    }
    let mut services = vec![];
    let wants = |needle: &str| {
        images.iter().any(|image| image.contains(needle))
            || env.iter().any(|(key, value)| {
                key.to_ascii_lowercase().contains(needle)
                    || value.to_ascii_lowercase().starts_with(needle)
            })
    };
    if wants("postgres") {
        services.push(ProcessSpec {
            name: "database".into(),
            kind: ProcessKind::Service,
            command: shell("postgres -D .compute/postgres -p $PORT"),
            repository: None,
            env: BTreeMap::new(),
            desired: ProcessDesired::Running,
            port: Some(5432),
            restart: 0,
            readiness: None,
            restart_policy: Default::default(),
            max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
        });
        notes.push("It appears to use PostgreSQL: a database service is proposed.".into());
    }
    if wants("redis") {
        services.push(ProcessSpec {
            name: "redis".into(),
            kind: ProcessKind::Service,
            command: shell("redis-server --port $PORT"),
            repository: None,
            env: BTreeMap::new(),
            desired: ProcessDesired::Running,
            port: Some(6379),
            restart: 0,
            readiness: None,
            restart_policy: Default::default(),
            max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
        });
        notes.push("It appears to use Redis: a Redis service is proposed.".into());
    }
    if runtime.is_none() {
        notes.push("Compute did not recognise the project's language; review the commands.".into());
    }
    if !env.is_empty() {
        notes.push(format!(
            "It documents {} configuration value(s); review them before GO.",
            env.len()
        ));
    }
    ProjectProposal {
        name: name.clone(),
        runtime,
        assembly: ProjectAssembly {
            repository: Some(repository),
            project: Some(ProjectSpec {
                name: name.clone(),
                repository: name.clone(),
                build,
                test,
                commands,
                checks,
            }),
            packages,
            processes,
        },
        services,
        config: env,
        notes,
        evidence,
    }
}

/// The version after `latest`: its patch number increased.
pub(crate) fn next_version(latest: Option<&str>) -> String {
    let Some(latest) = latest else {
        return "0.1.0".into();
    };
    let core = latest.trim_start_matches('v');
    let parts = core
        .split(['-', '+'])
        .next()
        .unwrap_or(core)
        .split('.')
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [major, minor, patch] => match (
            major.parse::<u64>(),
            minor.parse::<u64>(),
            patch.parse::<u64>(),
        ) {
            (Ok(major), Ok(minor), Ok(patch)) => {
                let prefix = if latest.starts_with('v') { "v" } else { "" };
                format!("{prefix}{major}.{minor}.{}", patch + 1)
            }
            _ => format!("{latest}.1"),
        },
        _ => format!("{latest}.1"),
    }
}

fn valid_version(label: &str) -> Result<(), EnvironmentError> {
    if label.is_empty()
        || label.len() > 64
        || !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
    {
        return Err(EnvironmentError::Invalid(format!(
            "version {label:?} must be 1-64 letters, digits, '.', '-', '_', or '+'"
        )));
    }
    Ok(())
}

fn step(steps: &[OperationStep], name: &str) -> Option<usize> {
    steps.iter().position(|step| step.name == name)
}

fn set(
    steps: &mut [OperationStep],
    name: &str,
    status: StepStatus,
    detail: Option<String>,
    evidence: Option<(&str, &str)>,
) -> bool {
    let Some(index) = step(steps, name) else {
        return false;
    };
    let current = &steps[index];
    let job = evidence.map(|(job, _)| job.to_owned());
    if current.status == status
        && current.detail == detail
        && (job.is_none() || current.job_id == job)
    {
        return false;
    }
    let step = &mut steps[index];
    step.status = status;
    step.detail = detail;
    if let Some((job, execution)) = evidence {
        step.job_id = (!job.is_empty()).then(|| job.to_owned());
        step.execution_id = (!execution.is_empty()).then(|| execution.to_owned());
    }
    step.at = Some(Utc::now());
    true
}

impl Daemon {
    /// The ports processes of every environment listen on, and the
    /// endpoint ports node environments hold: computers and the node model
    /// may share a host, and neither may take the other's port.
    pub(crate) async fn ports_in_use(&self) -> BTreeSet<u16> {
        let inner = self.inner.lock().await;
        let desired = &inner.desired;
        desired
            .environments
            .values()
            .filter_map(|record| record.value.contents.as_ref())
            .flat_map(|contents| contents.processes.iter().filter_map(|process| process.port))
            .chain(
                desired
                    .traffic
                    .values()
                    .map(|assignment| assignment.value.host_port),
            )
            .chain(
                desired
                    .workloads
                    .values()
                    .flat_map(|workload| workload.value.ports.iter().map(|binding| binding.host)),
            )
            .collect()
    }

    // ---- Inspecting a project ---------------------------------------------

    /// Inspect a project's source inside the environment's computer and
    /// propose an assembly: runtime, dependencies, build, tests, checks,
    /// start command, ports, services, configuration. Nothing changes until
    /// the proposal is submitted.
    pub async fn propose_project(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        request: ProposeRequest,
    ) -> Result<ProjectProposal, EnvironmentError> {
        if request.url.is_empty()
            || request.url.starts_with('-')
            || request.url.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(EnvironmentError::Invalid(
                "a source URL or path is required".into(),
            ));
        }
        if let Some(revision) = &request.revision
            && (revision.starts_with('-') || revision.bytes().any(|byte| byte.is_ascii_control()))
        {
            return Err(EnvironmentError::Invalid("invalid revision".into()));
        }
        let record = self.owned_environment(environment, operator).await?;
        let (_, client, session_id) = self.running(&record).await?;
        let nonce = crate::auth::hex(&crate::auth::random::<8>()?);
        let (evidence, output) = self
            .run_in_computer_command(
                &client,
                &session_id,
                script(
                    INSPECT,
                    [
                        request.url.clone(),
                        request.revision.clone().unwrap_or_default(),
                        nonce,
                    ],
                ),
                Duration::from_secs(10 * 60),
            )
            .await;
        if evidence.outcome != "succeeded" {
            return Err(EnvironmentError::Invalid(format!(
                "Compute could not read {}: {} (job {})",
                request.url,
                evidence.error.unwrap_or_default(),
                evidence.job_id
            )));
        }
        let taken = self.ports_in_use().await;
        let mut proposal = propose(
            &output,
            &request.url,
            request.revision.as_deref(),
            request.name.as_deref(),
            &taken,
        );
        proposal
            .evidence
            .push(format!("inspected by job {}", evidence.job_id));
        let change = self.event(
            Change::new(),
            events::ENVIRONMENT_COMMAND,
            Scope::environment(&record.value.name).execution(&evidence.execution_id),
            format!(
                "{operator} inspected {} in {}",
                request.url, record.value.name
            ),
            json!({
                "environment_id": record.id,
                "command": "inspect",
                "source": request.url,
                "job_id": evidence.job_id,
                "execution_id": evidence.execution_id,
            }),
        );
        self.apply(change).await?;
        Ok(proposal)
    }

    /// Restart a process in place: nothing else about it changes.
    pub async fn restart_process(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        name: &str,
    ) -> Result<ComputerView, EnvironmentError> {
        let name = name.to_owned();
        self.change_contents(
            environment,
            operator,
            format!("process {name} restarted"),
            move |contents| {
                let process = contents
                    .processes
                    .iter_mut()
                    .find(|process| process.name == name)
                    .ok_or_else(|| EnvironmentError::NotFound(format!("process {name}")))?;
                process.restart += 1;
                process.desired = ProcessDesired::Running;
                Ok(())
            },
        )
        .await
    }

    // ---- Versions --------------------------------------------------------

    async fn latest_version(
        &self,
        project: &str,
    ) -> Result<Option<VersionRecord>, EnvironmentError> {
        Ok(self
            .control()
            .query::<VersionRecord>(
                Query::all(Collection::Version)
                    .eq("project", project.to_owned())
                    .descending("created_at")
                    .limit(1),
            )
            .await?
            .into_iter()
            .next()
            .map(|stored| stored.value))
    }

    /// Publish a version of a project from the environment it is developed
    /// in: its source, build, tests, and checks, run in that environment's
    /// computer, and the digest of its source package. A durable operation:
    /// follow it with [`Self::version`].
    pub async fn publish_version(
        self: &Arc<Self>,
        project: &str,
        operator: &str,
        request: PublishRequest,
    ) -> Result<VersionRecord, EnvironmentError> {
        self.publish_version_from(project, operator, request, None)
            .await
    }

    /// Publish a version whose source was imported into the computer from
    /// an artifact, and record which one: the same publish.
    pub(crate) async fn publish_version_from(
        self: &Arc<Self>,
        project: &str,
        operator: &str,
        request: PublishRequest,
        artifact: Option<compute_state::ApplicationArtifactEvidence>,
    ) -> Result<VersionRecord, EnvironmentError> {
        let record = self
            .owned_environment(&request.environment, operator)
            .await?;
        self.require_live(&record).await?;
        let contents = record.value.contents.clone().unwrap_or_default();
        let spec = contents
            .projects
            .iter()
            .find(|spec| spec.name == project)
            .cloned()
            .ok_or_else(|| {
                EnvironmentError::NotFound(format!(
                    "project {project} in environment {}",
                    request.environment
                ))
            })?;
        let label = match request.version {
            Some(label) => label,
            None => next_version(
                self.latest_version(project)
                    .await?
                    .as_ref()
                    .map(|version| version.version.as_str()),
            ),
        };
        valid_version(&label)?;
        let version_id = ids::version(project, &label);
        if self
            .control()
            .get::<VersionRecord>(&version_id)
            .await?
            .is_some()
        {
            return Err(EnvironmentError::Conflict(format!(
                "{project} {label} already exists; versions are immutable"
            )));
        }
        let mut steps = ["Source", "Build", "Tests", "Checks", "Package", "Version"]
            .map(OperationStep::pending)
            .to_vec();
        if spec.build.is_empty() {
            set(
                &mut steps,
                "Build",
                StepStatus::Skipped,
                Some("no build".into()),
                None,
            );
        }
        if spec.test.is_empty() {
            set(
                &mut steps,
                "Tests",
                StepStatus::Skipped,
                Some("no tests".into()),
                None,
            );
        }
        if spec.checks.is_empty() {
            set(
                &mut steps,
                "Checks",
                StepStatus::Skipped,
                Some("no checks".into()),
                None,
            );
        }
        let value = VersionRecord {
            version_id: version_id.clone(),
            project: project.to_owned(),
            version: label.clone(),
            environment_id: record.id.clone(),
            environment: record.value.name.clone(),
            commit: None,
            package_digest: None,
            artifact,
            assembly: ProjectAssembly {
                project: Some(spec),
                ..Default::default()
            },
            config_keys: record.value.config.keys().cloned().collect(),
            status: VersionStatus::Publishing,
            steps,
            created_by: operator.to_owned(),
            created_at: Utc::now(),
            completed_at: None,
            failure: None,
        };
        let change = Change::new().with(|batch| batch.create(&version_id, &value));
        let change = self.event(
            change,
            events::VERSION_PUBLISHING,
            Scope::environment(&record.value.name),
            format!(
                "{operator} is publishing {project} {label} from {}",
                record.value.name
            ),
            json!({
                "environment_id": record.id,
                "project": project,
                "version": label,
                "version_id": version_id,
            }),
        );
        self.apply(change).await?;
        self.spawn_operation(version_id.clone());
        Ok(value)
    }

    /// A version of a project, by its label.
    pub async fn version(
        &self,
        project: &str,
        label: &str,
    ) -> Result<VersionRecord, EnvironmentError> {
        self.control()
            .get::<VersionRecord>(&ids::version(project, label))
            .await?
            .map(|stored| stored.value)
            .ok_or_else(|| EnvironmentError::NotFound(format!("{project} {label}")))
    }

    /// A project's versions, newest first.
    pub async fn versions(&self, project: &str) -> Result<Vec<VersionRecord>, EnvironmentError> {
        Ok(self
            .control()
            .query::<VersionRecord>(
                Query::all(Collection::Version)
                    .eq("project", project.to_owned())
                    .descending("created_at")
                    .limit(LIST_LIMIT),
            )
            .await?
            .into_iter()
            .map(|stored| stored.value)
            .collect())
    }

    /// Run the next step of a publish. Returns whether there is more to do.
    async fn publish_step(self: &Arc<Self>, version_id: &str) -> Result<bool, EnvironmentError> {
        let Some(stored) = self.control().get::<VersionRecord>(version_id).await? else {
            return Ok(false);
        };
        if stored.value.status != VersionStatus::Publishing {
            return Ok(false);
        }
        let mut value = stored.value.clone();
        let Some(index) = value.steps.iter().position(|step| !step.status.is_done()) else {
            return Ok(false);
        };
        let name = value.steps[index].name.clone();
        let fail = |mut value: VersionRecord, message: String| {
            value.status = VersionStatus::Failed;
            value.failure = Some(message);
            value.completed_at = Some(Utc::now());
            value
        };
        self.refresh_for_read().await?;
        let record = {
            let inner = self.inner.lock().await;
            inner
                .desired
                .environments
                .values()
                .find(|record| record.id == value.environment_id)
                .cloned()
        };
        let Some(record) = record else {
            let value = fail(value, "its environment no longer exists".into());
            self.finish_version(&stored, value).await?;
            return Ok(false);
        };
        let (computer, client, session_id) = match self.running(&record).await {
            Ok(running) => running,
            Err(error) => {
                if self
                    .stored_computer(&record.id)
                    .await
                    .is_some_and(|computer| computer.value.status.is_terminal())
                {
                    let value = fail(value, format!("its computer ended: {error}"));
                    self.finish_version(&stored, value).await?;
                    return Ok(false);
                }
                // Not running yet (resuming, provisioning): wait.
                tokio::time::sleep(Duration::from_secs(1)).await;
                return Ok(true);
            }
        };
        let spec = value
            .assembly
            .project
            .clone()
            .expect("a publish names its project");
        let config = record.value.config.clone();
        if value.steps[index].status != StepStatus::Running {
            set(&mut value.steps, &name, StepStatus::Running, None, None);
            self.write_version(&stored, value).await?;
        }
        let stored = self
            .control()
            .get::<VersionRecord>(version_id)
            .await?
            .ok_or_else(|| EnvironmentError::NotFound(version_id.into()))?;
        let mut value = stored.value.clone();
        let run = |argv: Vec<String>| {
            let mut arguments = vec![spec.repository.clone()];
            arguments.extend(argv);
            let mut command = script(INSTALL_PACKAGE, arguments);
            command.env = config.clone();
            command
        };
        match name.as_str() {
            "Source" => {
                let contents = record.value.contents.clone().unwrap_or_default();
                let observed = computer.observed.clone();
                let Some(seen) = observed.repositories.get(&spec.repository) else {
                    let value = fail(
                        value,
                        format!("repository {} is not checked out", spec.repository),
                    );
                    self.finish_version(&stored, value).await?;
                    return Ok(false);
                };
                let Some(commit) = seen.commit.clone() else {
                    let value = fail(
                        value,
                        format!("repository {} has no commit", spec.repository),
                    );
                    self.finish_version(&stored, value).await?;
                    return Ok(false);
                };
                let repository = contents
                    .repositories
                    .iter()
                    .find(|repository| repository.name == spec.repository)
                    .cloned()
                    .map(|mut repository| {
                        repository.revision = commit.clone();
                        repository.sync = 0;
                        repository
                    });
                value.assembly = ProjectAssembly {
                    repository,
                    project: Some(spec.clone()),
                    packages: contents
                        .packages
                        .iter()
                        .filter(|package| package.repository.as_deref() == Some(&spec.repository))
                        .cloned()
                        .collect(),
                    processes: contents
                        .processes
                        .iter()
                        .filter(|process| process.repository.as_deref() == Some(&spec.repository))
                        .cloned()
                        .map(|mut process| {
                            process.restart = 0;
                            process
                        })
                        .collect(),
                };
                value.commit = Some(commit.clone());
                set(
                    &mut value.steps,
                    "Source",
                    StepStatus::Succeeded,
                    Some(format!(
                        "{} at {} ({})",
                        spec.repository,
                        seen.revision,
                        &commit[..commit.len().min(12)]
                    )),
                    None,
                );
            }
            "Build" | "Tests" | "Checks" => {
                let commands = match name.as_str() {
                    "Build" => vec![("build".to_owned(), spec.build.clone())],
                    "Tests" => vec![("test".to_owned(), spec.test.clone())],
                    _ => spec
                        .checks
                        .iter()
                        .filter_map(|check| {
                            spec.commands
                                .get(check)
                                .map(|argv| (check.clone(), argv.clone()))
                        })
                        .collect(),
                };
                let mut passed = vec![];
                for (label, argv) in commands {
                    let (evidence, _) = self
                        .run_in_computer_command(
                            &client,
                            &session_id,
                            run(argv),
                            Duration::from_secs(60 * 60),
                        )
                        .await;
                    if evidence.outcome != "succeeded" {
                        set(
                            &mut value.steps,
                            &name,
                            StepStatus::Failed,
                            Some(format!(
                                "{label} failed: {}",
                                evidence.error.clone().unwrap_or_default()
                            )),
                            Some((evidence.job_id.as_str(), evidence.execution_id.as_str())),
                        );
                        let message = format!(
                            "{name} failed ({label}, job {}): {}",
                            evidence.job_id,
                            evidence.error.unwrap_or_default()
                        );
                        let value = fail(value, message);
                        self.finish_version(&stored, value).await?;
                        return Ok(false);
                    }
                    passed.push(label.clone());
                    set(
                        &mut value.steps,
                        &name,
                        StepStatus::Running,
                        Some(format!("passed: {}", passed.join(", "))),
                        Some((evidence.job_id.as_str(), evidence.execution_id.as_str())),
                    );
                }
                let job = value.steps[index].job_id.clone().unwrap_or_default();
                let execution = value.steps[index].execution_id.clone().unwrap_or_default();
                set(
                    &mut value.steps,
                    &name,
                    StepStatus::Succeeded,
                    Some(format!("passed: {}", passed.join(", "))),
                    Some((job.as_str(), execution.as_str())),
                );
            }
            "Package" => {
                let commit = value.commit.clone().unwrap_or_default();
                let (evidence, output) = self
                    .run_in_computer_command(
                        &client,
                        &session_id,
                        script(PACKAGE, [spec.repository.clone(), commit]),
                        Duration::from_secs(10 * 60),
                    )
                    .await;
                if evidence.outcome != "succeeded" {
                    set(
                        &mut value.steps,
                        "Package",
                        StepStatus::Failed,
                        evidence.error.clone(),
                        Some((evidence.job_id.as_str(), evidence.execution_id.as_str())),
                    );
                    let message = format!("Package failed: {}", evidence.error.unwrap_or_default());
                    let value = fail(value, message);
                    self.finish_version(&stored, value).await?;
                    return Ok(false);
                }
                let digest = format!("sha256:{}", output.trim());
                value.package_digest = Some(digest.clone());
                set(
                    &mut value.steps,
                    "Package",
                    StepStatus::Succeeded,
                    Some(digest),
                    Some((evidence.job_id.as_str(), evidence.execution_id.as_str())),
                );
            }
            _ => {
                set(
                    &mut value.steps,
                    "Version",
                    StepStatus::Succeeded,
                    Some(format!("{} {}", value.project, value.version)),
                    None,
                );
                value.status = VersionStatus::Published;
                value.completed_at = Some(Utc::now());
                self.finish_version(&stored, value).await?;
                return Ok(false);
            }
        }
        self.write_version(&stored, value).await?;
        Ok(true)
    }

    async fn write_version(
        &self,
        stored: &Stored<VersionRecord>,
        value: VersionRecord,
    ) -> Result<VersionRecord, EnvironmentError> {
        self.apply(Change::new().with(|batch| batch.replace(stored, &value)))
            .await?;
        Ok(value)
    }

    async fn finish_version(
        &self,
        stored: &Stored<VersionRecord>,
        value: VersionRecord,
    ) -> Result<(), EnvironmentError> {
        let (kind, message) = match value.status {
            VersionStatus::Published => (
                events::VERSION_PUBLISHED,
                format!("{} {} is published", value.project, value.version),
            ),
            _ => (
                events::VERSION_FAILED,
                format!(
                    "{} {} was not published: {}",
                    value.project,
                    value.version,
                    value.failure.clone().unwrap_or_default()
                ),
            ),
        };
        let change = Change::new().with(|batch| batch.replace(stored, &value));
        let change = self.event(
            change,
            kind,
            Scope::environment(&value.environment),
            message,
            json!({
                "environment_id": value.environment_id,
                "project": value.project,
                "version": value.version,
                "version_id": value.version_id,
                "commit": value.commit,
                "package_digest": value.package_digest,
                "failure": value.failure,
            }),
        );
        self.apply(change).await
    }

    // ---- Rollouts ----------------------------------------------------------

    async fn rollouts_where(&self, query: Query) -> Result<Vec<RolloutRecord>, EnvironmentError> {
        Ok(self
            .control()
            .query::<RolloutRecord>(query.descending("created_at").limit(LIST_LIMIT))
            .await?
            .into_iter()
            .map(|stored| stored.value)
            .collect())
    }

    /// Rollouts, newest first: of an environment, of a project, or both.
    pub async fn rollouts(
        &self,
        environment: Option<&str>,
        project: Option<&str>,
    ) -> Result<Vec<RolloutRecord>, EnvironmentError> {
        let mut rollouts = match (environment, project) {
            (Some(environment), _) => {
                self.refresh_for_read().await?;
                let id = self
                    .inner
                    .lock()
                    .await
                    .desired
                    .environment(environment)
                    .map(|record| record.id.clone())
                    .ok_or_else(|| {
                        EnvironmentError::NotFound(format!("environment {environment}"))
                    })?;
                self.rollouts_where(Query::all(Collection::Rollout).eq("environment_id", id))
                    .await?
            }
            (None, Some(project)) => {
                self.rollouts_where(
                    Query::all(Collection::Rollout).eq("project", project.to_owned()),
                )
                .await?
            }
            (None, None) => {
                return Err(EnvironmentError::Invalid(
                    "name an environment or a project".into(),
                ));
            }
        };
        if let Some(project) = project {
            rollouts.retain(|rollout| rollout.project == project);
        }
        Ok(rollouts)
    }

    pub async fn rollout(&self, rollout_id: &str) -> Result<RolloutRecord, EnvironmentError> {
        self.control()
            .get::<RolloutRecord>(rollout_id)
            .await?
            .map(|stored| stored.value)
            .ok_or_else(|| EnvironmentError::NotFound(format!("rollout {rollout_id}")))
    }

    /// The rollout that is current for a project in an environment.
    async fn active_rollout(
        &self,
        environment_id: &str,
        project: &str,
    ) -> Result<Option<RolloutRecord>, EnvironmentError> {
        Ok(self
            .rollouts_where(
                Query::all(Collection::Rollout).eq("environment_id", environment_id.to_owned()),
            )
            .await?
            .into_iter()
            .find(|rollout| rollout.project == project && rollout.status == RolloutStatus::Active))
    }

    /// Deploy a published version to an environment.
    pub async fn deploy_version(
        self: &Arc<Self>,
        project: &str,
        operator: &str,
        request: DeployVersionRequest,
    ) -> Result<RolloutRecord, EnvironmentError> {
        self.roll_out(
            project,
            operator,
            &request.environment,
            &request.version,
            RolloutKind::Deploy,
            None,
            request.expected_generation,
        )
        .await
    }

    /// Promote the version active in one environment to another.
    pub async fn promote_version(
        self: &Arc<Self>,
        project: &str,
        operator: &str,
        request: PromoteVersionRequest,
    ) -> Result<RolloutRecord, EnvironmentError> {
        let from = self.owned_environment(&request.from, operator).await?;
        let active = self
            .active_rollout(&from.id, project)
            .await?
            .ok_or_else(|| {
                EnvironmentError::Conflict(format!(
                    "no version of {project} is running in {}; deploy one there first",
                    request.from
                ))
            })?;
        self.roll_out(
            project,
            operator,
            &request.to,
            &active.version,
            RolloutKind::Promote,
            Some(request.from.clone()),
            request.expected_generation,
        )
        .await
    }

    /// Roll an environment back to an earlier version.
    pub async fn rollback_version(
        self: &Arc<Self>,
        project: &str,
        operator: &str,
        request: RollbackRequest,
    ) -> Result<RolloutRecord, EnvironmentError> {
        let record = self
            .owned_environment(&request.environment, operator)
            .await?;
        let label = match request.version {
            Some(label) => label,
            None => {
                let active = self.active_rollout(&record.id, project).await?;
                active
                    .and_then(|active| active.previous_version)
                    .ok_or_else(|| {
                        EnvironmentError::Conflict(format!(
                            "{project} has no earlier version in {}",
                            request.environment
                        ))
                    })?
            }
        };
        self.roll_out(
            project,
            operator,
            &request.environment,
            &label,
            RolloutKind::Rollback,
            None,
            None,
        )
        .await
    }

    /// What promoting would do, for review before GO.
    pub async fn promotion_plan(
        self: &Arc<Self>,
        project: &str,
        operator: &str,
        from: &str,
        to: &str,
    ) -> Result<PromotionPlan, EnvironmentError> {
        let source = self.owned_environment(from, operator).await?;
        let target = self.owned_environment(to, operator).await?;
        let active = self
            .active_rollout(&source.id, project)
            .await?
            .ok_or_else(|| {
                EnvironmentError::Conflict(format!("no version of {project} is running in {from}"))
            })?;
        let version = self.version(project, &active.version).await?;
        let current = self.active_rollout(&target.id, project).await?;
        let from_view = self.computer(from).await?;
        let from_healthy = from_view.converged
            && from_view.endpoints.iter().all(|endpoint| endpoint.serving)
            && from_view
                .observed
                .processes
                .values()
                .all(|process| process.state == compute_core::ProcessState::Running);
        let only = |a: &BTreeMap<String, String>, b: &BTreeMap<String, String>| {
            a.keys()
                .filter(|key| !b.contains_key(*key))
                .cloned()
                .collect::<Vec<_>>()
        };
        let different = source
            .value
            .config
            .iter()
            .filter(|(key, value)| {
                target
                    .value
                    .config
                    .get(*key)
                    .is_some_and(|other| other != *value)
            })
            .map(|(key, _)| key.clone())
            .collect();
        let contents = target.value.contents.clone().unwrap_or_default();
        let mut changes = vec![];
        if let Some(repository) = &version.assembly.repository {
            match contents
                .repositories
                .iter()
                .find(|existing| existing.name == repository.name)
            {
                Some(existing) => changes.push(format!(
                    "repository {}: {} → {}",
                    repository.name,
                    abbreviated(&existing.revision),
                    abbreviated(&repository.revision)
                )),
                None => changes.push(format!(
                    "add repository {} at {}",
                    repository.name,
                    abbreviated(&repository.revision)
                )),
            }
        }
        for process in &version.assembly.processes {
            if !contents
                .processes
                .iter()
                .any(|existing| existing.name == process.name)
            {
                changes.push(format!("add {} {}", process.kind.as_str(), process.name));
            } else {
                changes.push(format!(
                    "restart {} {}",
                    process.kind.as_str(),
                    process.name
                ));
            }
        }
        Ok(PromotionPlan {
            project: project.to_owned(),
            from: from.to_owned(),
            to: to.to_owned(),
            version: version.version,
            version_id: version.version_id,
            from_healthy,
            to_current: current.map(|current| current.version),
            changes,
            config_only_in_from: only(&source.value.config, &target.value.config),
            config_only_in_to: only(&target.value.config, &source.value.config),
            config_different: different,
            authority: format!("{operator}: owner of both environments, with the deploy scope"),
            approvals: vec![],
            expected_generation: contents.generation,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn roll_out(
        self: &Arc<Self>,
        project: &str,
        operator: &str,
        environment: &str,
        label: &str,
        kind: RolloutKind,
        from_environment: Option<String>,
        expected_generation: Option<u64>,
    ) -> Result<RolloutRecord, EnvironmentError> {
        let version = self.version(project, label).await?;
        if version.status != VersionStatus::Published {
            return Err(EnvironmentError::Conflict(format!(
                "{project} {label} is {}, not published",
                version.status.as_str()
            )));
        }
        let record = self.owned_environment(environment, operator).await?;
        let recent = self
            .rollouts_where(Query::all(Collection::Rollout).eq("environment_id", record.id.clone()))
            .await?;
        if let Some(in_flight) = recent
            .iter()
            .find(|rollout| rollout.project == project && rollout.status == RolloutStatus::Applying)
        {
            return Err(EnvironmentError::Conflict(format!(
                "{project} {} is still being rolled out to {environment} ({}); wait for it",
                in_flight.version, in_flight.rollout_id
            )));
        }
        let previous = recent
            .iter()
            .find(|rollout| rollout.project == project && rollout.status == RolloutStatus::Active)
            .cloned();
        let nonce = crate::auth::hex(&crate::auth::random::<8>()?);
        let rollout_id = ids::rollout(&record.id, project, &nonce);
        let assembly = version.assembly.clone();
        let taken = self.ports_in_use().await;
        let description = format!(
            "{} {project} {label}{}",
            kind.as_str(),
            from_environment
                .as_ref()
                .map(|from| format!(" from {from}"))
                .unwrap_or_default()
        );
        let rollout = std::sync::Mutex::new(None::<RolloutRecord>);
        self.change_environment_with(
                environment,
                operator,
                description.clone(),
                None,
                |value| {
                    let contents = value.contents.get_or_insert_with(Default::default);
                    if let Some(expected) = expected_generation
                        && expected != contents.generation
                    {
                        return Err(EnvironmentError::Conflict(format!(
                            "the environment changed since you loaded it (generation {expected}, now {})",
                            contents.generation
                        )));
                    }
                    apply_assembly(contents, &assembly, &taken);
                    Ok(())
                },
                |change, environment_record, generation| {
                    let mut steps = [
                        "Desired state",
                        "Checkout",
                        "Build",
                        "Restart applications",
                        "Health check",
                    ]
                    .map(OperationStep::pending)
                    .to_vec();
                    set(
                        &mut steps,
                        "Desired state",
                        StepStatus::Succeeded,
                        Some(format!("generation {generation}")),
                        None,
                    );
                    if assembly.project.as_ref().is_none_or(|spec| spec.build.is_empty()) {
                        set(&mut steps, "Build", StepStatus::Skipped, Some("no build".into()), None);
                    }
                    let value = RolloutRecord {
                        rollout_id: rollout_id.clone(),
                        kind,
                        project: project.to_owned(),
                        environment_id: environment_record.id.clone(),
                        environment: environment_record.value.name.clone(),
                        version_id: version.version_id.clone(),
                        version: version.version.clone(),
                        previous_version_id: previous.as_ref().map(|previous| previous.version_id.clone()),
                        previous_version: previous.as_ref().map(|previous| previous.version.clone()),
                        from_environment: from_environment.clone(),
                        status: RolloutStatus::Applying,
                        steps,
                        contents_generation: generation,
                        created_by: operator.to_owned(),
                        created_at: Utc::now(),
                        completed_at: None,
                        failure: None,
                    };
                    let change = change.with(|batch| batch.create(&rollout_id, &value));
                    let change = self.event(
                        change,
                        events::ROLLOUT_STARTED,
                        Scope::environment(&environment_record.value.name),
                        format!(
                            "{operator}: {} {project} {} to {}",
                            kind.as_str(),
                            version.version,
                            environment_record.value.name
                        ),
                        json!({
                            "environment_id": environment_record.id,
                            "rollout_id": rollout_id,
                            "kind": kind,
                            "project": project,
                            "version": version.version,
                            "version_id": version.version_id,
                            "previous_version": value.previous_version,
                            "from_environment": from_environment,
                            "generation": generation,
                        }),
                    );
                    *rollout.lock().expect("rollout") = Some(value);
                    Ok(change)
                },
            )
            .await?;
        self.spawn_operation(rollout_id.clone());
        rollout
            .into_inner()
            .expect("rollout")
            .ok_or(EnvironmentError::NotFound(rollout_id))
    }

    /// Look at a rollout against what the computer holds, and advance its
    /// steps. Returns whether there is more to do.
    async fn rollout_step(self: &Arc<Self>, rollout_id: &str) -> Result<bool, EnvironmentError> {
        let Some(stored) = self.control().get::<RolloutRecord>(rollout_id).await? else {
            return Ok(false);
        };
        if stored.value.status != RolloutStatus::Applying {
            return Ok(false);
        }
        let mut value = stored.value.clone();
        let version = self.version(&value.project, &value.version).await?;
        let Ok(view) = self.computer(&value.environment).await else {
            value.status = RolloutStatus::Failed;
            value.failure = Some("the environment no longer has a computer".into());
            self.finish_rollout(&stored, value).await?;
            return Ok(false);
        };
        let age = (Utc::now() - value.created_at).to_std().unwrap_or_default();
        let fail = |mut value: RolloutRecord,
                    step: &str,
                    message: String,
                    job: Option<(String, String)>| {
            set(
                &mut value.steps,
                step,
                StepStatus::Failed,
                Some(message.clone()),
                job.as_ref()
                    .map(|(job, execution)| (job.as_str(), execution.as_str())),
            );
            value.status = RolloutStatus::Failed;
            value.failure = Some(format!("{step}: {message}"));
            value
        };
        if view.status.is_terminal() {
            let value = fail(
                value,
                "Checkout",
                format!("the computer is {}", view.status),
                None,
            );
            self.finish_rollout(&stored, value).await?;
            return Ok(false);
        }
        if age > ROLLOUT_DEADLINE {
            let pending = value
                .steps
                .iter()
                .find(|step| !step.status.is_done())
                .map(|step| step.name.clone())
                .unwrap_or_else(|| "Health check".into());
            let value = fail(
                value,
                &pending,
                "did not finish within 30 minutes".into(),
                None,
            );
            self.finish_rollout(&stored, value).await?;
            return Ok(false);
        }
        let repository = version.assembly.repository.clone();
        let commit = version.commit.clone().unwrap_or_default();
        let project = version.assembly.project.clone();
        // Checkout.
        if let Some(repository) = &repository {
            match view.observed.repositories.get(&repository.name) {
                Some(seen) if seen.commit.as_deref() == Some(commit.as_str()) => {
                    set(
                        &mut value.steps,
                        "Checkout",
                        StepStatus::Succeeded,
                        Some(format!(
                            "{} at {}",
                            repository.name,
                            &commit[..commit.len().min(12)]
                        )),
                        Some((
                            seen.evidence.job_id.as_str(),
                            seen.evidence.execution_id.as_str(),
                        )),
                    );
                }
                Some(seen)
                    if seen.evidence.outcome != "succeeded"
                        && seen.revision == repository.revision =>
                {
                    let error = seen.evidence.error.clone().unwrap_or_default();
                    let job = Some((
                        seen.evidence.job_id.clone(),
                        seen.evidence.execution_id.clone(),
                    ));
                    let value = fail(value, "Checkout", error, job);
                    self.finish_rollout(&stored, value).await?;
                    return Ok(false);
                }
                _ => {
                    set(
                        &mut value.steps,
                        "Checkout",
                        StepStatus::Running,
                        None,
                        None,
                    );
                }
            }
        } else {
            set(
                &mut value.steps,
                "Checkout",
                StepStatus::Skipped,
                None,
                None,
            );
        }
        let checked_out =
            step(&value.steps, "Checkout").is_some_and(|index| value.steps[index].status.is_done());
        // Build.
        if checked_out && let Some(spec) = project.as_ref().filter(|spec| !spec.build.is_empty()) {
            match view.observed.builds.get(&spec.name) {
                Some(built) if built.commit.as_deref() == Some(commit.as_str()) => {
                    if built.evidence.outcome == "succeeded" {
                        set(
                            &mut value.steps,
                            "Build",
                            StepStatus::Succeeded,
                            Some(format!("built {}", &commit[..commit.len().min(12)])),
                            Some((
                                built.evidence.job_id.as_str(),
                                built.evidence.execution_id.as_str(),
                            )),
                        );
                    } else {
                        let error = built
                            .evidence
                            .error
                            .clone()
                            .unwrap_or_else(|| "the build failed".into());
                        let job = Some((
                            built.evidence.job_id.clone(),
                            built.evidence.execution_id.clone(),
                        ));
                        let value = fail(value, "Build", error, job);
                        self.finish_rollout(&stored, value).await?;
                        return Ok(false);
                    }
                }
                _ => {
                    set(&mut value.steps, "Build", StepStatus::Running, None, None);
                }
            }
        }
        let built =
            step(&value.steps, "Build").is_some_and(|index| value.steps[index].status.is_done());
        // The processes restarted: the computer holds the change.
        let processes = version
            .assembly
            .processes
            .iter()
            .map(|process| process.name.clone())
            .collect::<Vec<_>>();
        if checked_out && built {
            if view.converged && view.observed.converged_generation >= value.contents_generation {
                // The execution that made the version run: the durable job
                // that started its (first) process, and that job's receipt.
                let started = processes
                    .iter()
                    .find_map(|name| view.observed.processes.get(name))
                    .map(|seen| seen.evidence.clone());
                let restart = step(&value.steps, "Restart applications");
                let recorded = restart.and_then(|index| value.steps[index].job_id.clone());
                set(
                    &mut value.steps,
                    "Restart applications",
                    StepStatus::Succeeded,
                    Some(if processes.is_empty() {
                        "nothing runs from it".into()
                    } else {
                        processes.join(", ")
                    }),
                    started
                        .as_ref()
                        .map(|evidence| (evidence.job_id.as_str(), evidence.execution_id.as_str())),
                );
                if let (Some(index), Some(evidence)) = (restart, &started)
                    && (recorded.as_deref() != Some(evidence.job_id.as_str())
                        || value.steps[index].receipt.is_none())
                {
                    value.steps[index].receipt = self.job_receipt_id(&view, &evidence.job_id).await;
                }
            } else {
                set(
                    &mut value.steps,
                    "Restart applications",
                    StepStatus::Running,
                    None,
                    None,
                );
            }
        }
        let restarted = step(&value.steps, "Restart applications")
            .is_some_and(|index| value.steps[index].status.is_done());
        // Health: every process running (and ready, when it has a readiness
        // check, as a check inside the computer found it), every endpoint
        // answering.
        if restarted {
            let down = processes
                .iter()
                .filter(|name| {
                    let wants_ready = view
                        .desired
                        .processes
                        .iter()
                        .any(|spec| &spec.name == *name && spec.readiness.is_some());
                    view.observed.processes.get(*name).is_none_or(|seen| {
                        seen.state != compute_core::ProcessState::Running
                            || (wants_ready && seen.status() != "ready")
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            let mut unreachable = vec![];
            for endpoint in view
                .endpoints
                .iter()
                .filter(|endpoint| processes.contains(&endpoint.process))
            {
                if !reachable(endpoint.url.as_deref(), endpoint.port).await {
                    unreachable.push(format!("{}:{}", endpoint.process, endpoint.port));
                }
            }
            let restarted_at = step(&value.steps, "Restart applications")
                .and_then(|index| value.steps[index].at)
                .unwrap_or(value.created_at);
            let waited = (Utc::now() - restarted_at).to_std().unwrap_or_default();
            if down.is_empty() && unreachable.is_empty() {
                set(
                    &mut value.steps,
                    "Health check",
                    StepStatus::Succeeded,
                    Some(if view.endpoints.is_empty() {
                        "running".into()
                    } else {
                        "running, endpoints answering".into()
                    }),
                    None,
                );
                value.status = RolloutStatus::Active;
                value.completed_at = Some(Utc::now());
                self.finish_rollout(&stored, value).await?;
                return Ok(false);
            }
            if waited > HEALTH_DEADLINE {
                let mut problems = vec![];
                if !down.is_empty() {
                    problems.push(format!("not running or not ready: {}", down.join(", ")));
                }
                if !unreachable.is_empty() {
                    problems.push(format!("not answering: {}", unreachable.join(", ")));
                }
                let value = fail(value, "Health check", problems.join("; "), None);
                self.finish_rollout(&stored, value).await?;
                return Ok(false);
            }
            set(
                &mut value.steps,
                "Health check",
                StepStatus::Running,
                Some("waiting for the applications to answer".into()),
                None,
            );
        }
        if value != stored.value {
            self.apply(Change::new().with(|batch| batch.replace(&stored, &value)))
                .await?;
        }
        Ok(true)
    }

    /// The receipt the computer's target issued for one of its jobs.
    pub(crate) async fn job_receipt_id(&self, view: &ComputerView, job_id: &str) -> Option<String> {
        let client = self.target_client(view.target.as_deref()?).ok()?;
        let receipt = tokio::time::timeout(
            self.config.computer_liveness_timeout,
            client.job_receipt(job_id),
        )
        .await
        .ok()?
        .ok()?;
        Some(receipt.receipt.receipt_hash.0)
    }

    async fn finish_rollout(
        &self,
        stored: &Stored<RolloutRecord>,
        value: RolloutRecord,
    ) -> Result<(), EnvironmentError> {
        let mut change = Change::new().with(|batch| batch.replace(stored, &value));
        if value.status == RolloutStatus::Active {
            // What was current is superseded, and stays in the history.
            let earlier = self
                .rollouts_where(
                    Query::all(Collection::Rollout)
                        .eq("environment_id", value.environment_id.clone()),
                )
                .await?;
            for rollout in earlier.into_iter().filter(|rollout| {
                rollout.project == value.project
                    && rollout.rollout_id != value.rollout_id
                    && rollout.status == RolloutStatus::Active
            }) {
                if let Some(old) = self
                    .control()
                    .get::<RolloutRecord>(&rollout.rollout_id)
                    .await?
                {
                    let mut superseded = old.value.clone();
                    superseded.status = RolloutStatus::Superseded;
                    change = change.with(|batch| batch.replace(&old, &superseded));
                }
            }
        }
        let (kind, message) = match value.status {
            RolloutStatus::Active => (
                events::ROLLOUT_ACTIVE,
                format!(
                    "{} {} is running in {}",
                    value.project, value.version, value.environment
                ),
            ),
            _ => (
                events::ROLLOUT_FAILED,
                format!(
                    "{} {} did not become real in {}: {}",
                    value.project,
                    value.version,
                    value.environment,
                    value.failure.clone().unwrap_or_default()
                ),
            ),
        };
        let change = self.event(
            change,
            kind,
            Scope::environment(&value.environment),
            message,
            json!({
                "environment_id": value.environment_id,
                "rollout_id": value.rollout_id,
                "kind": value.kind,
                "project": value.project,
                "version": value.version,
                "failure": value.failure,
            }),
        );
        self.apply(change).await
    }

    // ---- Drivers -------------------------------------------------------------

    fn spawn_operation(self: &Arc<Self>, id: String) {
        if !self
            .operation_drivers
            .lock()
            .expect("drivers")
            .insert(id.clone())
        {
            return;
        }
        let daemon = self.clone();
        tokio::spawn(async move {
            loop {
                if daemon.is_shutting_down() {
                    break;
                }
                let more = if id.starts_with("ver_") {
                    Box::pin(daemon.publish_step(&id)).await
                } else {
                    Box::pin(daemon.rollout_step(&id)).await
                };
                match more {
                    Ok(true) => tokio::time::sleep(POLL).await,
                    Ok(false) => break,
                    Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
                }
            }
            daemon
                .operation_drivers
                .lock()
                .expect("drivers")
                .remove(&id);
        });
    }

    /// Resume the operations in flight: after a restart, every publish and
    /// rollout recorded as in progress is driven again. An indexed equality
    /// on their status, now and then.
    pub(crate) async fn drive_operations(self: &Arc<Self>) {
        {
            let mut polled = self.operations_polled.lock().expect("operations");
            if polled.is_some_and(|at| at.elapsed() < Duration::from_secs(2)) {
                return;
            }
            *polled = Some(std::time::Instant::now());
        }
        let control = self.control();
        let publishing = control
            .query::<VersionRecord>(
                Query::all(Collection::Version)
                    .eq("status", "publishing")
                    .limit(LIST_LIMIT),
            )
            .await;
        let applying = control
            .query::<RolloutRecord>(
                Query::all(Collection::Rollout)
                    .eq("status", "applying")
                    .limit(LIST_LIMIT),
            )
            .await;
        for id in publishing
            .into_iter()
            .flatten()
            .map(|stored| stored.id)
            .chain(applying.into_iter().flatten().map(|stored| stored.id))
        {
            self.spawn_operation(id);
        }
    }

    // ---- Software --------------------------------------------------------------

    /// Every project, where it runs, and at which version.
    pub async fn software(self: &Arc<Self>) -> Result<Vec<SoftwareSummary>, EnvironmentError> {
        self.refresh_for_read().await?;
        let records = {
            let inner = self.inner.lock().await;
            inner
                .desired
                .environments
                .values()
                .filter(|record| record.value.computer.is_some())
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut projects: BTreeMap<String, Vec<SoftwarePlacement>> = BTreeMap::new();
        for record in records {
            let Some(view) = self.computer_view_of(&record).await else {
                continue;
            };
            if view.status.is_terminal() {
                continue;
            }
            let contents = &view.desired;
            if contents.projects.is_empty() {
                continue;
            }
            let rollouts = self
                .rollouts_where(
                    Query::all(Collection::Rollout).eq("environment_id", record.id.clone()),
                )
                .await?;
            for project in &contents.projects {
                let repository = contents
                    .repositories
                    .iter()
                    .find(|repository| repository.name == project.repository);
                let rollout = rollouts.iter().find(|rollout| {
                    rollout.project == project.name && rollout.status != RolloutStatus::Superseded
                });
                let processes = contents
                    .processes
                    .iter()
                    .filter(|process| {
                        process.repository.as_deref() == Some(project.repository.as_str())
                    })
                    .map(|process| {
                        (
                            process.name.clone(),
                            view.observed
                                .processes
                                .get(&process.name)
                                .map_or("pending", |seen| seen.state.as_str())
                                .to_owned(),
                        )
                    })
                    .collect();
                projects
                    .entry(project.name.clone())
                    .or_default()
                    .push(SoftwarePlacement {
                        environment: record.value.name.clone(),
                        environment_id: record.id.clone(),
                        computer: view.status,
                        target: view.target.clone(),
                        revision: repository
                            .map(|repository| repository.revision.clone())
                            .unwrap_or_default(),
                        commit: view
                            .observed
                            .repositories
                            .get(&project.repository)
                            .and_then(|seen| seen.commit.clone()),
                        version: rollout.map(|rollout| rollout.version.clone()),
                        rollout: rollout.map(|rollout| rollout.status),
                        converged: view.converged,
                        processes,
                    });
            }
        }
        let mut summaries = vec![];
        for (project, environments) in projects {
            let latest = self.latest_version(&project).await?;
            summaries.push(SoftwareSummary {
                project,
                environments,
                latest_version: latest.as_ref().map(|latest| latest.version.clone()),
                latest_status: latest.map(|latest| latest.status),
            });
        }
        Ok(summaries)
    }

    /// A project: where it runs, its versions, and its rollouts.
    pub async fn software_view(
        self: &Arc<Self>,
        project: &str,
    ) -> Result<SoftwareView, EnvironmentError> {
        let summary = self
            .software()
            .await?
            .into_iter()
            .find(|summary| summary.project == project);
        let versions = self.versions(project).await?;
        let rollouts = self
            .rollouts_where(Query::all(Collection::Rollout).eq("project", project.to_owned()))
            .await?;
        if summary.is_none() && versions.is_empty() {
            return Err(EnvironmentError::NotFound(format!("project {project}")));
        }
        let next_version = next_version(versions.first().map(|version| version.version.as_str()));
        Ok(SoftwareView {
            summary: summary.unwrap_or(SoftwareSummary {
                project: project.to_owned(),
                environments: vec![],
                latest_version: versions.first().map(|version| version.version.clone()),
                latest_status: versions.first().map(|version| version.status),
            }),
            versions,
            rollouts,
            next_version,
        })
    }
}

/// Bring a version's assembly into an environment's contents: the
/// repository at the version's commit, the project as the version built it,
/// and whatever runs from it that the environment does not have yet (what
/// it has keeps its own ports and settings).
fn apply_assembly(
    contents: &mut compute_core::EnvironmentContents,
    assembly: &ProjectAssembly,
    taken: &BTreeSet<u16>,
) {
    if let Some(repository) = &assembly.repository {
        match contents
            .repositories
            .iter_mut()
            .find(|existing| existing.name == repository.name)
        {
            Some(existing) => *existing = repository.clone(),
            None => contents.repositories.push(repository.clone()),
        }
    }
    if let Some(project) = &assembly.project {
        match contents
            .projects
            .iter_mut()
            .find(|existing| existing.name == project.name)
        {
            Some(existing) => *existing = project.clone(),
            None => contents.projects.push(project.clone()),
        }
    }
    for package in &assembly.packages {
        if !contents
            .packages
            .iter()
            .any(|existing| existing.name == package.name)
        {
            contents.packages.push(package.clone());
        }
    }
    for process in &assembly.processes {
        if !contents
            .processes
            .iter()
            .any(|existing| existing.name == process.name)
        {
            // Computers may share a host: a new process gets a port no
            // environment uses.
            let mut process = process.clone();
            if let Some(mut port) = process.port {
                while taken.contains(&port)
                    || contents
                        .processes
                        .iter()
                        .any(|other| other.port == Some(port))
                {
                    port = port.checked_add(1).unwrap_or(1024);
                }
                process.port = Some(port);
            }
            contents.processes.push(process);
        }
    }
}

/// Whether an endpoint answers a TCP connection from the control plane.
async fn reachable(url: Option<&str>, port: u16) -> bool {
    let Some(url) = url else {
        return true;
    };
    let host = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let address = if host.is_empty() {
        format!("127.0.0.1:{port}")
    } else {
        host
    };
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(address)
        )
        .await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_project_is_proposed_from_its_package_json() {
        let output = "COMMIT abc\nBRANCH main\nFILE package.json\nFILE package-lock.json\nFILE .env.example\n\
                      PACKAGE_JSON_BEGIN\n{\"scripts\":{\"build\":\"tsc\",\"test\":\"vitest\",\"lint\":\"eslint .\",\"start\":\"node dist/index.js\"}}\nPACKAGE_JSON_END\n\
                      ENV DATABASE_URL=postgres://localhost/app\nENV PORT=3000\n";
        let proposal = propose(
            output,
            "https://example.invalid/acme/web-app.git",
            None,
            None,
            &BTreeSet::from([3000]),
        );
        assert_eq!(proposal.name, "web-app");
        assert_eq!(proposal.runtime.as_deref(), Some("node"));
        let project = proposal.assembly.project.unwrap();
        assert_eq!(project.build, shell("npm run build"));
        assert_eq!(project.test, shell("npm test"));
        assert_eq!(project.checks, vec!["lint".to_owned()]);
        assert_eq!(proposal.assembly.packages[0].install, shell("npm ci"));
        let web = &proposal.assembly.processes[0];
        assert_eq!(web.command, shell("npm start"));
        assert_eq!(web.port, Some(3001), "a port nobody else uses");
        assert_eq!(proposal.assembly.repository.unwrap().revision, "main");
        assert_eq!(proposal.services[0].name, "database");
        assert!(proposal.config.contains_key("DATABASE_URL"));
    }

    #[test]
    fn a_procfile_and_makefile_are_honoured_and_nothing_is_invented() {
        let output = "FILE Makefile\nFILE Procfile\nMAKE build\nMAKE test\nMAKE lint\n\
                      PROCFILE web: ./serve --port $PORT\nPROCFILE worker: ./work\n";
        let proposal = propose(
            output,
            "/srv/tool",
            Some("v2"),
            Some("tool"),
            &BTreeSet::new(),
        );
        let project = proposal.assembly.project.unwrap();
        assert_eq!(project.build, shell("make build"));
        assert_eq!(project.checks, vec!["lint".to_owned()]);
        assert_eq!(proposal.assembly.processes.len(), 2);
        assert_eq!(proposal.assembly.processes[0].name, "tool");
        assert!(proposal.assembly.processes[0].port.is_some());
        assert_eq!(proposal.assembly.processes[1].name, "tool-worker");
        assert_eq!(proposal.assembly.processes[1].port, None);
        assert_eq!(proposal.assembly.repository.unwrap().revision, "v2");
        let bare = propose(
            "FILE README.md\n",
            "https://example.invalid/x.git",
            None,
            None,
            &BTreeSet::new(),
        );
        assert!(bare.assembly.processes.is_empty());
        assert!(
            bare.notes
                .iter()
                .any(|note| note.contains("No start command"))
        );
    }

    #[test]
    fn versions_count_up() {
        assert_eq!(next_version(None), "0.1.0");
        assert_eq!(next_version(Some("1.8.3")), "1.8.4");
        assert_eq!(next_version(Some("v2.0.9")), "v2.0.10");
        assert_eq!(next_version(Some("2024-rc")), "2024-rc.1");
        assert!(valid_version("1.8.4").is_ok());
        assert!(valid_version("1.8 4").is_err());
    }

    #[test]
    fn a_version_brings_what_the_environment_lacks_and_keeps_what_it_has() {
        let repository = RepositorySpec {
            name: "web".into(),
            url: "u".into(),
            revision: "abc".into(),
            sync: 0,
        };
        let process = ProcessSpec {
            name: "web".into(),
            kind: ProcessKind::Application,
            command: vec!["./serve".into()],
            repository: Some("web".into()),
            env: BTreeMap::new(),
            desired: ProcessDesired::Running,
            port: Some(3000),
            restart: 0,
            readiness: None,
            restart_policy: Default::default(),
            max_restarts: compute_core::DEFAULT_MAX_RESTARTS,
        };
        let assembly = ProjectAssembly {
            repository: Some(repository.clone()),
            project: None,
            packages: vec![],
            processes: vec![process.clone()],
        };
        let mut contents = compute_core::EnvironmentContents::default();
        apply_assembly(&mut contents, &assembly, &BTreeSet::from([3000]));
        assert_eq!(
            contents.processes[0].port,
            Some(3001),
            "a port another computer uses is not reused"
        );
        contents.processes[0].port = Some(3000);
        assert_eq!(contents.repositories[0].revision, "abc");
        contents.processes[0].port = Some(4000);
        let mut next = assembly.clone();
        next.repository.as_mut().unwrap().revision = "def".into();
        apply_assembly(&mut contents, &next, &BTreeSet::new());
        assert_eq!(contents.repositories[0].revision, "def");
        assert_eq!(
            contents.processes[0].port,
            Some(4000),
            "the environment's own port stays"
        );
    }
}
