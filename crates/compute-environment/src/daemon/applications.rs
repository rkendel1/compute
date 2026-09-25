//! Applications: the product view of a project in this node's
//! `applications` environment.
//!
//! ```text
//! application ── deployments (v1, v2, …) ── executions ── receipts
//!      └── endpoint (stable across versions)
//! ```
//!
//! An application is a project with one service, `app`, released through
//! the ordinary release lifecycle. Nothing here is a second release path or
//! a second store: every operation is an existing daemon operation, and
//! every view is derived from control state.

use std::net::IpAddr;
use std::sync::Arc;

use compute_core::{ApplicationIdentity, WorkloadBundle};

use super::Daemon;
use crate::EnvironmentError;
use crate::model::*;
use crate::status::*;

/// The environment applications are released to on every node.
pub const APPLICATIONS_ENVIRONMENT: &str = "applications";
/// An application's one service.
pub const APPLICATION_WORKLOAD: &str = "app";
const HISTORY: usize = 50;

impl Daemon {
    /// Every application on this node.
    pub async fn applications(&self) -> Result<Vec<ApplicationView>, EnvironmentError> {
        let environment = match self.environment(APPLICATIONS_ENVIRONMENT).await {
            Ok(environment) => environment,
            Err(EnvironmentError::NotFound(_)) => return Ok(vec![]),
            Err(error) => return Err(error),
        };
        let mut applications = vec![];
        for project in environment.projects {
            applications.push(self.application(&project.name).await?);
        }
        Ok(applications)
    }

    /// One application: its status, stable endpoint, and versions.
    pub async fn application(&self, name: &str) -> Result<ApplicationView, EnvironmentError> {
        let identity = ApplicationIdentity::new(name, None)?;
        let project = match self.project(APPLICATIONS_ENVIRONMENT, name).await {
            Ok(project) => project,
            Err(EnvironmentError::NotFound(_)) => {
                return Err(EnvironmentError::NotFound(format!(
                    "application {name} is not deployed on this node"
                )));
            }
            Err(error) => return Err(error),
        };
        let deployments = self.application_deployments(name, Some(&project)).await?;
        let active = deployments
            .iter()
            .find(|deployment| deployment.active)
            .cloned();
        let deploying = deployments
            .iter()
            .find(|deployment| deployment.state == ApplicationDeploymentState::Deploying)
            .cloned();
        let status = if deploying.is_some() && active.is_none() {
            "deploying".to_owned()
        } else {
            project.actual_state.as_str().to_owned()
        };
        let endpoint = active
            .as_ref()
            .and_then(|deployment| deployment.endpoint.clone())
            .or_else(|| {
                deployments
                    .iter()
                    .find_map(|deployment| deployment.endpoint.clone())
            });
        Ok(ApplicationView {
            application: identity,
            status,
            node: self.node_url(),
            endpoint,
            active,
            deploying,
            deployments,
        })
    }

    /// An application's versions, newest first, in product terms.
    pub async fn application_deployments(
        &self,
        name: &str,
        project: Option<&ProjectView>,
    ) -> Result<Vec<ApplicationDeploymentView>, EnvironmentError> {
        let project = match project {
            Some(project) => project.clone(),
            None => self.project(APPLICATIONS_ENVIRONMENT, name).await?,
        };
        let records = self
            .deployments(
                Some(APPLICATIONS_ENVIRONMENT.into()),
                Some(name.into()),
                Some(HISTORY),
            )
            .await?;
        let current = project
            .deployment
            .as_ref()
            .map(|deployment| deployment.deployment_id.clone());
        let stopped = project.desired_state == DesiredState::Stopped
            || project.actual_state == ActualState::Stopped;
        let mut views = vec![];
        for (index, deployment) in records.iter().enumerate() {
            let record = &deployment.record;
            let active = current.as_deref() == Some(deployment.deployment_id.as_str());
            let state = match record.status {
                DeploymentStatus::Failed => ApplicationDeploymentState::Failed,
                DeploymentStatus::RolledBack => ApplicationDeploymentState::RolledBack,
                _ if active && stopped => ApplicationDeploymentState::Stopped,
                _ if active => ApplicationDeploymentState::Active,
                status if !status.is_terminal() && !status.serves() => {
                    ApplicationDeploymentState::Deploying
                }
                _ => ApplicationDeploymentState::Superseded,
            };
            // Newest first: the version released just before this one is
            // the next record, and earlier ones follow.
            let earlier = &records[index + 1..];
            let rollback_of = earlier
                .first()
                .filter(|previous| previous.record.revision_id != record.revision_id)
                .and_then(|_| {
                    earlier
                        .iter()
                        .find(|older| {
                            older.record.revision_id == record.revision_id
                                && older.record.status.serves()
                        })
                        .map(|older| older.record.version)
                });
            let workload = record
                .workloads
                .iter()
                .find(|workload| workload.name == APPLICATION_WORKLOAD)
                .or_else(|| record.workloads.first());
            views.push(ApplicationDeploymentView {
                application: name.to_owned(),
                version: record.version,
                deployment_id: deployment.deployment_id.clone(),
                state,
                active,
                rollback_of,
                endpoint: workload
                    .and_then(|workload| workload.endpoints.first())
                    .map(|binding| self.application_url(binding.host)),
                runtime: workload.map(|workload| workload.runtime.clone()),
                runtime_version: workload.and_then(|workload| {
                    workload
                        .resolved_runtime_version
                        .clone()
                        .or_else(|| workload.runtime_version.clone())
                }),
                placement: workload.and_then(|workload| workload.pool_placement.clone()),
                artifact: workload.and_then(|workload| workload.application_artifact.clone()),
                failure: record.failure.clone().or(record.rollback_reason.clone()),
                receipt: record.receipt.clone(),
                execution_receipts: record.receipt_ids.clone(),
                created_at: record.created_at,
                completed_at: record.completed_at,
            });
        }
        Ok(views)
    }

    /// One version, by version number (`3`, `v3`) or deployment ID.
    pub async fn application_deployment(
        &self,
        name: &str,
        target: &str,
    ) -> Result<ApplicationDeploymentView, EnvironmentError> {
        let deployments = self.application_deployments(name, None).await?;
        find_version(&deployments, target)
            .cloned()
            .ok_or_else(|| EnvironmentError::NotFound(format!("{name} {target}")))
    }

    /// Release a new version of an application on this node. The first
    /// deployment creates the application; every one is the ordinary
    /// release: revision, admission, start, readiness, traffic switch.
    pub async fn deploy_application(
        self: &Arc<Self>,
        name: &str,
        request: ApplicationDeployRequest,
    ) -> Result<ApplicationDeploymentView, EnvironmentError> {
        if !self.config.execution.deployments {
            return Err(EnvironmentError::Invalid(
                "this node does not host application deployments".into(),
            ));
        }
        let resolved = resolve_application(name, &request).await?;
        let bundle = WorkloadBundle::from_bytes(&resolved.bundle)?;
        let bundle_id = bundle.bundle_id()?;
        self.ensure_applications_environment().await?;
        let definition = RevisionDefinition {
            revision: format!("artifact-{}", bundle_id.trim_start_matches("sha256:")),
            source: request.source.clone(),
            workloads: vec![WorkloadDefinition {
                name: APPLICATION_WORKLOAD.into(),
                kind: WorkloadKind::Service,
                bundle: resolved.bundle,
                ports: vec![PortSpec {
                    name: "http".into(),
                    port: resolved.port,
                }],
                restart: RestartPolicy::OnFailure,
                desired_state: DesiredState::Running,
                readiness: Some(Readiness {
                    check: ReadinessCheck::Http,
                    port: Some("http".into()),
                    path: Some("/".into()),
                    task: None,
                    timeout_ms: 60_000,
                    interval_ms: 250,
                }),
            }],
        };
        let daemon = self.clone();
        let project = name.to_owned();
        let revision =
            on_own_task(async move { daemon.register_revision(&project, definition).await })
                .await?;
        self.release(DeployRequest {
            project: name.into(),
            environment: APPLICATIONS_ENVIRONMENT.into(),
            revision: Some(revision.revision_id),
            config: request.env,
            desired_state: Some(DesiredState::Running),
            placement: request.placement,
            artifact: resolved.evidence,
            required_config: resolved.required_env,
        })
        .await
    }

    /// Release through the ordinary lifecycle, and describe the result as
    /// a version of the application.
    async fn release(
        self: &Arc<Self>,
        request: DeployRequest,
    ) -> Result<ApplicationDeploymentView, EnvironmentError> {
        let name = request.project.clone();
        let daemon = self.clone();
        let deployment = on_own_task(async move { daemon.deploy(request).await }).await?;
        self.application_deployment(&name, &deployment.deployment_id)
            .await
    }

    /// Deploy an earlier version's revision and configuration again, as the
    /// next version. History is never edited.
    pub async fn rollback_application(
        self: &Arc<Self>,
        name: &str,
        request: ApplicationRollbackRequest,
    ) -> Result<ApplicationDeploymentView, EnvironmentError> {
        let deployments = self
            .deployments(
                Some(APPLICATIONS_ENVIRONMENT.into()),
                Some(name.into()),
                Some(HISTORY),
            )
            .await?;
        let target = deployments
            .iter()
            .find(|deployment| {
                deployment.deployment_id == request.target
                    || parse_version(&request.target) == Some(deployment.record.version)
            })
            .ok_or_else(|| {
                EnvironmentError::NotFound(format!("{name} has no version {}", request.target))
            })?;
        if matches!(
            target.record.status,
            DeploymentStatus::Failed | DeploymentStatus::RolledBack
        ) {
            return Err(EnvironmentError::Invalid(format!(
                "{name} v{} never served; roll back to a version that did",
                target.record.version
            )));
        }
        self.release(DeployRequest {
            project: name.into(),
            environment: APPLICATIONS_ENVIRONMENT.into(),
            revision: Some(target.record.revision_id.clone()),
            config: Some(target.record.config.clone()),
            desired_state: Some(DesiredState::Running),
            placement: request.placement,
            // The same artifact as the version rolled back to.
            artifact: target
                .record
                .workloads
                .iter()
                .find(|workload| workload.name == APPLICATION_WORKLOAD)
                .and_then(|workload| workload.application_artifact.clone()),
            required_config: Default::default(),
        })
        .await
    }

    /// Stop an application. Its versions, endpoint, and evidence remain.
    pub async fn stop_application(
        self: &Arc<Self>,
        name: &str,
    ) -> Result<ApplicationView, EnvironmentError> {
        let daemon = self.clone();
        let project = name.to_owned();
        on_own_task(async move {
            daemon
                .set_project_state(
                    APPLICATIONS_ENVIRONMENT,
                    &project,
                    DesiredState::Stopped,
                    false,
                )
                .await
        })
        .await?;
        self.application(name).await
    }

    /// The application's current output.
    pub async fn application_logs(&self, name: &str) -> Result<(String, String), EnvironmentError> {
        self.logs(APPLICATIONS_ENVIRONMENT, name, APPLICATION_WORKLOAD)
            .await
    }

    async fn ensure_applications_environment(self: &Arc<Self>) -> Result<(), EnvironmentError> {
        match self.environment(APPLICATIONS_ENVIRONMENT).await {
            Ok(_) => Ok(()),
            Err(EnvironmentError::NotFound(_)) => {
                match self
                    .create_environment(EnvironmentDefinition {
                        name: APPLICATIONS_ENVIRONMENT.into(),
                        desired_state: DesiredState::Running,
                        env: Default::default(),
                        policy: None,
                        provider: None,
                    })
                    .await
                {
                    Ok(_) | Err(EnvironmentError::Conflict(_)) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// This node as a `compute.remote@1` provider: capabilities, health,
    /// runs, and jobs, on the provider its deployments use.
    pub fn remote_service(&self) -> Option<Arc<compute_provider::RemoteService>> {
        self.remote.clone()
    }

    fn node_url(&self) -> String {
        self.config
            .public_url
            .clone()
            .unwrap_or_else(|| self.instance_id.clone())
    }

    /// Where an application endpoint on `port` is reached.
    fn application_url(&self, port: u16) -> String {
        let host = self.config.application_host.clone().unwrap_or_else(|| {
            let address = self.config.network.endpoint_address;
            if !address.is_unspecified() {
                return host_literal(address);
            }
            self.config
                .public_url
                .as_deref()
                .and_then(url_host)
                .unwrap_or_else(|| "127.0.0.1".into())
        });
        format!("http://{host}:{port}")
    }
}

/// Run a daemon operation as its own task. Registration and releases are
/// deep futures; polled by the scheduler rather than nested inside a
/// request's future, they stay within a worker thread's stack.
async fn on_own_task<T: Send + 'static>(
    operation: impl std::future::Future<Output = Result<T, EnvironmentError>> + Send + 'static,
) -> Result<T, EnvironmentError> {
    tokio::spawn(operation)
        .await
        .map_err(|error| EnvironmentError::Invalid(format!("operation did not finish: {error}")))?
}

fn find_version<'a>(
    deployments: &'a [ApplicationDeploymentView],
    target: &str,
) -> Option<&'a ApplicationDeploymentView> {
    deployments.iter().find(|deployment| {
        deployment.deployment_id == target || parse_version(target) == Some(deployment.version)
    })
}

fn parse_version(target: &str) -> Option<u64> {
    target.strip_prefix('v').unwrap_or(target).parse().ok()
}

fn host_literal(address: IpAddr) -> String {
    match address {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => format!("[{address}]"),
    }
}

fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next()?;
    let host = if authority.starts_with('[') {
        authority
            .split_once(']')
            .map(|(host, _)| format!("{host}]"))?
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
            .to_owned()
    };
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_hosts_parse() {
        assert_eq!(parse_version("v3"), Some(3));
        assert_eq!(parse_version("3"), Some(3));
        assert_eq!(parse_version("dep_abc"), None);
        assert_eq!(
            url_host("http://10.0.0.20:8787").as_deref(),
            Some("10.0.0.20")
        );
        assert_eq!(url_host("https://[::1]:8787/").as_deref(), Some("[::1]"));
        assert_eq!(
            url_host("http://node.example").as_deref(),
            Some("node.example")
        );
    }
}

/// What a deploy request releases: the bundle, its port, and, from an
/// artifact, the artifact's evidence and environment contract.
struct ResolvedApplication {
    bundle: Vec<u8>,
    port: u16,
    evidence: Option<compute_state::ApplicationArtifactEvidence>,
    required_env: std::collections::BTreeSet<String>,
}

/// Resolve a deploy request to the application it releases. An artifact
/// by reference is fetched here, on the provider, and must have the digest
/// the caller pinned; a manifest must name the application being deployed.
async fn resolve_application(
    name: &str,
    request: &ApplicationDeployRequest,
) -> Result<ResolvedApplication, EnvironmentError> {
    let Some(source) = &request.artifact else {
        let port = request.port.filter(|port| *port > 0).ok_or_else(|| {
            EnvironmentError::Invalid("an application needs the port it listens on".into())
        })?;
        if request.bundle.is_empty() {
            return Err(EnvironmentError::Invalid(
                "a deployment needs an application artifact or a bundle".into(),
            ));
        }
        ApplicationIdentity::new(name, Some(port))?;
        return Ok(ResolvedApplication {
            bundle: request.bundle.clone(),
            port,
            evidence: None,
            required_env: Default::default(),
        });
    };
    if !request.bundle.is_empty() || request.port.is_some() {
        return Err(EnvironmentError::Invalid(
            "an application artifact carries its bundle and port; send one or the other".into(),
        ));
    }
    let (bytes, url) = match source {
        ApplicationArtifactSource::Inline { data } => (data.clone(), None),
        ApplicationArtifactSource::Reference(reference) => {
            let fetching = reference.clone();
            let bytes = tokio::task::spawn_blocking(move || fetching.fetch())
                .await
                .map_err(|error| EnvironmentError::Invalid(error.to_string()))??;
            (bytes, Some(reference.url.clone()))
        }
    };
    let artifact = compute_core::ApplicationArtifact::from_bytes(&bytes)?;
    let manifest = &artifact.manifest;
    if manifest.application.name != name {
        return Err(EnvironmentError::Invalid(format!(
            "the artifact is application {}, not {name}",
            manifest.application.name
        )));
    }
    Ok(ResolvedApplication {
        bundle: artifact.bundle_bytes().to_vec(),
        port: manifest
            .application
            .port
            .expect("a verified artifact has a port"),
        evidence: Some(compute_state::ApplicationArtifactEvidence {
            artifact_id: artifact.artifact_id()?,
            url,
            version: manifest.version.clone(),
            capabilities: manifest.capabilities.iter().cloned().collect(),
        }),
        // Defaults built into the artifact satisfy a requirement too.
        required_env: manifest
            .env
            .required
            .iter()
            .filter(|name| !manifest.env.defaults.contains_key(*name))
            .cloned()
            .collect(),
    })
}
