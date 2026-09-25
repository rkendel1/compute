use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use compute_core::{
    CapacitySnapshot, CapacityWait, ComputeReservation, EXECUTION_JOB_VERSION, ExecutionJob,
    ExecutionStatus, JobArtifact, JobArtifacts, JobCancellation, JobId, JobReceipt, JobRequest,
    JobRequestedPolicy, JobResult, JobStatus, ProviderCapacity, ReceiptReservation, ReservationId,
    ReservationState, ResourceRequirements,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tempfile::NamedTempFile;
use tokio::sync::{Mutex, Notify};

use crate::{
    Admission, ComputeProvider, ProviderError, ProviderErrorKind, ProviderRequest, artifact_error,
    transport_error,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRequest {
    owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idempotency_key_hash: Option<String>,
    request: ProviderRequest,
    /// The admitted decision and the exact policy snapshot it was
    /// evaluated against. Execution uses this snapshot even if the server's
    /// policy changes later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    admission: Option<Admission>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExpiredMarker {
    owner: String,
    expired_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobEvent {
    pub job_id: JobId,
    pub sequence: u64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub timestamp: chrono::DateTime<Utc>,
}

pub struct JobManager {
    root: PathBuf,
    retention: Duration,
    provider: Arc<dyn ComputeProvider>,
    capacity: ProviderCapacity,
    mutation: Mutex<()>,
    wake: Notify,
}

impl JobManager {
    pub fn new(
        root: PathBuf,
        retention: Duration,
        capacity: ProviderCapacity,
        provider: Arc<dyn ComputeProvider>,
    ) -> Result<Arc<Self>, ProviderError> {
        fs::create_dir_all(&root).map_err(transport_error)?;
        let manager = Arc::new(Self {
            root,
            retention,
            provider,
            capacity,
            mutation: Mutex::new(()),
            wake: Notify::new(),
        });
        manager.recover()?;
        Ok(manager)
    }

    fn recover(self: &Arc<Self>) -> Result<(), ProviderError> {
        for entry in fs::read_dir(&self.root).map_err(transport_error)? {
            let entry = entry.map_err(transport_error)?;
            if !entry.file_type().map_err(transport_error)?.is_dir() {
                continue;
            }
            let Ok(job_id) = JobId::parse(entry.file_name().to_string_lossy().into_owned()) else {
                continue;
            };
            let Ok(mut job) = self.read_job(&job_id) else {
                continue;
            };
            let provider_id = job
                .provider_id
                .clone()
                .unwrap_or_else(|| match &job.provider {
                    compute_core::ProviderIdentity::Local { id }
                    | compute_core::ProviderIdentity::Remote { id, .. } => id.clone(),
                });
            if job.reservation.is_none() {
                job.reservation = Some(ComputeReservation {
                    reservation_id: ReservationId::generate(),
                    job_id: job_id.clone(),
                    provider_id,
                    resources: ResourceRequirements::from_limits(
                        &job.request.requested_execution.resources,
                    ),
                    state: ReservationState::Pending,
                    created_at: job.created_at,
                    expires_at: None,
                    reserved_at: None,
                    released_at: None,
                    capacity_snapshot: None,
                });
                job.capacity_wait = None;
                if matches!(
                    job.status,
                    JobStatus::Reserved | JobStatus::Admitted | JobStatus::Preparing
                ) {
                    job.status = JobStatus::Queued;
                }
                self.write_job(&job)?;
            }
            let reservation_invalid = job.reservation.as_ref().is_some_and(|reservation| {
                ReservationId::parse(reservation.reservation_id.0.clone()).is_err()
                    || reservation.job_id != job_id
                    || job
                        .provider_id
                        .as_ref()
                        .is_some_and(|provider_id| reservation.provider_id != *provider_id)
            });
            if reservation_invalid {
                job.status = JobStatus::Failed;
                job.failure = Some("reservation_invalid: job/provider binding mismatch".into());
                release_reservation(&mut job);
                job.updated_at = Utc::now();
                self.write_job(&job)?;
                self.append_event(&job_id, "terminal")?;
                continue;
            }
            match job.status {
                JobStatus::Accepted
                | JobStatus::Queued
                | JobStatus::WaitingForCapacity
                | JobStatus::Created
                | JobStatus::Reserved
                | JobStatus::Admitted
                | JobStatus::Preparing => {
                    let manager = self.clone();
                    tokio::spawn(async move { manager.run(job_id).await });
                }
                JobStatus::Running => {
                    let result_path = self.directory(&job_id).join("result.json");
                    if result_path.is_file() {
                        match self.read_json::<JobResult>(&result_path) {
                            Ok(result)
                                if result.status == terminal_status(&result.result)
                                    && result
                                        .result
                                        .receipt
                                        .as_ref()
                                        .is_some_and(|receipt| receipt.verify().is_ok()) =>
                            {
                                job.status = result.status;
                                job.execution_id = Some(result.result.execution_id.clone());
                                job.result_digest = Some(compute_core::sha256_identity(
                                    &serde_json::to_vec(&result).map_err(transport_error)?,
                                ));
                                job.failure = None;
                            }
                            _ => {
                                job.status = JobStatus::Failed;
                                job.failure = Some(
                                    "provider_interrupted: terminal evidence was incomplete or invalid"
                                        .into(),
                                );
                            }
                        }
                    } else {
                        job.status = JobStatus::Failed;
                        job.failure =
                            Some("provider_interrupted: server restarted during execution".into());
                    }
                    let released = release_reservation(&mut job);
                    job.updated_at = Utc::now();
                    self.write_job(&job)?;
                    self.append_event(&job.job_id, "terminal")?;
                    if released {
                        self.append_event(&job.job_id, "reservation_released")?;
                    }
                }
                _ => {
                    if release_reservation(&mut job) {
                        job.updated_at = Utc::now();
                        self.write_job(&job)?;
                        self.append_event(&job.job_id, "reservation_released")?;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn submit(
        self: &Arc<Self>,
        request: ProviderRequest,
        owner: String,
        idempotency_key: Option<&str>,
    ) -> Result<compute_core::JobSubmission, ProviderError> {
        if idempotency_key.is_some_and(|value| {
            value.is_empty()
                || value.len() > 256
                || value.bytes().any(|byte| byte.is_ascii_control())
        }) {
            return Err(ProviderError::new(
                ProviderErrorKind::IdempotencyConflict,
                "invalid idempotency key",
            ));
        }
        request.validate()?;
        let bundle = request.artifact.bundle()?;
        let verification = bundle.verification().map_err(artifact_error)?;
        bundle
            .require_ids(
                request.expected.workload_id.as_deref(),
                request.expected.bundle_id.as_deref(),
            )
            .map_err(artifact_error)?;
        let request_hash = request.request_hash()?;
        let key_hash = idempotency_key.map(|key| compute_core::sha256_identity(key.as_bytes()));
        let _guard = self.mutation.lock().await;
        if let Some(key_hash) = &key_hash
            && let Some(existing) = self.find_idempotent(&owner, key_hash)?
        {
            let stored = self.read_request(&existing)?;
            if stored.request.request_hash()? != request_hash {
                return Err(ProviderError::new(
                    ProviderErrorKind::IdempotencyConflict,
                    "idempotency key was already used for a different request",
                ));
            }
            let job = self.read_job(&existing)?;
            return Ok(compute_core::JobSubmission {
                job_id: existing,
                request_id: job.request.request_id,
                status: job.status,
            });
        }
        let job_id = JobId::generate();
        let directory = self.directory(&job_id);
        let dependency_id = bundle
            .dependency_capsule
            .as_ref()
            .map(|capsule| capsule.capsule_id())
            .transpose()
            .map_err(artifact_error)?;
        let request_id = request.execution.execution_request_id.clone();
        let requested_execution = JobRequestedPolicy {
            isolation: request
                .execution
                .isolation
                .unwrap_or(bundle.workload.isolation.profile),
            network: bundle.workload.network.clone(),
            resources: bundle.workload.resources.clone(),
        };
        let now = Utc::now();
        let provider_id = request
            .execution
            .placement
            .as_ref()
            .map(|placement| placement.provider_id.clone())
            .unwrap_or_else(|| match self.provider.identity() {
                compute_core::ProviderIdentity::Local { id }
                | compute_core::ProviderIdentity::Remote { id, .. } => id,
            });
        let reservation = ComputeReservation {
            reservation_id: ReservationId::generate(),
            job_id: job_id.clone(),
            provider_id: provider_id.clone(),
            resources: ResourceRequirements::from_limits(&bundle.workload.resources),
            state: ReservationState::Pending,
            created_at: now,
            expires_at: None,
            reserved_at: None,
            released_at: None,
            capacity_snapshot: None,
        };
        let job = ExecutionJob {
            version: EXECUTION_JOB_VERSION.into(),
            job_id: job_id.clone(),
            request: JobRequest {
                request_hash,
                request_id: request_id.clone(),
                workload_id: verification.workload_id,
                bundle_id: verification.bundle_id,
                dependency_id,
                runtime: bundle.workload.runtime,
                requested_execution,
            },
            status: JobStatus::Queued,
            provider: self.provider.identity(),
            placement_id: request
                .execution
                .placement
                .as_ref()
                .map(|placement| placement.placement_id.clone()),
            provider_id: Some(provider_id),
            admission: None,
            reservation: Some(reservation),
            capacity_wait: None,
            execution_id: None,
            result_digest: None,
            cancellation: JobCancellation::default(),
            failure: None,
            created_at: now,
            updated_at: now,
        };
        let staging = tempfile::Builder::new()
            .prefix(".compute-job-")
            .tempdir_in(&self.root)
            .map_err(transport_error)?;
        self.write_json(
            &staging.path().join("request.json"),
            &StoredRequest {
                owner,
                idempotency_key_hash: key_hash,
                request,
                admission: None,
            },
        )?;
        self.write_json(&staging.path().join("status.json"), &job)?;
        self.write_json(
            &staging.path().join("events.json"),
            &vec![
                JobEvent {
                    job_id: job_id.clone(),
                    sequence: 1,
                    event_type: "created".into(),
                    timestamp: now,
                },
                JobEvent {
                    job_id: job_id.clone(),
                    sequence: 2,
                    event_type: "queued".into(),
                    timestamp: Utc::now(),
                },
            ],
        )?;
        let staged_path = staging.keep();
        fs::rename(staged_path, &directory).map_err(transport_error)?;
        let manager = self.clone();
        let spawned_id = job_id.clone();
        tokio::spawn(async move { manager.run(spawned_id).await });
        Ok(compute_core::JobSubmission {
            job_id,
            request_id,
            status: JobStatus::Queued,
        })
    }

    async fn run(self: Arc<Self>, job_id: JobId) {
        loop {
            let Ok(current) = self.read_job(&job_id) else {
                return;
            };
            if current.status.is_terminal() {
                let _ = self.release(&job_id).await;
                return;
            }
            match self.try_reserve(&job_id).await {
                Ok(true) => break,
                Ok(false) => {
                    tokio::select! {
                        _ = self.wake.notified() => {},
                        _ = tokio::time::sleep(Duration::from_millis(250)) => {},
                    }
                }
                Err(error) => {
                    let _ = self
                        .transition(&job_id, JobStatus::Rejected, Some(error.to_string()))
                        .await;
                    return;
                }
            }
        }

        let mut stored = match self.read_request(&job_id) {
            Ok(stored) => stored,
            Err(error) => {
                let _ = self
                    .transition(&job_id, JobStatus::Rejected, Some(error.to_string()))
                    .await;
                return;
            }
        };
        let admission = match stored.admission.clone() {
            Some(admission) if admission.decision.admitted => {
                if self
                    .read_job(&job_id)
                    .is_ok_and(|job| job.admission.is_none())
                    && self
                        .persist_admission(&job_id, &mut stored, &admission)
                        .await
                        .is_err()
                {
                    let _ = self
                        .transition(
                            &job_id,
                            JobStatus::Rejected,
                            Some("admission reconciliation failed".into()),
                        )
                        .await;
                    return;
                }
                admission
            }
            _ => match self.provider.admit(stored.request.clone()).await {
                Ok(admission) if admission.decision.admitted => {
                    if self
                        .persist_admission(&job_id, &mut stored, &admission)
                        .await
                        .is_err()
                    {
                        let _ = self
                            .transition(
                                &job_id,
                                JobStatus::Rejected,
                                Some("admission persistence failed".into()),
                            )
                            .await;
                        return;
                    }
                    admission
                }
                Ok(admission) => {
                    let _ = self
                        .persist_admission(&job_id, &mut stored, &admission)
                        .await;
                    let _ = self
                        .transition(
                            &job_id,
                            JobStatus::Rejected,
                            Some(format!(
                                "admission_denied: {}",
                                admission
                                    .decision
                                    .reasons
                                    .iter()
                                    .map(|reason| reason.message.as_str())
                                    .collect::<Vec<_>>()
                                    .join("; ")
                            )),
                        )
                        .await;
                    return;
                }
                Err(error) => {
                    let _ = self
                        .transition(&job_id, JobStatus::Rejected, Some(error.to_string()))
                        .await;
                    return;
                }
            },
        };
        if self
            .transition(&job_id, JobStatus::Preparing, None)
            .await
            .is_err()
            || self
                .transition(&job_id, JobStatus::Running, None)
                .await
                .is_err()
        {
            let _ = self.release(&job_id).await;
            return;
        }
        let execution_request = stored.request;
        let reservation_evidence = self.read_job(&job_id).ok().and_then(|job| {
            let reservation = job.reservation?;
            Some(ReceiptReservation {
                reservation_id: reservation.reservation_id,
                requested_resources: reservation.resources.clone(),
                reserved_resources: reservation.resources,
                provider_capacity_snapshot: reservation.capacity_snapshot?,
            })
        });
        let Some(reservation_evidence) = reservation_evidence else {
            let _ = self
                .transition(
                    &job_id,
                    JobStatus::Failed,
                    Some("reserved job omitted its capacity snapshot".into()),
                )
                .await;
            return;
        };
        let response = self
            .provider
            .execute_admitted(execution_request, admission)
            .await;
        match response {
            Ok(mut response) => {
                if let Some(receipt) = response.result.receipt.as_mut() {
                    receipt.reservation = Some(reservation_evidence.clone());
                    if let Some(placement) = receipt.placement.as_mut() {
                        placement.reservation = Some(reservation_evidence);
                    }
                    if let Err(error) = receipt.seal() {
                        let _ = self
                            .transition(&job_id, JobStatus::Failed, Some(error.to_string()))
                            .await;
                        return;
                    }
                }
                let status = terminal_status(&response.result);
                if let Err(error) = self.persist_result(&job_id, status, response.result).await {
                    let _ = self
                        .transition(&job_id, JobStatus::Failed, Some(error.to_string()))
                        .await;
                }
            }
            Err(error) => {
                let _ = self
                    .transition(&job_id, JobStatus::Rejected, Some(error.to_string()))
                    .await;
            }
        }
    }

    async fn try_reserve(&self, job_id: &JobId) -> Result<bool, ProviderError> {
        let _guard = self.mutation.lock().await;
        let mut job = self.read_job(job_id)?;
        if job.status.is_terminal() {
            return Ok(false);
        }
        let reservation = job
            .reservation
            .as_ref()
            .ok_or_else(|| evidence_error("durable job has no capacity reservation"))?;
        if reservation.job_id != *job_id {
            return Err(evidence_error("reservation belongs to another job"));
        }
        if reservation.state == ReservationState::Reserved {
            return Ok(true);
        }
        if reservation.state.is_terminal() {
            return Err(evidence_error("terminal reservation cannot be reacquired"));
        }
        if reservation
            .expires_at
            .is_some_and(|expires_at| expires_at <= Utc::now())
        {
            let reservation = job.reservation.as_mut().expect("checked");
            reservation.state = ReservationState::Expired;
            reservation.released_at = Some(Utc::now());
            job.status = JobStatus::Rejected;
            job.failure = Some("reservation_expired".into());
            job.updated_at = Utc::now();
            self.write_job(&job)?;
            self.append_event(job_id, "terminal")?;
            self.wake.notify_waiters();
            return Ok(false);
        }

        let before = self.capacity_snapshot_locked()?;
        let required = reservation.resources.clone();
        let total = ResourceRequirements {
            cpu_millis: self.capacity.cpu_millis,
            memory_bytes: self.capacity.memory_bytes,
            disk_bytes: self.capacity.disk_bytes,
            concurrency: self.capacity.max_concurrency,
        };
        let impossible = insufficient_reasons(&required, &total);
        if !impossible.is_empty() {
            job.capacity_wait = Some(CapacityWait {
                required: required.clone(),
                available: total.clone(),
                reasons: impossible.clone(),
            });
            job.status = JobStatus::Rejected;
            job.failure = Some(format!(
                "permanent_capacity_insufficient: {}",
                impossible.join(",")
            ));
            let released = release_reservation(&mut job);
            job.updated_at = Utc::now();
            self.write_job(&job)?;
            self.append_event(job_id, "terminal")?;
            if released {
                self.append_event(job_id, "reservation_released")?;
            }
            self.wake.notify_waiters();
            return Ok(false);
        }
        let reasons = insufficient_reasons(&required, &before.available);
        if !reasons.is_empty() {
            let changed = job.status != JobStatus::WaitingForCapacity
                || job.capacity_wait.as_ref().is_none_or(|waiting| {
                    waiting.required != required
                        || waiting.available != before.available
                        || waiting.reasons != reasons
                });
            job.status = JobStatus::WaitingForCapacity;
            job.capacity_wait = Some(CapacityWait {
                required,
                available: before.available,
                reasons,
            });
            job.updated_at = Utc::now();
            self.write_job(&job)?;
            if changed {
                self.append_event(job_id, "waiting_for_capacity")?;
            }
            return Ok(false);
        }

        let reserved = before
            .reserved
            .checked_add(&required)
            .ok_or_else(|| evidence_error("reserved capacity overflow"))?;
        let after = capacity_snapshot(&self.capacity, reserved);
        let reservation = job.reservation.as_mut().expect("checked");
        reservation.state = ReservationState::Reserved;
        reservation.reserved_at = Some(Utc::now());
        reservation.capacity_snapshot = Some(after);
        job.status = JobStatus::Reserved;
        job.capacity_wait = None;
        job.failure = None;
        job.updated_at = Utc::now();
        self.write_job(&job)?;
        self.append_event(job_id, "reserved")?;
        Ok(true)
    }

    async fn persist_admission(
        &self,
        job_id: &JobId,
        stored: &mut StoredRequest,
        admission: &Admission,
    ) -> Result<(), ProviderError> {
        let _guard = self.mutation.lock().await;
        let mut job = self.read_job(job_id)?;
        if job.status.is_terminal()
            || !job
                .reservation
                .as_ref()
                .is_some_and(|reservation| reservation.state == ReservationState::Reserved)
        {
            return Err(evidence_error(
                "job lost its reservation before admission was persisted",
            ));
        }
        stored.admission = Some(admission.clone());
        self.write_json(&self.directory(job_id).join("request.json"), stored)?;
        job.admission = Some(admission.summary());
        if admission.decision.admitted {
            job.status = JobStatus::Admitted;
        }
        job.updated_at = Utc::now();
        self.write_job(&job)?;
        self.append_event(
            job_id,
            if admission.decision.admitted {
                "admitted"
            } else {
                "admission_denied"
            },
        )
    }

    async fn release(&self, job_id: &JobId) -> Result<(), ProviderError> {
        let _guard = self.mutation.lock().await;
        let mut job = self.read_job(job_id)?;
        let Some(reservation) = job.reservation.as_mut() else {
            return Ok(());
        };
        if reservation.state.is_terminal() {
            return Ok(());
        }
        reservation.state = ReservationState::Released;
        reservation.released_at = Some(Utc::now());
        job.updated_at = Utc::now();
        self.write_job(&job)?;
        self.append_event(job_id, "reservation_released")?;
        drop(_guard);
        self.wake.notify_waiters();
        Ok(())
    }

    pub async fn capacity_snapshot(&self) -> Result<CapacitySnapshot, ProviderError> {
        let _guard = self.mutation.lock().await;
        self.capacity_snapshot_locked()
    }

    fn capacity_snapshot_locked(&self) -> Result<CapacitySnapshot, ProviderError> {
        let mut reserved = ResourceRequirements::default();
        for entry in fs::read_dir(&self.root).map_err(transport_error)? {
            let entry = entry.map_err(transport_error)?;
            if !entry.file_type().map_err(transport_error)?.is_dir() {
                continue;
            }
            let Ok(job_id) = JobId::parse(entry.file_name().to_string_lossy().into_owned()) else {
                continue;
            };
            let Ok(job) = self.read_job(&job_id) else {
                continue;
            };
            if let Some(reservation) = job
                .reservation
                .filter(|reservation| reservation.state.is_active())
            {
                reserved = reserved
                    .checked_add(&reservation.resources)
                    .ok_or_else(|| evidence_error("reserved capacity overflow"))?;
            }
        }
        Ok(capacity_snapshot(&self.capacity, reserved))
    }

    async fn persist_result(
        &self,
        job_id: &JobId,
        status: JobStatus,
        result: compute_core::ExecutionResult,
    ) -> Result<(), ProviderError> {
        let _guard = self.mutation.lock().await;
        let directory = self.directory(job_id);
        let receipt = result.receipt.clone().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::EvidenceInvalid,
                "job result omitted its receipt",
            )
        })?;
        receipt.verify().map_err(evidence_error)?;
        self.write_json(
            &directory.join("result.json"),
            &JobResult {
                job_id: job_id.clone(),
                status,
                result: result.clone(),
            },
        )?;
        self.write_json(
            &directory.join("receipt.json"),
            &JobReceipt {
                job_id: job_id.clone(),
                receipt,
            },
        )?;
        let artifact_directory = directory.join("artifacts");
        fs::create_dir_all(&artifact_directory).map_err(transport_error)?;
        for output in &result.outputs {
            let digest = compute_core::sha256_identity(&output.data);
            self.write_bytes(
                &artifact_directory.join(digest.trim_start_matches("sha256:")),
                &output.data,
            )?;
        }
        let mut job = self.read_job(job_id)?;
        job.execution_id = Some(result.execution_id.clone());
        job.result_digest = Some(compute_core::sha256_identity(
            &serde_json::to_vec(&JobResult {
                job_id: job_id.clone(),
                status,
                result: result.clone(),
            })
            .map_err(transport_error)?,
        ));
        job.status = status;
        let released = release_reservation(&mut job);
        job.updated_at = Utc::now();
        self.write_job(&job)?;
        self.append_event(job_id, "terminal")?;
        if released {
            self.append_event(job_id, "reservation_released")?;
            self.wake.notify_waiters();
        }
        Ok(())
    }

    pub async fn status(&self, job_id: &JobId, owner: &str) -> Result<ExecutionJob, ProviderError> {
        self.authorize_owner(job_id, owner)?;
        self.require_not_expired(job_id)?;
        self.read_job(job_id)
    }

    pub async fn list(&self, owner: &str) -> Result<Vec<ExecutionJob>, ProviderError> {
        let _guard = self.mutation.lock().await;
        let mut jobs = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(transport_error)? {
            let entry = entry.map_err(transport_error)?;
            if !entry.file_type().map_err(transport_error)?.is_dir() {
                continue;
            }
            let Ok(job_id) = JobId::parse(entry.file_name().to_string_lossy().into_owned()) else {
                continue;
            };
            let Ok(stored) = self.read_request(&job_id) else {
                continue;
            };
            if stored.owner == owner
                && let Ok(job) = self.read_job(&job_id)
            {
                jobs.push(job);
            }
        }
        jobs.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.job_id.cmp(&right.job_id))
        });
        Ok(jobs)
    }

    pub async fn result(&self, job_id: &JobId, owner: &str) -> Result<JobResult, ProviderError> {
        self.authorize_owner(job_id, owner)?;
        self.require_not_expired(job_id)?;
        let path = self.directory(job_id).join("result.json");
        if !path.is_file() {
            let job = self.read_job(job_id)?;
            return Err(ProviderError::new(
                ProviderErrorKind::RemoteExecutionFailure,
                if job.status.is_terminal() {
                    "terminal job has no execution result"
                } else {
                    "job result is not available before terminal state"
                },
            ));
        }
        let result: JobResult = self.read_json(&path)?;
        self.verify_result(&result)?;
        Ok(result)
    }

    pub async fn receipt(&self, job_id: &JobId, owner: &str) -> Result<JobReceipt, ProviderError> {
        self.authorize_owner(job_id, owner)?;
        self.require_not_expired(job_id)?;
        let path = self.directory(job_id).join("receipt.json");
        if !path.is_file() {
            return Err(ProviderError::new(
                ProviderErrorKind::RemoteExecutionFailure,
                "job receipt is not available",
            ));
        }
        let receipt: JobReceipt = self.read_json(&path)?;
        receipt.receipt.verify().map_err(evidence_error)?;
        Ok(receipt)
    }

    pub async fn artifacts(
        &self,
        job_id: &JobId,
        owner: &str,
    ) -> Result<JobArtifacts, ProviderError> {
        let result = self.result(job_id, owner).await?;
        let receipt = result.result.receipt.as_ref().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::EvidenceInvalid,
                "job result omitted receipt",
            )
        })?;
        let mut artifacts = Vec::new();
        for output in &receipt.outputs {
            let Some(digest) = &output.sha256 else {
                continue;
            };
            let data = fs::read(
                self.directory(job_id)
                    .join("artifacts")
                    .join(digest.trim_start_matches("sha256:")),
            )
            .map_err(evidence_error)?;
            let artifact = JobArtifact {
                digest: digest.clone(),
                size: data.len() as u64,
                data,
            };
            if !artifact.verify() {
                return Err(evidence_error("artifact digest mismatch"));
            }
            artifacts.push(artifact);
        }
        artifacts.sort_by(|left, right| left.digest.cmp(&right.digest));
        artifacts.dedup_by(|left, right| left.digest == right.digest);
        Ok(JobArtifacts {
            job_id: job_id.clone(),
            artifacts,
        })
    }

    pub async fn cancel(&self, job_id: &JobId, owner: &str) -> Result<ExecutionJob, ProviderError> {
        self.authorize_owner(job_id, owner)?;
        self.require_not_expired(job_id)?;
        let _guard = self.mutation.lock().await;
        let mut job = self.read_job(job_id)?;
        job.cancellation.requested = true;
        match job.status {
            JobStatus::Created
            | JobStatus::Accepted
            | JobStatus::Queued
            | JobStatus::WaitingForCapacity
            | JobStatus::Reserved
            | JobStatus::Admitted => {
                job.status = JobStatus::Cancelled;
                job.cancellation.effective = true;
                job.cancellation.phase = Some("before_execution".into());
            }
            JobStatus::Preparing | JobStatus::Running => {
                job.cancellation.effective = false;
                job.cancellation.phase = Some("execution_not_interruptible".into());
            }
            _ => {
                job.cancellation.effective = false;
                job.cancellation.phase = Some("already_terminal".into());
            }
        }
        job.updated_at = Utc::now();
        let released = if job.status == JobStatus::Cancelled {
            release_reservation(&mut job)
        } else {
            false
        };
        self.write_job(&job)?;
        if job.status == JobStatus::Cancelled {
            self.append_event(job_id, "terminal")?;
        }
        if released {
            self.append_event(job_id, "reservation_released")?;
            self.wake.notify_waiters();
        }
        Ok(job)
    }

    pub async fn events(
        &self,
        job_id: &JobId,
        owner: &str,
    ) -> Result<Vec<JobEvent>, ProviderError> {
        self.authorize_owner(job_id, owner)?;
        self.read_events(job_id)
    }

    async fn transition(
        &self,
        job_id: &JobId,
        status: JobStatus,
        failure: Option<String>,
    ) -> Result<(), ProviderError> {
        let _guard = self.mutation.lock().await;
        let mut job = self.read_job(job_id)?;
        if job.status.is_terminal() {
            return Err(evidence_error(format!(
                "cannot transition terminal job from {:?} to {:?}",
                job.status, status
            )));
        }
        if !valid_transition(job.status, status) {
            return Err(evidence_error(format!(
                "invalid job transition from {:?} to {:?}",
                job.status, status
            )));
        }
        job.status = status;
        job.failure = failure;
        let released = status.is_terminal() && release_reservation(&mut job);
        job.updated_at = Utc::now();
        self.write_job(&job)?;
        self.append_event(
            job_id,
            match status {
                JobStatus::Queued => "queued",
                JobStatus::WaitingForCapacity => "waiting_for_capacity",
                JobStatus::Reserved => "reserved",
                JobStatus::Admitted => "admitted",
                JobStatus::Preparing => "preparing",
                JobStatus::Running => "running",
                _ if status.is_terminal() => "terminal",
                _ => "accepted",
            },
        )?;
        if released {
            self.append_event(job_id, "reservation_released")?;
            self.wake.notify_waiters();
        }
        Ok(())
    }

    fn verify_result(&self, job_result: &JobResult) -> Result<(), ProviderError> {
        let job = self.read_job(&job_result.job_id)?;
        let actual_digest =
            compute_core::sha256_identity(&serde_json::to_vec(job_result).map_err(evidence_error)?);
        if job.result_digest.as_deref() != Some(&actual_digest) {
            return Err(evidence_error("job result digest mismatch"));
        }
        let receipt = job_result.result.receipt.as_ref().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::EvidenceInvalid,
                "job result omitted receipt",
            )
        })?;
        receipt.verify().map_err(evidence_error)?;
        if receipt.execution_id.0 != job_result.result.execution_id
            || receipt.execution.status != job_result.result.status
            || receipt.execution.exit_code != job_result.result.exit_code
        {
            return Err(evidence_error("job result and receipt disagree"));
        }
        let expected = terminal_status(&job_result.result);
        if job_result.status != expected {
            return Err(evidence_error("job status and execution result disagree"));
        }
        Ok(())
    }

    fn authorize_owner(&self, job_id: &JobId, owner: &str) -> Result<(), ProviderError> {
        let directory = self.directory(job_id);
        if directory.join("expired.json").is_file() {
            let marker: ExpiredMarker = self.read_json(&directory.join("expired.json"))?;
            if marker.owner != owner {
                return Err(ProviderError::new(
                    ProviderErrorKind::Unauthorized,
                    "job belongs to another principal",
                ));
            }
            return Ok(());
        }
        let stored = self.read_request(job_id)?;
        if stored.owner != owner {
            return Err(ProviderError::new(
                ProviderErrorKind::Unauthorized,
                "job belongs to another principal",
            ));
        }
        let job = self.read_job(job_id)?;
        if stored.request.request_hash()? != job.request.request_hash {
            return Err(evidence_error("stored job request was modified"));
        }
        Ok(())
    }

    fn require_not_expired(&self, job_id: &JobId) -> Result<(), ProviderError> {
        let directory = self.directory(job_id);
        if directory.join("expired.json").is_file() {
            return Err(ProviderError::new(
                ProviderErrorKind::JobExpired,
                "job evidence has expired",
            ));
        }
        let job = self.read_job(job_id)?;
        if job.status.is_terminal() && self.retention != Duration::ZERO {
            let age = Utc::now()
                .signed_duration_since(job.updated_at)
                .to_std()
                .unwrap_or_default();
            if age > self.retention {
                let stored = self.read_request(job_id)?;
                self.write_json(
                    &directory.join("expired.json"),
                    &ExpiredMarker {
                        owner: stored.owner,
                        expired_at: Utc::now(),
                    },
                )?;
                for path in ["request.json", "result.json", "receipt.json"] {
                    let _ = fs::remove_file(directory.join(path));
                }
                let _ = fs::remove_dir_all(directory.join("artifacts"));
                return Err(ProviderError::new(
                    ProviderErrorKind::JobExpired,
                    "job evidence has expired",
                ));
            }
        }
        Ok(())
    }

    fn find_idempotent(&self, owner: &str, key_hash: &str) -> Result<Option<JobId>, ProviderError> {
        for entry in fs::read_dir(&self.root).map_err(transport_error)? {
            let entry = entry.map_err(transport_error)?;
            if !entry.file_type().map_err(transport_error)?.is_dir() {
                continue;
            }
            let Ok(job_id) = JobId::parse(entry.file_name().to_string_lossy().into_owned()) else {
                continue;
            };
            if let Ok(stored) = self.read_request(&job_id)
                && stored.owner == owner
                && stored.idempotency_key_hash.as_deref() == Some(key_hash)
            {
                return Ok(Some(job_id));
            }
        }
        Ok(None)
    }

    fn directory(&self, job_id: &JobId) -> PathBuf {
        self.root.join(&job_id.0)
    }
    fn read_job(&self, job_id: &JobId) -> Result<ExecutionJob, ProviderError> {
        self.read_json(&self.directory(job_id).join("status.json"))
    }
    fn write_job(&self, job: &ExecutionJob) -> Result<(), ProviderError> {
        self.write_json(&self.directory(&job.job_id).join("status.json"), job)
    }
    fn read_request(&self, job_id: &JobId) -> Result<StoredRequest, ProviderError> {
        self.read_json(&self.directory(job_id).join("request.json"))
    }
    fn read_events(&self, job_id: &JobId) -> Result<Vec<JobEvent>, ProviderError> {
        let path = self.directory(job_id).join("events.json");
        if !path.is_file() {
            return Ok(vec![]);
        }
        self.read_json(&path)
    }
    fn append_event(&self, job_id: &JobId, event_type: &str) -> Result<(), ProviderError> {
        let mut events = self.read_events(job_id)?;
        events.push(JobEvent {
            job_id: job_id.clone(),
            sequence: events.len() as u64 + 1,
            event_type: event_type.into(),
            timestamp: Utc::now(),
        });
        self.write_json(&self.directory(job_id).join("events.json"), &events)
    }
    fn read_json<T: DeserializeOwned>(&self, path: &Path) -> Result<T, ProviderError> {
        let bytes = fs::read(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ProviderError::new(ProviderErrorKind::UnknownJob, "unknown job")
            } else {
                transport_error(error)
            }
        })?;
        serde_json::from_slice(&bytes).map_err(evidence_error)
    }
    fn write_json(&self, path: &Path, value: &impl Serialize) -> Result<(), ProviderError> {
        self.write_bytes(
            path,
            &serde_json::to_vec_pretty(value).map_err(transport_error)?,
        )
    }
    fn write_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), ProviderError> {
        let parent = path
            .parent()
            .ok_or_else(|| transport_error("job path has no parent"))?;
        fs::create_dir_all(parent).map_err(transport_error)?;
        let mut temporary = NamedTempFile::new_in(parent).map_err(transport_error)?;
        temporary.write_all(bytes).map_err(transport_error)?;
        temporary.as_file().sync_all().map_err(transport_error)?;
        temporary
            .persist(path)
            .map_err(|error| transport_error(error.error))?;
        Ok(())
    }
}

fn evidence_error(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ProviderErrorKind::EvidenceInvalid, error.to_string())
}

fn terminal_status(result: &compute_core::ExecutionResult) -> JobStatus {
    match result.status {
        ExecutionStatus::TimedOut => JobStatus::TimedOut,
        ExecutionStatus::Completed if result.exit_code == Some(0) => JobStatus::Succeeded,
        ExecutionStatus::Completed | ExecutionStatus::Failed => JobStatus::Failed,
        ExecutionStatus::Cancelled | ExecutionStatus::Killed => JobStatus::Cancelled,
        _ => JobStatus::Failed,
    }
}

fn capacity_snapshot(
    capacity: &ProviderCapacity,
    reserved: ResourceRequirements,
) -> CapacitySnapshot {
    CapacitySnapshot {
        capacity: capacity.clone(),
        available: ResourceRequirements {
            cpu_millis: capacity.cpu_millis.saturating_sub(reserved.cpu_millis),
            memory_bytes: capacity.memory_bytes.saturating_sub(reserved.memory_bytes),
            disk_bytes: capacity.disk_bytes.saturating_sub(reserved.disk_bytes),
            concurrency: capacity
                .max_concurrency
                .saturating_sub(reserved.concurrency),
        },
        reserved,
    }
}

fn insufficient_reasons(
    required: &ResourceRequirements,
    available: &ResourceRequirements,
) -> Vec<String> {
    let mut reasons = vec![];
    if required.cpu_millis > available.cpu_millis {
        reasons.push("insufficient_cpu".into());
    }
    if required.memory_bytes > available.memory_bytes {
        reasons.push("insufficient_memory".into());
    }
    if required.disk_bytes > available.disk_bytes {
        reasons.push("insufficient_disk".into());
    }
    if required.concurrency > available.concurrency {
        reasons.push("insufficient_concurrency".into());
    }
    reasons
}

fn release_reservation(job: &mut ExecutionJob) -> bool {
    let Some(reservation) = job.reservation.as_mut() else {
        return false;
    };
    if reservation.state.is_terminal() {
        return false;
    }
    reservation.state = ReservationState::Released;
    reservation.released_at = Some(Utc::now());
    true
}

fn valid_transition(from: JobStatus, to: JobStatus) -> bool {
    if to.is_terminal() {
        return true;
    }
    matches!(
        (from, to),
        (JobStatus::Created | JobStatus::Accepted, JobStatus::Queued)
            | (
                JobStatus::Queued,
                JobStatus::WaitingForCapacity | JobStatus::Reserved
            )
            | (
                JobStatus::WaitingForCapacity,
                JobStatus::WaitingForCapacity | JobStatus::Reserved
            )
            | (
                JobStatus::Reserved,
                JobStatus::Admitted | JobStatus::Preparing
            )
            | (JobStatus::Admitted, JobStatus::Preparing)
            | (JobStatus::Preparing, JobStatus::Running)
    )
}
