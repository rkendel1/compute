//! Working in an environment: its projects' commands, releases, and work
//! sessions.
//!
//! ```text
//! build · test · project command · exec   ──▶ a durable job in the computer
//! release (a revision)                     ──▶ a change to desired state,
//!                                               reconciled in place
//! work session                             ──▶ a way in: attached to an
//!                                               environment, or owning a
//!                                               temporary one
//! ```
//!
//! The daemon coordinates and records; the environment's computer does the
//! work. Nothing here runs on the daemon's own node.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use compute_core::{ComputerLifecycle, ComputerStatus};
use compute_state::{
    Collection, EnvironmentRecord, Query, Stored, WorkSessionKind, WorkSessionRecord,
    WorkSessionStatus, events, ids,
};
use serde_json::json;

use super::computers::{INSTALL_PACKAGE, script};
use super::{Change, ComputerExec, Daemon, Scope};
use crate::EnvironmentError;
use crate::model::*;
use crate::status::*;

/// Sessions listed at most.
const SESSION_LIMIT: usize = 200;

impl Daemon {
    /// Run one of a project's commands (`build`, `test`, or a named one) in
    /// the environment's computer, in the project's checkout, as a durable
    /// job.
    pub async fn project_command(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        request: ProjectCommandRequest,
    ) -> Result<ComputerExec, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        let contents = record.value.contents.clone().unwrap_or_default();
        let project = contents
            .projects
            .iter()
            .find(|project| project.name == request.project)
            .ok_or_else(|| {
                EnvironmentError::NotFound(format!(
                    "project {} in environment {environment}",
                    request.project
                ))
            })?;
        let argv = project.command(&request.command).ok_or_else(|| {
            EnvironmentError::NotFound(format!(
                "command {} of project {}",
                request.command, project.name
            ))
        })?;
        let mut arguments = vec![project.repository.clone()];
        arguments.extend(argv.iter().cloned());
        let mut command = script(INSTALL_PACKAGE, arguments);
        command.env = request.env.clone();
        command.timeout = request.timeout.map(Duration::from_millis);
        command.validate()?;
        let summary = format!(
            "{operator} ran {} {} in {}",
            project.name, request.command, record.value.name
        );
        self.exec_in(
            &record,
            command,
            events::ENVIRONMENT_COMMAND,
            summary,
            json!({
                "project": project.name,
                "command": request.command,
                "repository": project.repository,
                "operator": operator,
            }),
        )
        .await
    }

    /// Release a revision of a project: its repository moves to the
    /// revision, and the computer checks it out, builds it, and restarts
    /// what runs from it, in place. A deployment is this change.
    pub async fn release_project(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        request: ReleaseRequest,
    ) -> Result<ComputerView, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        let contents = record.value.contents.clone().unwrap_or_default();
        let project = contents
            .projects
            .iter()
            .find(|project| project.name == request.project)
            .cloned()
            .ok_or_else(|| {
                EnvironmentError::NotFound(format!(
                    "project {} in environment {environment}",
                    request.project
                ))
            })?;
        let from = contents
            .repositories
            .iter()
            .find(|repository| repository.name == project.repository)
            .map(|repository| repository.revision.clone());
        let expected = request.expected_generation;
        let revision = request.revision.clone();
        self.change_environment(
            environment,
            operator,
            format!(
                "release {} {} → {}",
                project.name,
                from.as_deref().unwrap_or("?"),
                request.revision
            ),
            Some((
                events::ENVIRONMENT_RELEASE,
                json!({
                    "environment_id": record.id,
                    "project": project.name,
                    "repository": project.repository,
                    "from": from,
                    "to": request.revision,
                    "operator": operator,
                }),
            )),
            move |value| {
                let contents = value.contents.get_or_insert_with(Default::default);
                if let Some(expected) = expected
                    && expected != contents.generation
                {
                    return Err(EnvironmentError::Conflict(format!(
                        "the environment changed since you loaded it (generation {expected}, now {})",
                        contents.generation
                    )));
                }
                let repository = contents
                    .repositories
                    .iter_mut()
                    .find(|repository| repository.name == project.repository)
                    .ok_or_else(|| {
                        EnvironmentError::NotFound(format!("repository {}", project.repository))
                    })?;
                repository.revision = revision.clone();
                Ok(())
            },
        )
        .await
    }

    /// Add or change a project.
    pub async fn upsert_project(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        project: compute_core::ProjectSpec,
    ) -> Result<ComputerView, EnvironmentError> {
        let description = format!("project {}", project.name);
        self.change_contents(environment, operator, description, move |contents| {
            if let Some(existing) = contents
                .projects
                .iter_mut()
                .find(|existing| existing.name == project.name)
            {
                *existing = project.clone();
            } else {
                contents.projects.push(project.clone());
            }
            Ok(())
        })
        .await
    }

    /// Replace the environment's configuration: every process, build, and
    /// command sees it, and what depends on it restarts in place.
    pub async fn set_config(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        config: std::collections::BTreeMap<String, String>,
    ) -> Result<ComputerView, EnvironmentError> {
        validate_env("environment", &config)?;
        self.change_environment(
            environment,
            operator,
            "configuration replaced".into(),
            None,
            move |value| {
                value.config = config.clone();
                Ok(())
            },
        )
        .await
    }

    // ---- Work sessions ----------------------------------------------------

    /// Open a work session: enter an environment its owner already has, or
    /// have a temporary environment made for the session.
    pub async fn open_session(
        self: &Arc<Self>,
        operator: &str,
        request: OpenSessionRequest,
    ) -> Result<WorkSessionView, EnvironmentError> {
        let nonce = crate::auth::hex(&crate::auth::random::<12>()?);
        let (record, kind) = match &request.environment {
            Some(environment) => {
                if request.computer.is_some() || request.contents.is_some() {
                    return Err(EnvironmentError::Invalid(
                        "a session entering an environment takes no computer or contents; \
                         change the environment instead"
                            .into(),
                    ));
                }
                let record = self.owned_environment(environment, operator).await?;
                self.require_live(&record).await?;
                (record, WorkSessionKind::Attached)
            }
            None => {
                let mut computer = request.computer.clone().unwrap_or(ComputerRequest {
                    lifecycle: ComputerLifecycle::Ephemeral,
                    requirements: Default::default(),
                    target: None,
                    ttl_seconds: None,
                });
                if computer.lifecycle != ComputerLifecycle::Ephemeral {
                    return Err(EnvironmentError::Invalid(
                        "a session's own environment is temporary; create an environment to \
                         keep one, then open a session in it"
                            .into(),
                    ));
                }
                computer.ttl_seconds = computer.ttl_seconds.or(Some(super::computers::DEFAULT_TTL));
                let name = format!("work-{}", &nonce[..10]);
                self.create_computer_environment(
                    ComputerEnvironmentDefinition {
                        name: name.clone(),
                        desired_state: DesiredState::Running,
                        env: request.env.clone(),
                        policy: None,
                        computer,
                        contents: request.contents.clone().unwrap_or_default(),
                    },
                    operator,
                )
                .await?;
                (
                    self.owned_environment(&name, operator).await?,
                    WorkSessionKind::Ephemeral,
                )
            }
        };
        let session_id = ids::work_session(&record.id, operator, &nonce);
        let value = WorkSessionRecord {
            session_id: session_id.clone(),
            environment_id: record.id.clone(),
            environment: record.value.name.clone(),
            owner: operator.to_owned(),
            kind,
            status: WorkSessionStatus::Open,
            opened_at: Utc::now(),
            closed_at: None,
            expires_at: (kind == WorkSessionKind::Ephemeral)
                .then(|| {
                    record
                        .value
                        .computer
                        .as_ref()
                        .and_then(|spec| spec.expires_at)
                })
                .flatten(),
            close_reason: None,
        };
        let change = Change::new().with(|batch| batch.create(&session_id, &value));
        let change = self.event(
            change,
            events::WORK_SESSION_OPENED,
            Scope::environment(&record.value.name),
            format!(
                "{operator} opened a {} work session in {}",
                kind.as_str(),
                record.value.name
            ),
            json!({
                "environment_id": record.id,
                "session_id": session_id,
                "kind": kind,
                "operator": operator,
            }),
        );
        if let Err(error) = self.apply(change).await {
            // A temporary environment nobody holds is not left behind.
            if kind == WorkSessionKind::Ephemeral {
                let _ = self.destroy_computer(&record.value.name, operator).await;
            }
            return Err(error);
        }
        self.session_view(value, true).await
    }

    /// Close a work session. Only a session that made its own environment
    /// takes the environment's computer with it; an environment someone
    /// entered is left exactly as it is.
    pub async fn close_session(
        self: &Arc<Self>,
        operator: &str,
        session_id: &str,
    ) -> Result<WorkSessionView, EnvironmentError> {
        let stored = self.owned_session(operator, session_id).await?;
        let mut value = stored.value.clone();
        if value.status == WorkSessionStatus::Open {
            value.status = WorkSessionStatus::Closed;
            value.closed_at = Some(Utc::now());
            value.close_reason = Some(format!("closed by {operator}"));
            let change = Change::new().with(|batch| batch.replace(&stored, &value));
            let change = self.event(
                change,
                events::WORK_SESSION_CLOSED,
                Scope::environment(&value.environment),
                format!(
                    "{operator} closed a work session in {}{}",
                    value.environment,
                    if value.kind == WorkSessionKind::Ephemeral {
                        "; its environment's computer is destroyed"
                    } else {
                        "; the environment keeps running"
                    }
                ),
                json!({
                    "environment_id": value.environment_id,
                    "session_id": value.session_id,
                    "kind": value.kind,
                    "operator": operator,
                }),
            );
            self.apply(change).await?;
            if value.kind == WorkSessionKind::Ephemeral {
                match self.destroy_computer(&value.environment, operator).await {
                    Ok(_) | Err(EnvironmentError::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        self.session_view(value, false).await
    }

    /// One of the operator's work sessions.
    pub async fn work_session(
        self: &Arc<Self>,
        operator: &str,
        session_id: &str,
    ) -> Result<WorkSessionView, EnvironmentError> {
        let stored = self.owned_session(operator, session_id).await?;
        self.session_view(stored.value, false).await
    }

    /// The operator's work sessions, newest first; in one environment when
    /// named. Read by an indexed equality, ordered and limited by FeltDB.
    pub async fn work_sessions(
        self: &Arc<Self>,
        operator: &str,
        environment: Option<&str>,
    ) -> Result<Vec<WorkSessionView>, EnvironmentError> {
        let query = match environment {
            Some(environment) => {
                let record = self.owned_environment(environment, operator).await?;
                Query::all(Collection::WorkSession).eq("environment_id", record.id.clone())
            }
            None => Query::all(Collection::WorkSession).eq("owner", operator.to_owned()),
        };
        let sessions = self
            .control()
            .query::<WorkSessionRecord>(query.descending("opened_at").limit(SESSION_LIMIT))
            .await?;
        let mut views = vec![];
        for session in sessions {
            if session.value.owner == operator {
                views.push(self.session_view(session.value, false).await?);
            }
        }
        Ok(views)
    }

    async fn owned_session(
        &self,
        operator: &str,
        session_id: &str,
    ) -> Result<Stored<WorkSessionRecord>, EnvironmentError> {
        let stored = self
            .control()
            .get::<WorkSessionRecord>(session_id)
            .await?
            .ok_or_else(|| EnvironmentError::NotFound(format!("work session {session_id}")))?;
        if stored.value.owner != operator {
            return Err(EnvironmentError::Forbidden(format!(
                "work session {session_id} belongs to another principal"
            )));
        }
        Ok(stored)
    }

    /// A session as it stands. A session whose environment's computer has
    /// ended is reported closed with the reason; the record itself is
    /// closed by the controller.
    async fn session_view(
        self: &Arc<Self>,
        value: WorkSessionRecord,
        connect: bool,
    ) -> Result<WorkSessionView, EnvironmentError> {
        let computer = self.stored_computer(&value.environment_id).await;
        let ended = computer
            .as_ref()
            .is_some_and(|computer| computer.value.status.is_terminal());
        let mut view = WorkSessionView {
            session_id: value.session_id.clone(),
            environment: value.environment.clone(),
            environment_id: value.environment_id.clone(),
            owner: value.owner.clone(),
            kind: value.kind,
            status: value.status,
            opened_at: value.opened_at,
            closed_at: value.closed_at,
            expires_at: value.expires_at,
            close_reason: value.close_reason.clone(),
            connection: None,
        };
        if ended && view.status == WorkSessionStatus::Open {
            view.status = WorkSessionStatus::Closed;
            view.close_reason = computer
                .map(|computer| format!("the environment's computer is {}", computer.value.status));
        }
        let running = self
            .stored_computer(&value.environment_id)
            .await
            .is_some_and(|computer| computer.value.status == ComputerStatus::Running);
        if connect && running && view.status == WorkSessionStatus::Open {
            view.connection = self
                .computer_connect(&value.environment, &value.owner)
                .await
                .ok();
        }
        Ok(view)
    }

    /// Close the open sessions of an environment whose computer ended:
    /// expired, destroyed, or failed. Their records stay.
    pub(crate) async fn end_sessions_of(
        &self,
        record: &Stored<EnvironmentRecord>,
        status: ComputerStatus,
    ) -> Result<(), EnvironmentError> {
        let open = self
            .control()
            .query::<WorkSessionRecord>(
                Query::all(Collection::WorkSession)
                    .eq("environment_id", record.id.clone())
                    .descending("opened_at")
                    .limit(SESSION_LIMIT),
            )
            .await?;
        for stored in open
            .into_iter()
            .filter(|stored| stored.value.status == WorkSessionStatus::Open)
        {
            let mut value = stored.value.clone();
            value.status = WorkSessionStatus::Closed;
            value.closed_at = Some(Utc::now());
            value.close_reason = Some(format!("the environment's computer is {status}"));
            let change = Change::new().with(|batch| batch.replace(&stored, &value));
            let change = self.event(
                change,
                events::WORK_SESSION_CLOSED,
                Scope::environment(&record.value.name),
                format!(
                    "A work session in {} ended: its computer is {status}",
                    record.value.name
                ),
                json!({
                    "environment_id": record.id,
                    "session_id": value.session_id,
                    "kind": value.kind,
                }),
            );
            self.apply(change).await?;
        }
        Ok(())
    }
}
