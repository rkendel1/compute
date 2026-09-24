use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use compute_core::{
    EXECUTION_JOB_VERSION, ExecutionJob, ExecutionStatus, JobArtifact, JobArtifacts,
    JobCancellation, JobId, JobReceipt, JobRequest, JobRequestedPolicy, JobResult, JobStatus,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tempfile::NamedTempFile;
use tokio::sync::{Mutex, Semaphore};

use crate::{
    ComputeProvider, ProviderError, ProviderErrorKind, ProviderRequest, artifact_error,
    transport_error,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRequest {
    owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idempotency_key_hash: Option<String>,
    request: ProviderRequest,
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
    capacity: Arc<Semaphore>,
    mutation: Mutex<()>,
}

impl JobManager {
    pub fn new(
        root: PathBuf,
        retention: Duration,
        max_concurrent_jobs: usize,
        provider: Arc<dyn ComputeProvider>,
    ) -> Result<Arc<Self>, ProviderError> {
        fs::create_dir_all(&root).map_err(transport_error)?;
        let manager = Arc::new(Self {
            root,
            retention,
            provider,
            capacity: Arc::new(Semaphore::new(max_concurrent_jobs.max(1))),
            mutation: Mutex::new(()),
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
            match job.status {
                JobStatus::Accepted | JobStatus::Queued | JobStatus::Created => {
                    let manager = self.clone();
                    tokio::spawn(async move { manager.run(job_id).await });
                }
                JobStatus::Preparing | JobStatus::Running => {
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
                    job.updated_at = Utc::now();
                    self.write_job(&job)?;
                    self.append_event(&job.job_id, "terminal")?;
                }
                _ => {}
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
            status: JobStatus::Accepted,
            provider: self.provider.identity(),
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
                    event_type: "accepted".into(),
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
            status: JobStatus::Accepted,
        })
    }

    async fn run(self: Arc<Self>, job_id: JobId) {
        if self
            .transition(&job_id, JobStatus::Queued, None)
            .await
            .is_err()
        {
            return;
        }
        let Ok(permit) = self.capacity.clone().acquire_owned().await else {
            return;
        };
        let Ok(current) = self.read_job(&job_id) else {
            return;
        };
        if current.status.is_terminal() {
            return;
        }
        if self
            .transition(&job_id, JobStatus::Preparing, None)
            .await
            .is_err()
        {
            return;
        }
        let stored = match self.read_request(&job_id) {
            Ok(stored) => stored,
            Err(error) => {
                let _ = self
                    .transition(&job_id, JobStatus::Rejected, Some(error.to_string()))
                    .await;
                return;
            }
        };
        if self
            .transition(&job_id, JobStatus::Running, None)
            .await
            .is_err()
        {
            return;
        }
        let response = self.provider.execute(stored.request).await;
        drop(permit);
        match response {
            Ok(response) => {
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
        job.updated_at = Utc::now();
        self.write_job(&job)?;
        self.append_event(job_id, "terminal")
    }

    pub async fn status(&self, job_id: &JobId, owner: &str) -> Result<ExecutionJob, ProviderError> {
        self.authorize_owner(job_id, owner)?;
        self.require_not_expired(job_id)?;
        self.read_job(job_id)
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
            JobStatus::Created | JobStatus::Accepted | JobStatus::Queued => {
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
        self.write_job(&job)?;
        if job.status == JobStatus::Cancelled {
            self.append_event(job_id, "terminal")?;
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
            return Ok(());
        }
        job.status = status;
        job.failure = failure;
        job.updated_at = Utc::now();
        self.write_job(&job)?;
        self.append_event(
            job_id,
            match status {
                JobStatus::Queued => "queued",
                JobStatus::Preparing => "preparing",
                JobStatus::Running => "running",
                _ if status.is_terminal() => "terminal",
                _ => "accepted",
            },
        )
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
