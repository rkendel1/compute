//! Provider-pool and placement certification for `compute certify`.
//!
//! Proves, against the assembled distribution, that
//! requirements → capability discovery → compatibility → deterministic
//! selection → execution → receipt holds for every runtime, and that
//! incompatibility, invalid or stale capabilities, and provider loss lead to
//! no execution and no redirection.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::Utc;
use compute_core::{
    IsolationProfile, ProviderIdentity, RuntimeKind, SelectionMode, WorkloadBundle,
};
use compute_placement::{
    AdmissionContext, CapabilityCache, DiscoveryMode, DiscoveryRecord, DiscoveryStatus,
    DispatchErrorCode, EvaluationStatus, PlacementOutcome, PlacementReport, PlacementRequirements,
    PoolPolicy, ProviderConfig, ProviderKind, ProviderPool, ReasonCode, RequirementOptions,
    SubmissionMode, dispatch, place,
};
use compute_provider::{
    Admission, ComputeProvider, ExecuteResponse, InspectResponse, LocalProvider,
    ProviderCapabilities, ProviderError, ProviderHealth, ProviderPolicy, ProviderRequest,
    RemoteProvider, ServerConfig,
};

/// The local provider, counting executions so certification can prove that
/// a failed placement ran nothing.
struct CountingLocal {
    inner: LocalProvider,
    executions: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ComputeProvider for CountingLocal {
    fn identity(&self) -> ProviderIdentity {
        self.inner.identity()
    }
    async fn inspect(&self, request: ProviderRequest) -> Result<InspectResponse, ProviderError> {
        self.inner.inspect(request).await
    }
    async fn execute(&self, request: ProviderRequest) -> Result<ExecuteResponse, ProviderError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(request).await
    }
    async fn capabilities(&self) -> Result<ProviderCapabilities, ProviderError> {
        self.inner.capabilities().await
    }
    async fn health(&self) -> Result<ProviderHealth, ProviderError> {
        self.inner.health().await
    }
    async fn admit(&self, request: ProviderRequest) -> Result<Admission, ProviderError> {
        self.inner.admit(request).await
    }
    async fn execute_admitted(
        &self,
        request: ProviderRequest,
        admission: Admission,
    ) -> Result<ExecuteResponse, ProviderError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.inner.execute_admitted(request, admission).await
    }
}

async fn spawn_server(
    policy: ProviderPolicy,
) -> Result<(String, tokio::task::JoinHandle<()>, tempfile::TempDir), String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| error.to_string())?;
    let endpoint = format!(
        "http://{}",
        listener.local_addr().map_err(|error| error.to_string())?
    );
    let jobs = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut config = ServerConfig::local_with_policy(endpoint.clone(), policy);
    config.job_store = jobs.path().to_path_buf();
    let handle = tokio::spawn(async move {
        let _ = compute_provider::serve_listener(listener, config).await;
    });
    Ok((endpoint, handle, jobs))
}

fn remote_config(endpoint: &str, priority: i64) -> ProviderConfig {
    ProviderConfig {
        kind: ProviderKind::Remote,
        endpoint: Some(endpoint.into()),
        priority,
        token_env: None,
    }
}

/// A three-provider pool with intentionally different capabilities:
/// `restricted` (priority 100) offers only WASM under process isolation,
/// `remote` (priority 50) offers everything, `local` (priority 10) offers
/// everything but durable jobs.
pub struct PlacementHarness {
    pool: ProviderPool,
    local_executions: Arc<AtomicUsize>,
    distribution_id: String,
    restricted: tokio::task::JoinHandle<()>,
    _restricted_jobs: tempfile::TempDir,
}

impl Drop for PlacementHarness {
    fn drop(&mut self) {
        self.restricted.abort();
    }
}

impl PlacementHarness {
    pub async fn start(remote_endpoint: &str, distribution_id: String) -> Result<Self, String> {
        let (restricted_endpoint, restricted, restricted_jobs) = spawn_server(ProviderPolicy {
            runtimes: Some([RuntimeKind::Wasm].into_iter().collect()),
            isolation_profiles: Some([IsolationProfile::Process].into_iter().collect()),
            ..ProviderPolicy::default()
        })
        .await?;
        let local_executions = Arc::new(AtomicUsize::new(0));
        let mut pool = ProviderPool::new(PoolPolicy::default());
        let add = |error: compute_placement::PlacementError| error.to_string();
        pool.add(
            "local",
            ProviderConfig {
                kind: ProviderKind::Local,
                endpoint: None,
                priority: 10,
                token_env: None,
            },
            Arc::new(CountingLocal {
                inner: LocalProvider::new(),
                executions: local_executions.clone(),
            }),
        )
        .map_err(add)?;
        pool.add_remote(
            "remote",
            remote_config(remote_endpoint, 50),
            Arc::new(RemoteProvider::new(remote_endpoint)),
        )
        .map_err(add)?;
        pool.add_remote(
            "restricted",
            remote_config(&restricted_endpoint, 100),
            Arc::new(RemoteProvider::new(restricted_endpoint.clone())),
        )
        .map_err(add)?;
        Ok(Self {
            pool,
            local_executions,
            distribution_id,
            restricted,
            _restricted_jobs: restricted_jobs,
        })
    }

    async fn discover(&self, only: Option<&str>) -> Vec<DiscoveryRecord> {
        let mut cache = CapabilityCache::default();
        self.pool
            .capabilities(&mut cache, DiscoveryMode::Refresh, only, Utc::now())
            .await
    }

    fn requirements(
        &self,
        bundle: &WorkloadBundle,
        submission: SubmissionMode,
    ) -> Result<(ProviderRequest, PlacementRequirements, AdmissionContext), String> {
        let mut request =
            ProviderRequest::bundle(bundle.to_bytes().map_err(|error| error.to_string())?);
        request.expected.workload_id = Some(bundle.workload_id().map_err(|e| e.to_string())?);
        request.expected.bundle_id = Some(bundle.bundle_id().map_err(|e| e.to_string())?);
        let size = serde_json::to_vec(&request)
            .map_err(|error| error.to_string())?
            .len() as u64;
        let requirements = PlacementRequirements::from_bundle(
            bundle,
            size,
            submission,
            &RequirementOptions {
                distribution_id: Some(self.distribution_id.clone()),
                ..RequirementOptions::default()
            },
        )
        .map_err(|error| error.to_string())?;
        let contract = compute_policy::ExecutionContract::from_bundle(bundle, None)
            .map_err(|error| error.to_string())?;
        Ok((request, requirements, AdmissionContext::new(&[], contract)))
    }

    fn place(
        &self,
        records: &[DiscoveryRecord],
        requirements: &PlacementRequirements,
        admission: &AdmissionContext,
        explicit: Option<&str>,
    ) -> PlacementReport {
        place(
            &self.pool.configs(),
            self.pool.policy(),
            records,
            requirements,
            admission,
            explicit,
        )
    }

    /// Positive placement for one runtime's certified bundle: the correct
    /// providers are compatible, selection is deterministic, execution
    /// happens on the selected provider, and the receipt proves it.
    pub async fn certify_runtime(
        &self,
        kind: RuntimeKind,
        bundle_path: &Path,
    ) -> Result<(), String> {
        let bundle = WorkloadBundle::read(bundle_path).map_err(|error| error.to_string())?;
        let (request, requirements, admission) =
            self.requirements(&bundle, SubmissionMode::Synchronous)?;
        let records = self.discover(None).await;
        if let Some(record) = records
            .iter()
            .find(|record| record.status != DiscoveryStatus::Discovered)
        {
            return Err(format!(
                "provider {} capability discovery failed: {:?}",
                record.provider_id, record.error
            ));
        }
        let report = self.place(&records, &requirements, &admission, None);
        let expected: &[&str] = if kind == RuntimeKind::Wasm {
            &["restricted", "remote", "local"]
        } else {
            &["remote", "local"]
        };
        if report.compatible_providers != expected {
            return Err(format!(
                "compatible providers {:?}, expected {expected:?}: {:?}",
                report.compatible_providers, report.explanation.considered
            ));
        }
        if kind != RuntimeKind::Wasm {
            let restricted = report
                .providers
                .iter()
                .find(|provider| provider.provider_id == "restricted")
                .ok_or("restricted provider was not evaluated")?;
            if restricted.status != EvaluationStatus::Incompatible
                || restricted.reasons.first().map(|reason| reason.code)
                    != Some(ReasonCode::RuntimeUnsupported)
            {
                return Err("priority-100 incompatible provider was not excluded".into());
            }
        }
        let again = self.place(&self.discover(None).await, &requirements, &admission, None);
        if again.placement_id != report.placement_id
            || again.selected != report.selected
            || again.explanation != report.explanation
        {
            return Err("placement is not deterministic".into());
        }
        let selected = report
            .selected
            .as_ref()
            .ok_or("placement selected no provider")?;
        let before = self.local_executions.load(Ordering::SeqCst);
        let response = dispatch::execute(&self.pool, &report, request.clone())
            .await
            .map_err(|error| format!("pool execution failed: {error}"))?;
        let result = &response.result;
        if result.status != compute_core::ExecutionStatus::Completed || result.exit_code != Some(0)
        {
            return Err(format!(
                "pool execution did not complete: {:?}",
                result.error
            ));
        }
        let receipt = result
            .receipt
            .as_ref()
            .ok_or("pool execution omitted its receipt")?;
        report.verify_receipt(receipt)?;
        let placement = receipt
            .placement
            .as_ref()
            .ok_or("receipt omitted placement")?;
        if placement.provider_id != selected.provider_id
            || placement.selection_mode != SelectionMode::Pool
        {
            return Err("receipt placement does not name the selected provider".into());
        }
        if self.local_executions.load(Ordering::SeqCst) != before {
            return Err("a remote placement executed locally".into());
        }

        if kind != RuntimeKind::Wasm {
            let only = self.discover(Some("restricted")).await;
            let explicit = self.place(&only, &requirements, &admission, Some("restricted"));
            if explicit.outcome != PlacementOutcome::PlacementFailed
                || explicit
                    .failure
                    .as_ref()
                    .map(|failure| failure.code.as_str())
                    != Some("explicit_provider_incompatible")
            {
                return Err("incompatible explicit provider was accepted".into());
            }
            match dispatch::execute(&self.pool, &explicit, request).await {
                Err(error) if error.code == DispatchErrorCode::PlacementFailed => {}
                _ => return Err("incompatible explicit provider did not fail closed".into()),
            }
        }
        Ok(())
    }

    /// Negative placement cases: nothing executes and nothing is redirected.
    pub async fn certify_failures(&self, bundle_path: &Path) -> Result<String, String> {
        let bundle = WorkloadBundle::read(bundle_path).map_err(|error| error.to_string())?;
        let (request, requirements, admission) =
            self.requirements(&bundle, SubmissionMode::Synchronous)?;
        let executions = self.local_executions.load(Ordering::SeqCst);

        // All providers incompatible.
        let mut impossible = requirements.clone();
        impossible.distribution = Some(compute_placement::DistributionRequirement {
            id: format!("sha256:{}", "0".repeat(64)),
        });
        let records = self.discover(None).await;
        let report = self.place(&records, &impossible, &admission, None);
        if report.failure.as_ref().map(|failure| failure.code.as_str())
            != Some("no_compatible_provider")
            || report.incompatible_providers.len() != 3
        {
            return Err("all-incompatible pool did not fail with no_compatible_provider".into());
        }
        match dispatch::execute(&self.pool, &report, request.clone()).await {
            Err(error) if error.code == DispatchErrorCode::PlacementFailed => {}
            _ => return Err("failed placement dispatched a workload".into()),
        }

        // Malformed capabilities, provider disappearance, stale descriptors.
        let malformed = serve_raw("{\"protocol\":\"compute.remote@1\"}").await?;
        let vanished = {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
            format!(
                "http://{}",
                listener.local_addr().map_err(|error| error.to_string())?
            )
        };
        let (doomed_endpoint, doomed, _doomed_jobs) =
            spawn_server(ProviderPolicy::default()).await?;
        let mut hostile = ProviderPool::new(PoolPolicy::default());
        let add = |error: compute_placement::PlacementError| error.to_string();
        for (id, endpoint) in [
            ("malformed", malformed.0.as_str()),
            ("vanished", vanished.as_str()),
            ("doomed", doomed_endpoint.as_str()),
        ] {
            hostile
                .add_remote(
                    id,
                    remote_config(endpoint, 1),
                    Arc::new(RemoteProvider::new(endpoint)),
                )
                .map_err(add)?;
        }
        let mut cache = CapabilityCache::default();
        let now = Utc::now();
        let records = hostile
            .capabilities(&mut cache, DiscoveryMode::Refresh, None, now)
            .await;
        let status = |id: &str| {
            records
                .iter()
                .find(|record| record.provider_id == id)
                .map(|record| record.status)
        };
        if status("malformed") != Some(DiscoveryStatus::Invalid) {
            return Err("malformed capabilities were not rejected".into());
        }
        if status("vanished") != Some(DiscoveryStatus::Unavailable) {
            return Err("an unreachable provider was not reported unavailable".into());
        }
        let report = place(
            &hostile.configs(),
            hostile.policy(),
            &records,
            &requirements,
            &admission,
            None,
        );
        if report.compatible_providers != ["doomed"] {
            return Err(format!(
                "hostile pool admitted {:?}",
                report.compatible_providers
            ));
        }
        let expired =
            now + chrono::Duration::seconds(hostile.policy().capability_ttl_seconds as i64 + 1);
        let stale = hostile
            .capabilities(
                &mut cache,
                DiscoveryMode::PreferCache,
                Some("doomed"),
                expired,
            )
            .await;
        let stale_report = place(
            &hostile.configs(),
            hostile.policy(),
            &stale,
            &requirements,
            &admission,
            None,
        );
        if stale.first().map(|record| record.status) != Some(DiscoveryStatus::Stale)
            || stale_report.outcome != PlacementOutcome::PlacementFailed
            || stale_report
                .providers
                .first()
                .map(|provider| provider.status)
                != Some(EvaluationStatus::CapabilitiesUnknown)
        {
            return Err("stale capabilities were treated as valid".into());
        }

        // Provider unavailable after selection: distinct error, no retry.
        doomed.abort();
        let _ = doomed.await;
        match dispatch::execute(&hostile, &report, request).await {
            Err(error)
                if error.code == DispatchErrorCode::ProviderUnavailable
                    && !error.retried
                    && error.provider_id.as_deref() == Some("doomed") => {}
            other => {
                return Err(format!(
                    "provider loss after selection was not reported distinctly: {other:?}"
                ));
            }
        }
        malformed.1.abort();
        if self.local_executions.load(Ordering::SeqCst) != executions {
            return Err("a failed placement executed a workload".into());
        }
        Ok("incompatible pool, malformed capabilities, provider disappearance, stale capabilities, provider loss after selection, and incompatible explicit providers executed nothing and redirected nothing".into())
    }
}

async fn serve_raw(body: &'static str) -> Result<(String, tokio::task::JoinHandle<()>), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| error.to_string())?;
    let endpoint = format!(
        "http://{}",
        listener.local_addr().map_err(|error| error.to_string())?
    );
    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = [0; 8192];
            let _ = stream.read(&mut buffer).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    Ok((endpoint, handle))
}
