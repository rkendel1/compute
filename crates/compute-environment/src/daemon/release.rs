//! Releases: zero-downtime deployment as a durable, resumable state machine.
//!
//! ```text
//! pending → starting → ready → network_ready → switching → active → draining → complete
//!    ╰─────────╰─────────╰──────────╰── failed        (what served keeps serving)
//!                                       switching/active ── rolled_back
//! ```
//!
//! | Status | Durable evidence | What the controller does next |
//! | --- | --- | --- |
//! | `pending` | the release record | admit and place every workload, plan ports, create `starting` instances |
//! | `starting` | instances and their ports | start them next to what serves; check readiness; mark instances `ready` |
//! | `ready` | readiness result | verify endpoints, and record DNS and TLS for its domains |
//! | `network_ready` | network result | one transaction: traffic assignments, membership, workloads, instance states |
//! | `switching` | traffic assignments | verify the data plane follows them, through the endpoint |
//! | `active` | switch result | begin draining the replaced instances |
//! | `draining` | `draining` instances | wait until each finished its connections or the drain timeout |
//! | `complete` | the deployment receipt | nothing |
//!
//! Each step reads control state, inspects the node, and commits at most
//! one transition, so a daemon that restarts at any point reloads the
//! release and continues from its status. Nothing that serves is stopped
//! before traffic has moved; a release that fails before `switching`
//! leaves the previous revision serving, and one that fails after it is
//! rolled back to the previous revision.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use compute_state::events;
use compute_state::{
    DeploymentRecord, DeploymentStatus, DeploymentWorkload, EnvironmentProjectRecord,
    InstanceState, Readiness, ReadinessCheck, RevisionWorkload, Stored, TrafficAssignmentRecord,
    WorkloadInstanceRecord, WorkloadRecord, ids,
};
use serde_json::{Value, json};

use super::execute::{Target, placement_failure, run_config};
use super::reconcile::accepts;
use super::{Change, Daemon, Desired, Key, Scope, Unit};
use crate::EnvironmentError;
use crate::model::*;

/// A process with no port to check is ready once it stays up this long.
const PROCESS_READY_AFTER: Duration = Duration::from_millis(500);

impl Daemon {
    /// Advance every release in flight as far as the evidence allows, and
    /// finish draining what releases replaced. Returns whether control
    /// state changed.
    pub(crate) async fn advance_releases(self: &Arc<Self>) -> bool {
        let mut changed = false;
        for _ in 0..8 {
            // The data plane follows every committed switch before anything
            // it replaced may stop.
            self.sync_endpoints().await;
            let mut progressed = self.adopt_services().await;
            progressed |= self.drain_instances().await;
            let releases = self
                .inner
                .lock()
                .await
                .desired
                .in_flight()
                .cloned()
                .collect::<Vec<_>>();
            for release in releases {
                match self.step(&release).await {
                    Ok(advanced) => progressed |= advanced,
                    // A write that did not commit is retried next cycle,
                    // from whatever control state then says.
                    Err(EnvironmentError::Unavailable(error)) => {
                        self.inner.lock().await.state_error = Some(error);
                    }
                    Err(error) => {
                        eprintln!("release {}: {error}; retrying", release.id);
                    }
                }
            }
            if !progressed {
                break;
            }
            changed = true;
            if self.refresh().await.is_err() {
                break;
            }
        }
        changed
    }

    async fn step(
        self: &Arc<Self>,
        release: &Stored<DeploymentRecord>,
    ) -> Result<bool, EnvironmentError> {
        let desired = self.inner.lock().await.desired.clone();
        let record = &release.value;
        if !desired.environments.contains_key(&record.environment) {
            return self
                .fail(
                    release,
                    "environment",
                    "the environment was destroyed".into(),
                )
                .await;
        }
        let membership = desired
            .memberships
            .get(&(record.environment.clone(), record.project.clone()));
        if membership.is_none() {
            return self
                .fail(
                    release,
                    "project",
                    "the project was removed from the environment".into(),
                )
                .await;
        }
        // A deployment recorded before releases that is already current has
        // nothing to release.
        let current = membership.and_then(|membership| membership.value.deployment_id.as_deref())
            == Some(release.id.as_str());
        let has_instances = desired
            .instances
            .values()
            .any(|instance| instance.value.deployment_id == release.id);
        if current
            && !has_instances
            && matches!(
                record.status,
                DeploymentStatus::Pending | DeploymentStatus::Starting | DeploymentStatus::Ready
            )
        {
            return self.complete(release, &desired).await;
        }
        match record.status {
            DeploymentStatus::Pending => self.admit(release, &desired).await,
            DeploymentStatus::Starting => self.await_readiness(release, &desired).await,
            DeploymentStatus::Ready => self.verify_network(release, &desired).await,
            DeploymentStatus::NetworkReady => self.switch(release, &desired).await,
            DeploymentStatus::Switching => self.verify_switch(release, &desired).await,
            DeploymentStatus::Active => self.begin_drain(release).await,
            DeploymentStatus::Draining => self.finish_drain(release, &desired).await,
            _ => Ok(false),
        }
    }

    /// Add a status transition to a change.
    fn transition(
        &self,
        change: Change,
        release: &Stored<DeploymentRecord>,
        next: DeploymentStatus,
        mut fields: Value,
    ) -> Change {
        let now = Utc::now();
        fields["status"] = json!(next);
        fields["status_since"] = json!(now);
        fields["updated_at"] = json!(now);
        change.with(|batch| batch.update(release, fields))
    }

    fn release_scope(release: &Stored<DeploymentRecord>) -> Scope {
        Scope::project(&release.value.environment, &release.value.project).deployment(&release.id)
    }

    fn since(release: &DeploymentRecord) -> chrono::TimeDelta {
        Utc::now() - release.status_since.unwrap_or(release.updated_at)
    }

    // ---- pending: admission, placement, ports ------------------------------

    async fn admit(
        self: &Arc<Self>,
        release: &Stored<DeploymentRecord>,
        desired: &Desired,
    ) -> Result<bool, EnvironmentError> {
        let record = &release.value;
        let environment = desired
            .environments
            .get(&record.environment)
            .cloned()
            .expect("checked");
        let Some(revision) = desired.revisions.get(&record.revision_id).cloned() else {
            return self
                .fail(
                    release,
                    "admission",
                    format!("revision {} is missing", record.revision_id),
                )
                .await;
        };
        let mut used_endpoints = desired
            .workloads
            .values()
            .flat_map(|workload| workload.value.ports.iter().map(|port| port.host))
            .chain(
                desired
                    .traffic
                    .values()
                    .map(|assignment| assignment.value.host_port),
            )
            .chain(
                desired
                    .in_flight()
                    .filter(|other| other.id != release.id)
                    .flat_map(|other| other.value.workloads.iter())
                    .flat_map(|workload| workload.endpoints.iter().map(|port| port.host)),
            )
            .collect::<BTreeSet<_>>();
        let mut used_instances = desired
            .instances
            .values()
            .filter(|instance| {
                !matches!(
                    instance.value.state,
                    InstanceState::Stopped | InstanceState::Failed
                )
            })
            .flat_map(|instance| instance.value.ports.iter().map(|port| port.host))
            .collect::<BTreeSet<_>>();
        let mut evidence = vec![];
        let mut instances = vec![];
        let mut failure = None;
        for workload in &revision.workloads {
            let key: Key = (
                record.environment.clone(),
                record.project.clone(),
                workload.name.clone(),
            );
            let workload_id = ids::workload(&environment.id, &record.project_id, &workload.name);
            let service = workload.kind == WorkloadKind::Service;
            let (endpoints, instance_ports) = if service {
                match self.plan_ports(
                    desired,
                    &key,
                    workload,
                    &mut used_endpoints,
                    &mut used_instances,
                ) {
                    Ok(ports) => ports,
                    Err(error) => {
                        failure = Some(format!("{}: {error}", workload.name));
                        break;
                    }
                }
            } else {
                (vec![], vec![])
            };
            let prepared = match self
                .prepare(Target {
                    environment: &environment,
                    config: &record.config,
                    project_id: &record.project_id,
                    revision: &revision,
                    workload,
                    workload_id: &workload_id,
                    ports: &instance_ports,
                })
                .await
            {
                Ok(prepared) => prepared,
                Err(error) => {
                    failure = Some(format!("{}: {error}", workload.name));
                    break;
                }
            };
            let report = &prepared.report;
            let mut admitted = DeploymentWorkload {
                name: workload.name.clone(),
                kind: workload.kind,
                bundle_id: workload.bundle_id.clone(),
                admitted: false,
                policy_id: Some(report.policy_id.clone()),
                admission_id: None,
                placement_id: Some(report.placement_id.clone()),
                provider: None,
                reasons: vec![],
                endpoints,
            };
            match &report.selected {
                Some(selected)
                    if service
                        && selected.provider_kind != compute_placement::ProviderKind::Local =>
                {
                    admitted.provider = Some(selected.provider_id.clone());
                    admitted.reasons.push(format!(
                        "services run on the daemon's own node; placement selected {}",
                        selected.provider_id
                    ));
                }
                Some(selected) => {
                    admitted.admitted = true;
                    admitted.policy_id = Some(selected.policy_id.clone());
                    admitted.admission_id = Some(selected.admission_id.clone());
                    admitted.provider = Some(selected.provider_id.clone());
                }
                None => {
                    let (message, decision) = placement_failure(report);
                    if let Some(decision) = decision {
                        admitted.policy_id = Some(decision.policy_id.clone());
                        admitted.admission_id = Some(decision.admission_id.clone());
                    }
                    admitted.reasons.push(message);
                }
            }
            if !admitted.admitted {
                failure = Some(format!(
                    "{}: {}",
                    workload.name,
                    admitted.reasons.join("; ")
                ));
                evidence.push(admitted);
                break;
            }
            evidence.push(admitted);
            if service {
                instances.push((workload.clone(), workload_id, instance_ports));
            }
        }
        if let Some(failure) = failure {
            return self
                .fail_with(
                    Change::new(),
                    release,
                    desired,
                    "admission",
                    failure,
                    json!({ "workloads": evidence }),
                )
                .await;
        }
        let now = Utc::now();
        let mut change = self.transition(
            Change::new(),
            release,
            DeploymentStatus::Starting,
            json!({ "workloads": evidence }),
        );
        for (workload, workload_id, ports) in instances {
            let id = ids::instance(&workload_id, &release.id);
            if desired.instances.contains_key(&id) {
                continue;
            }
            let instance = WorkloadInstanceRecord {
                environment_id: record.environment_id.clone(),
                environment: record.environment.clone(),
                project_id: record.project_id.clone(),
                project: record.project.clone(),
                workload: workload.name.clone(),
                workload_id,
                deployment_id: release.id.clone(),
                revision: record.revision.clone(),
                state: InstanceState::Starting,
                ports,
                readiness: None,
                started_at: Some(now),
                ready_at: None,
                stopped_at: None,
                error: None,
                updated_at: now,
            };
            change = change.with(|batch| batch.create(&id, &instance));
        }
        let scope = Self::release_scope(release);
        let change = self.event(
            change,
            events::DEPLOYMENT_ADMITTED,
            scope.clone(),
            format!(
                "{} {} admitted in {}",
                record.project, record.revision, record.environment
            ),
            json!({ "admissions": evidence.iter().map(|w| &w.admission_id).collect::<Vec<_>>() }),
        );
        let change = self.event(
            change,
            events::DEPLOYMENT_PLACED,
            scope,
            format!(
                "{} {} placed in {}; starting its instances",
                record.project, record.revision, record.environment
            ),
            json!({ "providers": evidence.iter().map(|w| &w.provider).collect::<Vec<_>>() }),
        );
        self.apply(change).await?;
        Ok(true)
    }

    /// A service's stable endpoints (kept across releases by port name) and
    /// its new instance's own ports.
    fn plan_ports(
        &self,
        desired: &Desired,
        key: &Key,
        workload: &RevisionWorkload,
        used_endpoints: &mut BTreeSet<u16>,
        used_instances: &mut BTreeSet<u16>,
    ) -> Result<(Vec<PortBinding>, Vec<PortBinding>), EnvironmentError> {
        let existing = desired.workloads.get(key);
        let mut endpoints = vec![];
        let mut instance_ports = vec![];
        for port in &workload.ports {
            let endpoint = ids::endpoint(&key.0, &key.1, &key.2, &port.name);
            let kept = existing
                .and_then(|existing| {
                    existing
                        .value
                        .ports
                        .iter()
                        .find(|binding| binding.name == port.name)
                        .map(|binding| binding.host)
                })
                .or_else(|| {
                    desired
                        .traffic
                        .get(&endpoint)
                        .map(|assignment| assignment.value.host_port)
                });
            let host = match kept {
                Some(host) => host,
                None => {
                    let host = self.free_port(self.config.port_range, used_endpoints, true)?;
                    used_endpoints.insert(host);
                    host
                }
            };
            endpoints.push(PortBinding {
                name: port.name.clone(),
                logical: port.port,
                host,
            });
            let instance =
                self.free_port(self.config.instance_port_range, used_instances, false)?;
            used_instances.insert(instance);
            instance_ports.push(PortBinding {
                name: port.name.clone(),
                logical: port.port,
                host: instance,
            });
        }
        Ok((endpoints, instance_ports))
    }

    fn free_port(
        &self,
        (low, high): (u16, u16),
        used: &BTreeSet<u16>,
        endpoint: bool,
    ) -> Result<u16, EnvironmentError> {
        let address = if endpoint {
            self.config.network.endpoint_address
        } else {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        };
        (low..=high)
            .find(|candidate| {
                !used.contains(candidate)
                    && std::net::TcpListener::bind((address, *candidate)).is_ok()
            })
            .ok_or_else(|| {
                EnvironmentError::Invalid(format!(
                    "no free host port in {low}-{high} for {}",
                    if endpoint {
                        "an endpoint"
                    } else {
                        "an instance"
                    }
                ))
            })
    }

    // ---- starting: readiness ----------------------------------------------

    async fn await_readiness(
        self: &Arc<Self>,
        release: &Stored<DeploymentRecord>,
        desired: &Desired,
    ) -> Result<bool, EnvironmentError> {
        let record = &release.value;
        let Some(revision) = desired.revisions.get(&record.revision_id) else {
            return Ok(false);
        };
        let instances = desired
            .instances
            .values()
            .filter(|instance| instance.value.deployment_id == release.id)
            .cloned()
            .collect::<Vec<_>>();
        let mut change = Change::new();
        let mut results = BTreeMap::new();
        let mut waiting = vec![];
        let mut timeout = Duration::ZERO;
        for instance in &instances {
            let key = Desired::instance_key(&instance.value);
            let Some(workload) = revision
                .workloads
                .iter()
                .find(|workload| workload.name == key.2)
            else {
                continue;
            };
            let readiness = workload
                .readiness
                .clone()
                .unwrap_or_else(|| Readiness::default_for(&workload.ports));
            timeout = timeout.max(Duration::from_millis(readiness.timeout_ms));
            if instance.value.state == InstanceState::Ready {
                results.insert(
                    key.2.clone(),
                    json!({
                        "check": readiness.check,
                        "ready_at": instance.value.ready_at,
                        "detail": instance.value.readiness,
                    }),
                );
                continue;
            }
            let outcome = if desired.workload_runs(&key, &release.id) {
                self.check_readiness(release, instance, workload, &readiness)
                    .await
            } else {
                Check::Ready("not started: its desired state is stopped".into())
            };
            match outcome {
                Check::Ready(detail) => {
                    let now = Utc::now();
                    change = change.with(|batch| {
                        batch.update(
                            instance,
                            json!({
                                "state": InstanceState::Ready,
                                "ready_at": now,
                                "readiness": detail,
                                "updated_at": now,
                            }),
                        )
                    });
                    change = self.event(
                        change,
                        events::INSTANCE_READY,
                        Scope::workload(&key).deployment(&release.id),
                        format!(
                            "{} {} is ready in {}/{}: {detail}",
                            key.2, record.revision, key.0, key.1
                        ),
                        json!({ "instance_id": instance.id, "check": readiness.check }),
                    );
                    results.insert(
                        key.2.clone(),
                        json!({ "check": readiness.check, "ready_at": now, "detail": detail }),
                    );
                }
                Check::Waiting(detail) => waiting.push(format!("{}: {detail}", key.2)),
                Check::Failed(reason) => {
                    return self
                        .fail_with(
                            change,
                            release,
                            desired,
                            "readiness",
                            format!("{} failed before it was ready: {reason}", key.2),
                            json!({}),
                        )
                        .await;
                }
            }
        }
        if waiting.is_empty() {
            let change = self.transition(
                change,
                release,
                DeploymentStatus::Ready,
                json!({ "readiness_result": results }),
            );
            let change = self.event(
                change,
                events::DEPLOYMENT_READY,
                Self::release_scope(release),
                format!(
                    "{} {} is ready in {}",
                    record.project, record.revision, record.environment
                ),
                json!({ "readiness": results }),
            );
            self.apply(change).await?;
            return Ok(true);
        }
        if Self::since(record).to_std().unwrap_or_default() > timeout {
            return self
                .fail_with(
                    change,
                    release,
                    desired,
                    "readiness",
                    format!(
                        "readiness timed out after {}s: {}",
                        timeout.as_secs(),
                        waiting.join("; ")
                    ),
                    json!({}),
                )
                .await;
        }
        if change.batch.is_empty() {
            return Ok(false);
        }
        self.apply(change).await?;
        Ok(true)
    }

    async fn check_readiness(
        self: &Arc<Self>,
        release: &Stored<DeploymentRecord>,
        instance: &Stored<WorkloadInstanceRecord>,
        workload: &RevisionWorkload,
        readiness: &Readiness,
    ) -> Check {
        let key = Desired::instance_key(&instance.value);
        let unit = Unit::new(&key, &release.id);
        let (state, held, failures, error, running_since) = {
            let mut inner = self.inner.lock().await;
            // Honor the check interval.
            let now = std::time::Instant::now();
            let last = inner.network.readiness_checked.get(&instance.id).copied();
            if last.is_some_and(|last| {
                now.duration_since(last) < Duration::from_millis(readiness.interval_ms)
            }) {
                return Check::Waiting("checking".into());
            }
            inner
                .network
                .readiness_checked
                .insert(instance.id.clone(), now);
            match inner.runtime.get(&unit) {
                Some(runtime) => (
                    runtime.state,
                    runtime.held,
                    runtime.consecutive_failures,
                    runtime.error.clone(),
                    runtime.running_since,
                ),
                None => return Check::Waiting("not started yet".into()),
            }
        };
        let error = error.unwrap_or_else(|| "it exited".into());
        match state {
            Some(ActualState::Denied) => return Check::Failed(format!("denied: {error}")),
            Some(ActualState::Failed | ActualState::Stopped | ActualState::Completed)
                if held && (workload.restart == RestartPolicy::Never || failures >= 3) =>
            {
                return Check::Failed(error);
            }
            Some(ActualState::Running) => {}
            _ => return Check::Waiting("starting".into()),
        }
        let port = || {
            let binding = match &readiness.port {
                Some(name) => instance.value.ports.iter().find(|port| port.name == *name),
                None => instance.value.ports.first(),
            };
            binding.map(|binding| (binding.name.clone(), binding.host))
        };
        match readiness.check {
            ReadinessCheck::Process => {
                let up = running_since
                    .map(|since| Utc::now() - since)
                    .and_then(|elapsed| elapsed.to_std().ok())
                    .unwrap_or_default();
                if up >= PROCESS_READY_AFTER {
                    Check::Ready(format!("running for {}ms", up.as_millis()))
                } else {
                    Check::Waiting("the process has just started".into())
                }
            }
            ReadinessCheck::Port => match port() {
                Some((name, host)) if accepts(host).await => {
                    Check::Ready(format!("port {name} accepts connections"))
                }
                Some((name, _)) => {
                    Check::Waiting(format!("port {name} is not accepting connections"))
                }
                None => Check::Failed("readiness names a port the service does not declare".into()),
            },
            ReadinessCheck::Http => {
                let path = readiness.path.clone().unwrap_or_else(|| "/".into());
                match port() {
                    Some((name, host)) => match http_status(host, &path).await {
                        Some(status) if (200..400).contains(&status) => {
                            Check::Ready(format!("GET {path} on port {name} answered {status}"))
                        }
                        Some(status) => Check::Waiting(format!("GET {path} answered {status}")),
                        None => Check::Waiting(format!("GET {path} on port {name} got no answer")),
                    },
                    None => {
                        Check::Failed("readiness names a port the service does not declare".into())
                    }
                }
            }
            ReadinessCheck::Task => {
                let Some(task) = readiness.task.clone() else {
                    return Check::Failed("a task readiness check needs a task".into());
                };
                let task_key = (key.0.clone(), key.1.clone(), task.clone());
                match self.run_unit_task(Unit::new(&task_key, &release.id)).await {
                    Ok(execution)
                        if execution.record.exit_code.unwrap_or(0) == 0
                            && execution.record.status == "completed" =>
                    {
                        Check::Ready(format!("task {task} exited 0"))
                    }
                    Ok(execution) => Check::Waiting(format!(
                        "task {task} ended {} ({})",
                        execution.record.status,
                        execution
                            .record
                            .exit_code
                            .map_or("no exit code".into(), |c| c.to_string())
                    )),
                    Err(error) => Check::Waiting(format!("task {task}: {error}")),
                }
            }
        }
    }

    // ---- ready: endpoints, DNS, TLS ----------------------------------------

    async fn verify_network(
        self: &Arc<Self>,
        release: &Stored<DeploymentRecord>,
        desired: &Desired,
    ) -> Result<bool, EnvironmentError> {
        let record = &release.value;
        let endpoint_errors = self.inner.lock().await.endpoint_errors.clone();
        let mut endpoints = vec![];
        for workload in &record.workloads {
            let key: Key = (
                record.environment.clone(),
                record.project.clone(),
                workload.name.clone(),
            );
            let Some(instance) = desired.instance(&key, &release.id) else {
                continue;
            };
            for binding in resolved_endpoints(desired, &key, workload) {
                let endpoint = ids::endpoint(&key.0, &key.1, &key.2, &binding.name);
                let listening = self.route(binding.host).is_some();
                if let Some(error) = endpoint_errors.get(&binding.host) {
                    return self.fail(release, "network", error.clone()).await;
                }
                if !listening
                    && std::net::TcpListener::bind((
                        self.config.network.endpoint_address,
                        binding.host,
                    ))
                    .is_err()
                {
                    return self
                        .fail(
                            release,
                            "network",
                            format!(
                                "endpoint {endpoint} cannot listen: port {} is in use by another process",
                                binding.host
                            ),
                        )
                        .await;
                }
                let target = instance
                    .value
                    .ports
                    .iter()
                    .find(|port| port.name == binding.name)
                    .map(|port| port.host);
                let reachable = match target {
                    Some(port) => accepts(port).await,
                    None => false,
                };
                // A service that proved readiness on a port must still
                // accept connections there.
                let listens = listens(desired, &release.id, &key.2);
                let runs = desired.workload_runs(&key, &release.id);
                if runs && listens && !reachable {
                    if Self::since(record).to_std().unwrap_or_default() > self.config.switch_timeout
                    {
                        return self
                            .fail(
                                release,
                                "network",
                                format!(
                                    "{} stopped accepting connections on {}",
                                    key.2, binding.name
                                ),
                            )
                            .await;
                    }
                    return Ok(false);
                }
                endpoints.push(json!({
                    "endpoint": endpoint,
                    "host_port": binding.host,
                    "target_port": target,
                    "listening": listening,
                    "reachable": reachable,
                }));
            }
        }
        let domains = desired
            .domains
            .values()
            .filter(|domain| {
                domain.value.environment == record.environment
                    && domain.value.project == record.project
            })
            .map(|domain| {
                json!({
                    "domain": domain.value.name,
                    "endpoint": ids::endpoint(&domain.value.environment, &domain.value.project, &domain.value.workload, &domain.value.port),
                    "dns": domain.value.dns.status,
                    "tls": domain.value.tls.status,
                    "routing": domain.value.routing.status,
                })
            })
            .collect::<Vec<_>>();
        // DNS and TLS belong to the domain, not the revision: they are
        // recorded here and never hold a release back.
        let degraded = domains
            .iter()
            .any(|domain| domain["dns"] == "failed" || domain["tls"] == "failed");
        let result = json!({
            "endpoints": endpoints,
            "domains": domains,
            "degraded": degraded,
            "verified_at": Utc::now(),
        });
        let change = self.transition(
            Change::new(),
            release,
            DeploymentStatus::NetworkReady,
            json!({ "network_result": result }),
        );
        self.apply(change).await?;
        Ok(true)
    }

    // ---- network_ready → switching: the switch -----------------------------

    async fn switch(
        self: &Arc<Self>,
        release: &Stored<DeploymentRecord>,
        desired: &Desired,
    ) -> Result<bool, EnvironmentError> {
        let record = &release.value;
        let (change, moved) = self.route_to(Change::new(), desired, release).await?;
        let result = json!({ "endpoints": moved, "switched_at": Utc::now() });
        let change = self.transition(
            change,
            release,
            DeploymentStatus::Switching,
            json!({ "traffic_switch_result": result }),
        );
        let scope = Self::release_scope(release);
        let change = self.event(
            change,
            events::DEPLOYMENT_SWITCHED,
            scope.clone(),
            format!(
                "{} {} now receives traffic in {}",
                record.project, record.revision, record.environment
            ),
            json!({ "endpoints": moved }),
        );
        let change = self.event(
            change,
            events::DEPLOYMENT_ACTIVATED,
            scope.clone(),
            format!(
                "{} {} is now current in {}",
                record.project, record.revision, record.environment
            ),
            json!({ "revision": record.revision, "previous": record.previous }),
        );
        let change = if moved.is_empty() {
            change
        } else {
            self.event(
                change,
                events::ROUTE_SWITCHED,
                scope,
                format!(
                    "{} endpoint(s) switched to {}",
                    moved.len(),
                    record.revision
                ),
                json!({ "endpoints": moved }),
            )
        };
        self.apply(change).await?;
        Ok(true)
    }

    /// Make `target` the deployment that serves its project: membership,
    /// workloads, instance states, and traffic assignments, in one change.
    /// Returns the endpoints that moved.
    async fn route_to(
        &self,
        mut change: Change,
        desired: &Desired,
        target: &Stored<DeploymentRecord>,
    ) -> Result<(Change, Vec<Value>), EnvironmentError> {
        let record = &target.value;
        let now = Utc::now();
        let environment = &record.environment;
        let project = &record.project;
        let revision = desired.revisions.get(&record.revision_id).ok_or_else(|| {
            EnvironmentError::NotFound(format!("revision {}", record.revision_id))
        })?;
        let membership = desired
            .memberships
            .get(&(environment.clone(), project.clone()));
        let membership_record = EnvironmentProjectRecord {
            environment_id: record.environment_id.clone(),
            environment: environment.clone(),
            project_id: record.project_id.clone(),
            project: project.clone(),
            desired_state: membership.map_or(DesiredState::Running, |m| m.value.desired_state),
            config: run_config(record, membership),
            revision_id: Some(record.revision_id.clone()),
            deployment_id: Some(target.id.clone()),
            created_at: membership.map_or(now, |m| m.value.created_at),
            updated_at: now,
        };
        change = match membership {
            Some(existing) => change.with(|batch| batch.replace(existing, &membership_record)),
            None => change.with(|batch| {
                batch.create(
                    &ids::membership(&record.environment_id, &record.project_id),
                    &membership_record,
                )
            }),
        };
        // Workloads follow the revision; each keeps its desired state.
        let mut kept = BTreeSet::new();
        let mut endpoints = BTreeMap::new();
        for workload in &revision.workloads {
            let key: Key = (environment.clone(), project.clone(), workload.name.clone());
            kept.insert(key.clone());
            let existing = desired.workloads.get(&key);
            let evidence = record
                .workloads
                .iter()
                .find(|evidence| evidence.name == workload.name);
            let ports = match evidence {
                Some(evidence) => resolved_endpoints(desired, &key, evidence),
                None => existing.map(|e| e.value.ports.clone()).unwrap_or_default(),
            };
            let value = WorkloadRecord {
                environment_id: record.environment_id.clone(),
                environment: environment.clone(),
                project_id: record.project_id.clone(),
                project: project.clone(),
                name: workload.name.clone(),
                kind: workload.kind,
                desired_state: existing.map_or(workload.desired_state, |e| e.value.desired_state),
                restart: workload.restart,
                bundle_id: workload.bundle_id.clone(),
                workload_identity: workload.workload_identity.clone(),
                runtime: workload.runtime.clone(),
                ports: ports.clone(),
                deployment_id: target.id.clone(),
            };
            change = match existing {
                Some(existing) => change.with(|batch| batch.replace(existing, &value)),
                None => change.with(|batch| {
                    batch.create(
                        &ids::workload(&record.environment_id, &record.project_id, &workload.name),
                        &value,
                    )
                }),
            };
            if workload.kind == WorkloadKind::Service {
                endpoints.insert(key, ports);
            }
        }
        for (key, existing) in desired.workloads_of(environment, project) {
            if !kept.contains(key) {
                change = change.with(|batch| batch.delete(existing));
                change = self.delete_status(change, &existing.id).await?;
            }
        }
        // Instances: the target's serve; whatever served drains.
        for instance in desired.instances.values().filter(|instance| {
            instance.value.environment == *environment && instance.value.project == *project
        }) {
            let next = if instance.value.deployment_id == target.id {
                InstanceState::Serving
            } else {
                match instance.value.state {
                    InstanceState::Serving => InstanceState::Draining,
                    InstanceState::Starting | InstanceState::Ready => InstanceState::Stopped,
                    other => other,
                }
            };
            if next != instance.value.state {
                let mut fields = json!({ "state": next, "updated_at": now });
                if next == InstanceState::Stopped {
                    fields["stopped_at"] = json!(now);
                }
                change = change.with(|batch| batch.update(instance, fields));
            }
        }
        // Traffic: one assignment per endpoint, naming exactly one instance.
        let mut moved = vec![];
        let mut assigned = BTreeSet::new();
        for (key, ports) in &endpoints {
            let Some(instance) = desired.instance(key, &target.id) else {
                continue;
            };
            for binding in ports {
                let endpoint = ids::endpoint(&key.0, &key.1, &key.2, &binding.name);
                let Some(target_port) = instance
                    .value
                    .ports
                    .iter()
                    .find(|port| port.name == binding.name)
                    .map(|port| port.host)
                else {
                    continue;
                };
                assigned.insert(endpoint.clone());
                let existing = desired.traffic.get(&endpoint);
                let domains = desired
                    .domains
                    .values()
                    .filter(|domain| {
                        ids::endpoint(
                            &domain.value.environment,
                            &domain.value.project,
                            &domain.value.workload,
                            &domain.value.port,
                        ) == endpoint
                    })
                    .map(|domain| domain.value.name.clone())
                    .collect();
                let assignment = TrafficAssignmentRecord {
                    endpoint: endpoint.clone(),
                    environment: key.0.clone(),
                    project: key.1.clone(),
                    workload: key.2.clone(),
                    port: binding.name.clone(),
                    host_port: binding.host,
                    domains,
                    deployment_id: target.id.clone(),
                    revision: record.revision.clone(),
                    instance_id: instance.id.clone(),
                    target_port,
                    status: "active".into(),
                    previous_deployment_id: existing.map(|e| e.value.deployment_id.clone()),
                    previous_instance_id: existing.map(|e| e.value.instance_id.clone()),
                    switched_at: now,
                };
                moved.push(json!({
                    "endpoint": endpoint,
                    "host_port": binding.host,
                    "from_instance": assignment.previous_instance_id,
                    "from_revision": existing.map(|e| e.value.revision.clone()),
                    "to_instance": instance.id,
                    "to_port": target_port,
                }));
                change = match existing {
                    Some(existing) => change.with(|batch| batch.replace(existing, &assignment)),
                    None => {
                        change.with(|batch| batch.create(&ids::traffic(&endpoint), &assignment))
                    }
                };
            }
        }
        for (endpoint, existing) in &desired.traffic {
            if existing.value.environment == *environment
                && existing.value.project == *project
                && !assigned.contains(endpoint)
            {
                change = change.with(|batch| batch.delete(existing));
            }
        }
        Ok((change, moved))
    }

    // ---- switching: verification through the data plane --------------------

    async fn verify_switch(
        self: &Arc<Self>,
        release: &Stored<DeploymentRecord>,
        desired: &Desired,
    ) -> Result<bool, EnvironmentError> {
        let record = &release.value;
        let mut verified = vec![];
        let mut problem = None;
        for assignment in desired
            .traffic
            .values()
            .filter(|assignment| assignment.value.deployment_id == release.id)
        {
            let assignment = &assignment.value;
            let key: Key = (
                assignment.environment.clone(),
                assignment.project.clone(),
                assignment.workload.clone(),
            );
            let route = self.route(assignment.host_port);
            if route
                .as_ref()
                .map(|route| (&route.instance_id, route.target_port))
                != Some((&assignment.instance_id, assignment.target_port))
            {
                problem = Some(format!(
                    "endpoint {} does not yet forward to {}",
                    assignment.endpoint, assignment.instance_id
                ));
                break;
            }
            if !desired.workload_runs(&key, &release.id) {
                verified.push(json!({ "endpoint": assignment.endpoint, "verified": "not running by desired state" }));
                continue;
            }
            let readiness = desired
                .revision_workload(&release.id, &key.2)
                .and_then(|workload| workload.readiness.clone());
            let check = match readiness {
                Some(readiness)
                    if readiness.check == ReadinessCheck::Http
                        && readiness
                            .port
                            .as_ref()
                            .is_none_or(|port| *port == assignment.port) =>
                {
                    let path = readiness.path.unwrap_or_else(|| "/".into());
                    match http_status(assignment.host_port, &path).await {
                        Some(status) if (200..400).contains(&status) => {
                            Ok(format!("GET {path} through the endpoint answered {status}"))
                        }
                        other => Err(format!(
                            "GET {path} through endpoint {} answered {other:?}",
                            assignment.endpoint
                        )),
                    }
                }
                _ if !listens(desired, &release.id, &key.2) => Ok(
                    "the endpoint forwards to the instance; readiness does not check a port".into(),
                ),
                _ => {
                    if accepts(assignment.target_port).await {
                        Ok("the endpoint forwards to an instance that accepts connections".into())
                    } else {
                        Err(format!(
                            "the instance behind {} does not accept connections",
                            assignment.endpoint
                        ))
                    }
                }
            };
            match check {
                Ok(detail) => {
                    verified.push(json!({ "endpoint": assignment.endpoint, "verified": detail }))
                }
                Err(error) => {
                    problem = Some(error);
                    break;
                }
            }
        }
        if let Some(problem) = problem {
            if Self::since(record).to_std().unwrap_or_default() > self.config.switch_timeout {
                return self.roll_back(release, desired, problem).await;
            }
            return Ok(false);
        }
        let mut result = record
            .traffic_switch_result
            .clone()
            .unwrap_or_else(|| json!({}));
        result["verified"] = json!(verified);
        result["verified_at"] = json!(Utc::now());
        let change = self.transition(
            Change::new(),
            release,
            DeploymentStatus::Active,
            json!({ "traffic_switch_result": result }),
        );
        self.apply(change).await?;
        Ok(true)
    }

    // ---- active → draining → complete ---------------------------------------

    async fn begin_drain(
        self: &Arc<Self>,
        release: &Stored<DeploymentRecord>,
    ) -> Result<bool, EnvironmentError> {
        let record = &release.value;
        let change = self.transition(
            Change::new(),
            release,
            DeploymentStatus::Draining,
            json!({}),
        );
        let change = self.event(
            change,
            events::DEPLOYMENT_DRAINING,
            Self::release_scope(release),
            format!(
                "{} {} serves in {}; draining what it replaced",
                record.project, record.revision, record.environment
            ),
            json!({ "drain_timeout_ms": self.config.drain_timeout.as_millis() as u64 }),
        );
        self.apply(change).await?;
        Ok(true)
    }

    async fn finish_drain(
        self: &Arc<Self>,
        release: &Stored<DeploymentRecord>,
        desired: &Desired,
    ) -> Result<bool, EnvironmentError> {
        let record = &release.value;
        let draining = desired.instances.values().any(|instance| {
            instance.value.environment == record.environment
                && instance.value.project == record.project
                && instance.value.state == InstanceState::Draining
        });
        if draining {
            return Ok(false);
        }
        self.complete(release, desired).await
    }

    async fn complete(
        self: &Arc<Self>,
        release: &Stored<DeploymentRecord>,
        desired: &Desired,
    ) -> Result<bool, EnvironmentError> {
        let record = &release.value;
        let now = Utc::now();
        let mut finished = record.clone();
        finished.status = DeploymentStatus::Complete;
        finished.completed_at = Some(now);
        let receipt = self.deployment_receipt(&release.id, &finished).await?;
        let mut change = self.transition(
            Change::new(),
            release,
            DeploymentStatus::Complete,
            json!({ "completed_at": now, "receipt": receipt }),
        );
        // Instances replaced by earlier releases are history now.
        for instance in desired.instances.values().filter(|instance| {
            instance.value.environment == record.environment
                && instance.value.project == record.project
                && instance.value.deployment_id != release.id
                && matches!(
                    instance.value.state,
                    InstanceState::Stopped | InstanceState::Failed
                )
        }) {
            change = change.with(|batch| batch.delete(instance));
        }
        let change = self.event(
            change,
            events::DEPLOYMENT_COMPLETED,
            Self::release_scope(release),
            format!(
                "{} {} is released in {}",
                record.project, record.revision, record.environment
            ),
            json!({ "revision": record.revision, "receipt": receipt }),
        );
        self.apply(change).await?;
        Ok(true)
    }

    // ---- failure and rollback ------------------------------------------------

    pub(crate) async fn fail(
        self: &Arc<Self>,
        release: &Stored<DeploymentRecord>,
        stage: &str,
        reason: String,
    ) -> Result<bool, EnvironmentError> {
        let desired = self.inner.lock().await.desired.clone();
        self.fail_with(Change::new(), release, &desired, stage, reason, json!({}))
            .await
    }

    /// Fail a release that has not moved traffic. What served keeps
    /// serving; the release's own instances stop.
    /// Fail a release, merging `fields` (evidence gathered on the way) into
    /// the same write.
    async fn fail_with(
        self: &Arc<Self>,
        mut change: Change,
        release: &Stored<DeploymentRecord>,
        desired: &Desired,
        stage: &str,
        reason: String,
        mut fields: Value,
    ) -> Result<bool, EnvironmentError> {
        let record = &release.value;
        let now = Utc::now();
        let mut finished = record.clone();
        if let Some(workloads) = fields.get("workloads") {
            finished.workloads = serde_json::from_value(workloads.clone())?;
        }
        finished.status = DeploymentStatus::Failed;
        finished.failure = Some(reason.clone());
        finished.completed_at = Some(now);
        let receipt = self.deployment_receipt(&release.id, &finished).await?;
        fields["failure"] = json!(reason);
        fields["completed_at"] = json!(now);
        fields["receipt"] = json!(receipt);
        change = self.transition(change, release, DeploymentStatus::Failed, fields);
        for instance in desired
            .instances
            .values()
            .filter(|instance| instance.value.deployment_id == release.id)
        {
            if !matches!(
                instance.value.state,
                InstanceState::Stopped | InstanceState::Failed
            ) {
                change = change.with(|batch| {
                    batch.update(
                        instance,
                        json!({
                            "state": InstanceState::Failed,
                            "error": reason,
                            "stopped_at": now,
                            "updated_at": now,
                        }),
                    )
                });
            }
        }
        // A project whose first release failed was never deployed here.
        if let Some(membership) = desired
            .memberships
            .get(&(record.environment.clone(), record.project.clone()))
            && membership.value.deployment_id.is_none()
        {
            change = change.with(|batch| batch.delete(membership));
        }
        let change = self.event(
            change,
            events::DEPLOYMENT_FAILED,
            Self::release_scope(release),
            format!(
                "{} {} was not released to {}: {reason}",
                record.project, record.revision, record.environment
            ),
            json!({ "stage": stage, "reason": reason, "receipt": receipt }),
        );
        self.apply(change).await?;
        Ok(true)
    }

    /// Return traffic to the deployment the release replaced.
    pub(crate) async fn roll_back(
        self: &Arc<Self>,
        release: &Stored<DeploymentRecord>,
        desired: &Desired,
        reason: String,
    ) -> Result<bool, EnvironmentError> {
        let record = &release.value;
        let previous = match &record.previous {
            Some(previous) => match desired.deployments.get(previous) {
                Some(previous) => Some(previous.clone()),
                None => self.control().get::<DeploymentRecord>(previous).await?,
            },
            None => None,
        };
        let Some(previous) = previous else {
            // Nothing served before: take the release out of service.
            let mut change = Change::new();
            for assignment in desired.traffic.values().filter(|assignment| {
                assignment.value.environment == record.environment
                    && assignment.value.project == record.project
            }) {
                change = change.with(|batch| batch.delete(assignment));
            }
            for (_, workload) in desired.workloads_of(&record.environment, &record.project) {
                change = change.with(|batch| batch.delete(workload));
                change = self.delete_status(change, &workload.id).await?;
            }
            if let Some(membership) = desired
                .memberships
                .get(&(record.environment.clone(), record.project.clone()))
            {
                change = change.with(|batch| batch.delete(membership));
            }
            return self
                .fail_with(change, release, desired, "switch", reason, json!({}))
                .await;
        };
        let mut desired = desired.clone();
        desired
            .deployments
            .insert(previous.id.clone(), previous.clone());
        if !desired.revisions.contains_key(&previous.value.revision_id)
            && let Some(revision) = self
                .control()
                .get::<compute_state::ProjectRevisionRecord>(&previous.value.revision_id)
                .await?
        {
            desired
                .revisions
                .insert(previous.value.revision_id.clone(), revision.value);
        }
        // The previous instances serve again (restarted if they already
        // stopped); the release's own instances drain.
        let (change, moved) = self.route_to(Change::new(), &desired, &previous).await?;
        let now = Utc::now();
        let mut finished = record.clone();
        finished.status = DeploymentStatus::RolledBack;
        finished.rollback_reason = Some(reason.clone());
        finished.completed_at = Some(now);
        let receipt = self.deployment_receipt(&release.id, &finished).await?;
        let change = self.transition(
            change,
            release,
            DeploymentStatus::RolledBack,
            json!({ "rollback_reason": reason, "completed_at": now, "receipt": receipt }),
        );
        let change = self.event(
            change,
            events::DEPLOYMENT_ROLLED_BACK,
            Self::release_scope(release),
            format!(
                "{} {} was rolled back to {} in {}: {reason}",
                record.project, record.revision, previous.value.revision, record.environment
            ),
            json!({ "reason": reason, "to_deployment": previous.id, "endpoints": moved, "receipt": receipt }),
        );
        self.apply(change).await?;
        Ok(true)
    }

    // ---- instances outside releases ------------------------------------------

    /// Stop replaced instances once they have finished their connections,
    /// or once the drain timeout passes.
    async fn drain_instances(self: &Arc<Self>) -> bool {
        let draining = {
            let inner = self.inner.lock().await;
            // What a release replaced keeps running until the release is
            // verified, so a rollback returns to an instance that is up.
            let verifying = inner
                .desired
                .in_flight()
                .filter(|release| release.value.status == DeploymentStatus::Switching)
                .map(|release| {
                    (
                        release.value.environment.clone(),
                        release.value.project.clone(),
                    )
                })
                .collect::<BTreeSet<_>>();
            inner
                .desired
                .instances
                .values()
                .filter(|instance| {
                    instance.value.state == InstanceState::Draining
                        && !verifying.contains(&(
                            instance.value.environment.clone(),
                            instance.value.project.clone(),
                        ))
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut change = Change::new();
        let now = Utc::now();
        for instance in draining {
            // Never stop what an endpoint still routes to.
            let routed = !instance.value.ports.is_empty()
                && self
                    .routes_snapshot()
                    .values()
                    .any(|route| route.instance_id == instance.id);
            if routed {
                continue;
            }
            let open = self.open_connections(&instance.id).await;
            let waited = (now - instance.value.updated_at)
                .to_std()
                .unwrap_or_default();
            if open > 0 && waited < self.config.drain_timeout {
                continue;
            }
            change = change.with(|batch| {
                batch.update(
                    &instance,
                    json!({ "state": InstanceState::Stopped, "stopped_at": now, "updated_at": now }),
                )
            });
            let key = Desired::instance_key(&instance.value);
            change = self.event(
                change,
                events::INSTANCE_STOPPED,
                Scope::workload(&key).deployment(&instance.value.deployment_id),
                if open == 0 {
                    format!("{} {} drained and stopped", key.2, instance.value.revision)
                } else {
                    format!(
                        "{} {} stopped after the drain timeout with {open} connection(s) open",
                        key.2, instance.value.revision
                    )
                },
                json!({ "instance_id": instance.id, "open_connections": open }),
            );
        }
        if change.batch.is_empty() {
            return false;
        }
        match self.apply(change).await {
            Ok(()) => true,
            Err(error) => {
                self.inner.lock().await.state_error = Some(error.to_string());
                false
            }
        }
    }

    /// Services deployed before releases run without instances or traffic
    /// assignments: give each current service an instance that serves on
    /// its existing endpoints.
    async fn adopt_services(self: &Arc<Self>) -> bool {
        let desired = self.inner.lock().await.desired.clone();
        let releasing = desired
            .in_flight()
            .map(|release| {
                (
                    release.value.environment.clone(),
                    release.value.project.clone(),
                )
            })
            .collect::<BTreeSet<_>>();
        let mut used = desired
            .instances
            .values()
            .flat_map(|instance| instance.value.ports.iter().map(|port| port.host))
            .collect::<BTreeSet<_>>();
        let mut change = Change::new();
        let now = Utc::now();
        for (key, workload) in &desired.workloads {
            if workload.value.kind != WorkloadKind::Service
                || releasing.contains(&(key.0.clone(), key.1.clone()))
                || desired
                    .instance(key, &workload.value.deployment_id)
                    .is_some()
                || !desired
                    .deployments
                    .contains_key(&workload.value.deployment_id)
            {
                continue;
            }
            let mut ports = vec![];
            for binding in &workload.value.ports {
                let Ok(port) = self.free_port(self.config.instance_port_range, &used, false) else {
                    return false;
                };
                used.insert(port);
                ports.push(PortBinding {
                    name: binding.name.clone(),
                    logical: binding.logical,
                    host: port,
                });
            }
            let revision = desired
                .deployments
                .get(&workload.value.deployment_id)
                .map(|deployment| deployment.value.revision.clone())
                .unwrap_or_default();
            let id = ids::instance(&workload.id, &workload.value.deployment_id);
            let instance = WorkloadInstanceRecord {
                environment_id: workload.value.environment_id.clone(),
                environment: key.0.clone(),
                project_id: workload.value.project_id.clone(),
                project: key.1.clone(),
                workload: key.2.clone(),
                workload_id: workload.id.clone(),
                deployment_id: workload.value.deployment_id.clone(),
                revision: revision.clone(),
                state: InstanceState::Serving,
                ports: ports.clone(),
                readiness: Some("adopted: running before zero-downtime releases".into()),
                started_at: Some(now),
                ready_at: Some(now),
                stopped_at: None,
                error: None,
                updated_at: now,
            };
            change = change.with(|batch| batch.create(&id, &instance));
            for (binding, port) in workload.value.ports.iter().zip(&ports) {
                let endpoint = ids::endpoint(&key.0, &key.1, &key.2, &binding.name);
                let assignment = TrafficAssignmentRecord {
                    endpoint: endpoint.clone(),
                    environment: key.0.clone(),
                    project: key.1.clone(),
                    workload: key.2.clone(),
                    port: binding.name.clone(),
                    host_port: binding.host,
                    domains: vec![],
                    deployment_id: workload.value.deployment_id.clone(),
                    revision: revision.clone(),
                    instance_id: id.clone(),
                    target_port: port.host,
                    status: "active".into(),
                    previous_deployment_id: None,
                    previous_instance_id: None,
                    switched_at: now,
                };
                change = match desired.traffic.get(&endpoint) {
                    Some(existing) => change.with(|batch| batch.replace(existing, &assignment)),
                    None => {
                        change.with(|batch| batch.create(&ids::traffic(&endpoint), &assignment))
                    }
                };
            }
        }
        if change.batch.is_empty() {
            return false;
        }
        match self.apply(change).await {
            Ok(()) => true,
            Err(error) => {
                self.inner.lock().await.state_error = Some(error.to_string());
                false
            }
        }
    }

    // ---- receipts --------------------------------------------------------------

    /// Write a deployment receipt: what was released, where, with which
    /// evidence. It holds digests and identifiers, never configuration
    /// values, keys, or credentials.
    async fn deployment_receipt(
        &self,
        deployment_id: &str,
        record: &DeploymentRecord,
    ) -> Result<String, EnvironmentError> {
        let transitions = self
            .events(super::EventFilter {
                deployment_id: Some(deployment_id.into()),
                limit: Some(500),
                ..Default::default()
            })
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|event| json!({ "sequence": event.sequence, "kind": event.kind, "at": event.at }))
            .collect::<Vec<_>>();
        let receipt = json!({
            "format": "compute.deployment-receipt@1",
            "deployment_id": deployment_id,
            "environment_id": record.environment_id,
            "environment": record.environment,
            "project_id": record.project_id,
            "project": record.project,
            "revision_id": record.revision_id,
            "revision": record.revision,
            "revision_digest": record.revision_digest,
            "old_revision": record.old_revision,
            "previous_deployment_id": record.previous,
            "promoted_from": record.promoted_from,
            "config_digest": record.config_digest,
            "status": record.status,
            "failure": record.failure,
            "rollback_reason": record.rollback_reason,
            "workloads": record.workloads,
            "readiness": record.readiness_result,
            "network": record.network_result,
            "traffic_switch": record.traffic_switch_result,
            "execution_receipts": record.receipt_ids,
            "events": transitions,
            "created_at": record.created_at,
            "completed_at": record.completed_at,
            "issued_by": {
                "compute": env!("CARGO_PKG_VERSION"),
                "daemon": self.instance_id,
            },
        });
        let bytes = serde_json::to_vec(&receipt)?;
        Ok(self
            .config
            .artifacts
            .put("deployment-receipt", &bytes)
            .await?)
    }
}

enum Check {
    Ready(String),
    Waiting(String),
    Failed(String),
}

/// Whether a workload's readiness proves it accepts connections.
fn listens(desired: &Desired, deployment_id: &str, workload: &str) -> bool {
    desired
        .revision_workload(deployment_id, workload)
        .map(|workload| {
            workload
                .readiness
                .clone()
                .unwrap_or_else(|| Readiness::default_for(&workload.ports))
        })
        .is_some_and(|readiness| {
            matches!(readiness.check, ReadinessCheck::Port | ReadinessCheck::Http)
        })
}

/// A workload's stable endpoints: the release's evidence, or, for a
/// deployment recorded before releases, the workload's current bindings.
fn resolved_endpoints(
    desired: &Desired,
    key: &Key,
    evidence: &DeploymentWorkload,
) -> Vec<PortBinding> {
    if !evidence.endpoints.is_empty() || evidence.kind != WorkloadKind::Service {
        return evidence.endpoints.clone();
    }
    desired
        .workloads
        .get(key)
        .map(|workload| workload.value.ports.clone())
        .unwrap_or_default()
}

/// `GET path` on a local port; the status code.
pub(crate) async fn http_status(port: u16, path: &str) -> Option<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let exchange = async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .ok()?;
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .ok()?;
        let mut head = [0u8; 64];
        let mut read = 0;
        while read < 12 {
            let count = stream.read(&mut head[read..]).await.ok()?;
            if count == 0 {
                break;
            }
            read += count;
        }
        let text = String::from_utf8_lossy(&head[..read]).into_owned();
        text.split(' ').nth(1)?.parse().ok()
    };
    tokio::time::timeout(Duration::from_secs(2), exchange)
        .await
        .ok()
        .flatten()
}
