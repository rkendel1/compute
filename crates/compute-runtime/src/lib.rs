use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::Utc;
use compute_core::{
    BundleIdentity, BundleInspection, BundleVerification, BundleWorkloadPlan, ComputeCapability,
    ComputeError, ComputeManifest, DependencyCapsule, DistributionIdentity,
    ExecutionDependencyEvidence, Inspection, IsolationProfile, PortableDataFlow,
    ReceiptEnvironment, Result, RuntimeAdapter, RuntimeAvailability, RuntimeKind, RuntimeSpec,
    Workload, WorkloadBundle, WorkloadIdentity, WorkloadPlan, WorkloadSpec,
    WorkloadValidationStatus, create_execution_receipt, input_receipts, request_workload_identity,
    sha256_file_identity, sha256_identity,
};
use compute_runtime_process::{
    BunRuntime, DenoRuntime, DotnetRuntime, JvmRuntime, NativeRuntime, NodeRuntime, PhpRuntime,
    PythonRuntime, RubyRuntime, ShellRuntime,
};
use compute_runtime_wasm::WasmRuntime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReceiptManifestRuntime {
    version: String,
    executable: String,
    artifact_sha256: String,
    payload_sha256: String,
    reported_version: String,
}

pub struct Compute {
    adapters: Vec<Box<dyn RuntimeAdapter>>,
}

impl Default for Compute {
    fn default() -> Self {
        Self::new()
    }
}

impl Compute {
    pub fn new() -> Self {
        Self {
            adapters: vec![
                Box::new(WasmRuntime),
                Box::new(NodeRuntime),
                Box::new(BunRuntime),
                Box::new(DenoRuntime),
                Box::new(PythonRuntime),
                Box::new(RubyRuntime),
                Box::new(PhpRuntime),
                Box::new(JvmRuntime),
                Box::new(DotnetRuntime),
                Box::new(NativeRuntime),
                Box::new(ShellRuntime),
            ],
        }
    }

    pub async fn runtimes(&self) -> Vec<RuntimeAvailability> {
        let mut items = Vec::with_capacity(self.adapters.len());
        for adapter in &self.adapters {
            items.push(adapter.availability(None).await);
        }

        items
    }

    pub async fn inventory(&self) -> compute_core::RuntimeInventory {
        let mut runtimes = Vec::with_capacity(self.adapters.len());
        for adapter in &self.adapters {
            let descriptor = adapter.descriptor();
            let availability = adapter.availability(None).await;
            runtimes.push(compute_core::RuntimeInventoryEntry {
                id: descriptor.id,
                version: descriptor.version,
                executable: descriptor.executable,
                available: availability.available,
                compatible: availability.compatible,
                detected_version: availability.version,
                detected_executable: availability.executable,
                source: availability.source,
                capabilities: descriptor.capabilities,
                remediation: availability.remediation,
            });
        }
        compute_core::RuntimeInventory {
            compute_version: env!("CARGO_PKG_VERSION").into(),
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            runtimes,
        }
    }

    /// Identity of the installed Compute distribution, independent of any
    /// particular runtime selection.
    pub fn installed_distribution_identity(&self) -> Result<DistributionIdentity> {
        if let Some(home) = distribution_root() {
            let manifest_path = home.join("runtime-manifest.json");
            if manifest_path.is_file() {
                let manifest: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(manifest_path)?)?;
                let string = |name: &str| {
                    manifest
                        .get(name)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            ComputeError::InvalidReceipt(format!(
                                "distribution manifest is missing {name}"
                            ))
                        })
                };
                return Ok(DistributionIdentity {
                    id: string("distribution_id")?,
                    platform: string("platform")?,
                    manifest_version: manifest
                        .get("schema_version")
                        .and_then(serde_json::Value::as_u64)
                        .ok_or_else(|| {
                            ComputeError::InvalidReceipt(
                                "distribution manifest is missing schema_version".into(),
                            )
                        })?
                        .to_string(),
                });
            }
        }
        let lock_bytes = include_bytes!("../../../distribution/runtime-lock.json");
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let descriptor = serde_json::to_vec(&serde_json::json!({
            "kind": "source-development", "compute_version": env!("CARGO_PKG_VERSION"),
            "platform": platform, "runtime_lock": sha256_identity(lock_bytes),
        }))?;
        Ok(DistributionIdentity {
            id: sha256_identity(&descriptor),
            platform,
            manifest_version: "development".into(),
        })
    }

    /// Content identity of each runtime artifact in the installed
    /// distribution. These are the same values receipts record as
    /// `runtime.distribution_runtime_id`.
    pub fn runtime_artifact_identities(&self) -> Result<BTreeMap<RuntimeKind, String>> {
        let mut identities = BTreeMap::new();
        if let Some(root) = distribution_root() {
            let manifest_path = root.join("runtime-manifest.json");
            if manifest_path.is_file() {
                let manifest: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(manifest_path)?)?;
                for adapter in &self.adapters {
                    if let Some(payload) = manifest
                        .get("runtimes")
                        .and_then(|value| value.get(adapter.kind().as_str()))
                        .and_then(|value| value.get("payload_sha256"))
                        .and_then(serde_json::Value::as_str)
                    {
                        identities.insert(adapter.kind(), prefixed_digest(payload)?);
                    }
                }
                return Ok(identities);
            }
        }
        let lock: serde_json::Value =
            serde_json::from_slice(include_bytes!("../../../distribution/runtime-lock.json"))?;
        for adapter in &self.adapters {
            if let Some(entry) = lock
                .get("runtimes")
                .and_then(|value| value.get(adapter.kind().as_str()))
            {
                identities.insert(adapter.kind(), sha256_identity(&serde_json::to_vec(entry)?));
            }
        }
        Ok(identities)
    }

    pub async fn doctor(&self) -> Vec<compute_core::RuntimeReport> {
        let mut reports = Vec::with_capacity(self.adapters.len());
        for adapter in &self.adapters {
            reports.push(compute_core::RuntimeReport {
                runtime: adapter.kind(),
                descriptor: adapter.descriptor(),
                availability: adapter.availability(None).await,
                capabilities: adapter.capabilities(),
            });
        }
        reports
    }

    pub fn capabilities(&self, kind: RuntimeKind) -> Result<compute_core::RuntimeCapabilities> {
        Ok(self.adapter(kind)?.capabilities())
    }

    pub async fn runtime(
        &self,
        kind: RuntimeKind,
        requested: Option<&str>,
    ) -> Result<RuntimeAvailability> {
        let adapter = self.adapter(kind)?;
        let mut runtime = adapter.availability(requested).await;
        runtime.selected = true;
        Ok(runtime)
    }

    /// Resolve the distribution identity that would execute a request without
    /// starting the workload. Providers use this for fail-closed preflight.
    pub async fn distribution_identity(
        &self,
        request: &compute_core::ExecutionRequest,
    ) -> Result<DistributionIdentity> {
        let adapter = self.adapter(request.runtime.kind)?;
        let resolved = adapter.resolve(request).await?;
        Ok(receipt_environment(adapter, &resolved)?.distribution)
    }

    pub fn inspect_path(&self, path: &Path, runtime: Option<RuntimeSpec>) -> Result<Inspection> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if path.file_name().and_then(|value| value.to_str()) == Some("compute.json") {
            let manifest = ComputeManifest::load(&path)?;
            let entrypoint = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(&manifest.entrypoint);
            let selected_runtime = runtime.unwrap_or(manifest.runtime.clone());
            return Ok(Inspection {
                path,
                entrypoint,
                runtime: Some(selected_runtime),
                candidates: vec![manifest.runtime.kind],
                ambiguous: false,
                manifest: true,
            });
        }

        let candidates = candidates_for_path(&path);
        let ambiguous = candidates.len() > 1 && runtime.is_none();
        let runtime = match (runtime, candidates.as_slice()) {
            (Some(runtime), _) => Some(runtime),
            (None, [kind]) => Some(RuntimeSpec {
                kind: *kind,
                version: default_version(*kind),
            }),
            _ => None,
        };

        Ok(Inspection {
            path: path.clone(),
            entrypoint: path,
            runtime,
            candidates,
            ambiguous,
            manifest: false,
        })
    }

    pub async fn run(&self, workload: Workload) -> Result<compute_core::ExecutionResult> {
        let workload_id = request_workload_identity(&workload)?;
        self.run_identified(workload, workload_id, None).await
    }

    async fn run_identified(
        &self,
        workload: Workload,
        workload_id: WorkloadIdentity,
        bundle_id: Option<BundleIdentity>,
    ) -> Result<compute_core::ExecutionResult> {
        let inputs = input_receipts(&workload)?;
        let adapter = self.adapter(workload.runtime.kind)?;
        let capabilities = adapter.capabilities();
        let isolation = capabilities
            .resolve_isolation(adapter.kind(), &workload)
            .map_err(|reason| ComputeError::IsolationUnavailable {
                runtime: adapter.kind(),
                profile: workload.isolation,
                code: reason.code,
            })?;
        let runtime = adapter.resolve(&workload).await?;
        if let Some(capsule) = &workload.dependencies
            && let Some(required) = &capsule.runtime_version
        {
            let found = runtime.resolved_version.as_deref().unwrap_or("unknown");
            if found != required {
                return Err(ComputeError::InvalidDependencyCapsule(format!(
                    "dependency capsule requires {}@{required}, resolved {found}",
                    capsule.runtime
                )));
            }
        }
        let environment = receipt_environment(adapter, &runtime)?;
        let started_at = Utc::now();
        let mut result = adapter.execute(&workload, &runtime).await?;
        let finished_at = Utc::now();
        result.isolation = Some(isolation);
        result.dependencies = match &workload.dependencies {
            Some(capsule) => Some(ExecutionDependencyEvidence {
                capsule_id: capsule.capsule_id()?,
                file_count: capsule.files.len() as u64,
                verified: true,
            }),
            None => None,
        };
        result.receipt = Some(create_execution_receipt(
            &workload,
            &runtime,
            &result,
            workload_id,
            bundle_id,
            inputs,
            environment,
            started_at,
            finished_at,
        )?);
        let provider = compute_core::ProviderIdentity::Local { id: "local".into() };
        result.provider = Some(provider.clone());
        if let Some(receipt) = &mut result.receipt {
            receipt.provider = Some(provider);
            receipt.provider_protocol = Some("compute.local@1".into());
            receipt.seal()?;
        }
        Ok(result)
    }

    pub fn load_workload(&self, path: &Path) -> Result<WorkloadSpec> {
        WorkloadSpec::load(path)
    }

    pub fn execution_request(
        &self,
        path: &Path,
        workload: &WorkloadSpec,
    ) -> Result<compute_core::ExecutionRequest> {
        workload.materialize(path)
    }

    pub fn execution_request_from(
        &self,
        root: &Path,
        workload: &WorkloadSpec,
    ) -> Result<compute_core::ExecutionRequest> {
        workload.materialize_from(root)
    }

    pub fn plan_generated_workload(
        &self,
        root: &Path,
        workload: WorkloadSpec,
    ) -> Result<WorkloadPlan> {
        self.plan_generated_workload_with_dependencies(root, workload, None)
    }

    pub fn plan_generated_workload_with_dependencies(
        &self,
        root: &Path,
        workload: WorkloadSpec,
        capsule: Option<DependencyCapsule>,
    ) -> Result<WorkloadPlan> {
        let workload_id = workload.workload_id()?;
        let mut request = workload.materialize_from(root)?;
        attach_dependencies_for_plan(&workload, &mut request, capsule)?;
        self.plan_request(workload, workload_id, &request)
    }

    pub async fn run_generated_workload(
        &self,
        root: &Path,
        workload: WorkloadSpec,
        stdin: Vec<u8>,
    ) -> Result<compute_core::ExecutionResult> {
        self.run_generated_workload_with_dependencies(root, workload, stdin, None)
            .await
    }

    pub async fn run_generated_workload_with_dependencies(
        &self,
        root: &Path,
        workload: WorkloadSpec,
        stdin: Vec<u8>,
        capsule: Option<DependencyCapsule>,
    ) -> Result<compute_core::ExecutionResult> {
        let workload_id = WorkloadIdentity::parse(workload.workload_id()?)?;
        let mut request = workload.materialize_from(root)?;
        attach_dependencies(&workload, &mut request, capsule)?;
        request.stdin = stdin;
        self.run_identified(request, workload_id, None).await
    }

    pub async fn plan_workload(&self, path: &Path) -> Result<WorkloadPlan> {
        self.plan_workload_with_options(path, None, None).await
    }

    pub async fn plan_workload_with_id(
        &self,
        path: &Path,
        expected_workload_id: &str,
    ) -> Result<WorkloadPlan> {
        self.plan_workload_with_options(path, Some(expected_workload_id), None)
            .await
    }

    pub async fn plan_workload_with_options(
        &self,
        path: &Path,
        expected_workload_id: Option<&str>,
        isolation: Option<IsolationProfile>,
    ) -> Result<WorkloadPlan> {
        self.plan_workload_with_dependencies(path, expected_workload_id, isolation, None)
            .await
    }

    pub async fn plan_workload_with_dependencies(
        &self,
        path: &Path,
        expected_workload_id: Option<&str>,
        isolation: Option<IsolationProfile>,
        capsule: Option<DependencyCapsule>,
    ) -> Result<WorkloadPlan> {
        let workload = self.load_workload(path)?;
        if let Some(expected) = expected_workload_id {
            workload.require_id(expected)?;
        }
        let workload_id = workload.workload_id()?;
        let mut request = self.execution_request(path, &workload)?;
        attach_dependencies_for_plan(&workload, &mut request, capsule)?;
        request.isolation = resolve_override(workload.isolation.profile, isolation)?;
        self.plan_request(workload, workload_id, &request)
    }

    fn plan_request(
        &self,
        workload: WorkloadSpec,
        workload_id: String,
        request: &compute_core::ExecutionRequest,
    ) -> Result<WorkloadPlan> {
        let adapter = self.adapter(workload.runtime)?;
        let capabilities = adapter.capabilities();
        let isolation = capabilities.isolation_plan(workload.runtime, request);
        let capability_error = isolation
            .reason
            .as_ref()
            .map(|reason| reason.message.clone());
        Ok(WorkloadPlan {
            valid: true,
            workload_id,
            capability: ComputeCapability::default(),
            data_flow: PortableDataFlow {
                steps: vec![
                    "input".into(),
                    "materialization".into(),
                    "entrypoint".into(),
                    "runtime".into(),
                    "declared_output".into(),
                    "collection".into(),
                ],
                input_root: "/work".into(),
                entrypoint: workload.entrypoint.clone(),
                runtime: workload.runtime,
                declared_outputs: workload.outputs.clone(),
                output_root: "/output".into(),
            },
            input_preparation: workload.inputs.clone(),
            workload: workload.clone(),
            validation: WorkloadValidationStatus::Valid,
            resolved_runtime: request.runtime.clone(),
            backend_capabilities: capabilities,
            capability_compatible: capability_error.is_none(),
            capability_error,
            isolation,
            output_root: "/output".into(),
            dependencies: compute_core::DependencyRequirementPlan {
                required: workload.dependencies.is_some(),
                capsule_id: workload
                    .dependencies
                    .as_ref()
                    .map(|value| value.capsule.clone()),
                available: request.dependencies.is_some(),
            },
        })
    }

    pub async fn run_workload(&self, path: &Path) -> Result<compute_core::ExecutionResult> {
        self.run_workload_with_options(path, None, None).await
    }

    pub async fn run_workload_with_id(
        &self,
        path: &Path,
        expected_workload_id: &str,
    ) -> Result<compute_core::ExecutionResult> {
        self.run_workload_with_options(path, Some(expected_workload_id), None)
            .await
    }

    pub async fn run_workload_with_options(
        &self,
        path: &Path,
        expected_workload_id: Option<&str>,
        isolation: Option<IsolationProfile>,
    ) -> Result<compute_core::ExecutionResult> {
        self.run_workload_with_dependencies(path, expected_workload_id, isolation, None)
            .await
    }

    pub async fn run_workload_with_dependencies(
        &self,
        path: &Path,
        expected_workload_id: Option<&str>,
        isolation: Option<IsolationProfile>,
        capsule: Option<DependencyCapsule>,
    ) -> Result<compute_core::ExecutionResult> {
        let workload = self.load_workload(path)?;
        if let Some(expected) = expected_workload_id {
            workload.require_id(expected)?;
        }
        let workload_id = WorkloadIdentity::parse(workload.workload_id()?)?;
        let mut request = self.execution_request(path, &workload)?;
        attach_dependencies(&workload, &mut request, capsule)?;
        request.isolation = resolve_override(workload.isolation.profile, isolation)?;
        self.run_identified(request, workload_id, None).await
    }

    pub fn load_bundle(&self, path: &Path) -> Result<WorkloadBundle> {
        WorkloadBundle::read(path)
    }

    pub fn inspect_bundle(&self, path: &Path) -> Result<BundleInspection> {
        self.load_bundle(path)?.inspection()
    }

    pub fn verify_bundle(&self, path: &Path) -> Result<BundleVerification> {
        self.load_bundle(path)?.verification()
    }

    pub fn create_bundle(&self, workload: &Path, output: &Path) -> Result<BundleInspection> {
        let bundle = WorkloadBundle::create(workload)?;
        bundle.write(output)?;
        bundle.inspection()
    }

    pub fn create_bundle_with_dependencies(
        &self,
        workload_path: &Path,
        output: &Path,
        capsule: DependencyCapsule,
    ) -> Result<BundleInspection> {
        let workload = WorkloadSpec::load(workload_path)?;
        let base = workload_path.parent().unwrap_or_else(|| Path::new("."));
        let bundle = WorkloadBundle::create_from_with_capsule(workload, base, Some(capsule))?;
        bundle.write(output)?;
        bundle.inspection()
    }

    pub fn create_generated_bundle(
        &self,
        root: &Path,
        workload: WorkloadSpec,
        output: &Path,
    ) -> Result<BundleInspection> {
        self.create_generated_bundle_with_dependencies(root, workload, output, None)
    }

    pub fn create_generated_bundle_with_dependencies(
        &self,
        root: &Path,
        workload: WorkloadSpec,
        output: &Path,
        capsule: Option<DependencyCapsule>,
    ) -> Result<BundleInspection> {
        let bundle = WorkloadBundle::create_from_with_capsule(workload, root, capsule)?;
        bundle.write(output)?;
        bundle.inspection()
    }

    pub fn plan_bundle(
        &self,
        path: &Path,
        expected_workload_id: Option<&str>,
        expected_bundle_id: Option<&str>,
    ) -> Result<BundleWorkloadPlan> {
        self.plan_bundle_with_isolation(path, expected_workload_id, expected_bundle_id, None)
    }

    pub fn plan_bundle_with_isolation(
        &self,
        path: &Path,
        expected_workload_id: Option<&str>,
        expected_bundle_id: Option<&str>,
        isolation: Option<IsolationProfile>,
    ) -> Result<BundleWorkloadPlan> {
        self.plan_bundle_with_dependencies(
            path,
            expected_workload_id,
            expected_bundle_id,
            isolation,
            None,
        )
    }

    pub fn plan_bundle_with_dependencies(
        &self,
        path: &Path,
        expected_workload_id: Option<&str>,
        expected_bundle_id: Option<&str>,
        isolation: Option<IsolationProfile>,
        capsule: Option<DependencyCapsule>,
    ) -> Result<BundleWorkloadPlan> {
        let bundle = self.load_bundle(path)?;
        bundle.require_ids(expected_workload_id, expected_bundle_id)?;
        let verification = bundle.verification()?;
        let mut materialized = bundle.materialize()?;
        attach_dependencies_for_plan(&bundle.workload, &mut materialized.request, capsule)?;
        materialized.request.isolation =
            resolve_override(bundle.workload.isolation.profile, isolation)?;
        let plan = self.plan_request(
            bundle.workload.clone(),
            verification.workload_id.clone(),
            &materialized.request,
        )?;
        Ok(BundleWorkloadPlan {
            bundle_verification: verification,
            plan,
        })
    }

    pub async fn run_bundle(
        &self,
        path: &Path,
        expected_workload_id: Option<&str>,
        expected_bundle_id: Option<&str>,
    ) -> Result<compute_core::ExecutionResult> {
        self.run_bundle_with_isolation(path, expected_workload_id, expected_bundle_id, None)
            .await
    }

    pub async fn run_bundle_with_isolation(
        &self,
        path: &Path,
        expected_workload_id: Option<&str>,
        expected_bundle_id: Option<&str>,
        isolation: Option<IsolationProfile>,
    ) -> Result<compute_core::ExecutionResult> {
        self.run_bundle_with_dependencies(
            path,
            expected_workload_id,
            expected_bundle_id,
            isolation,
            None,
        )
        .await
    }

    pub async fn run_bundle_with_dependencies(
        &self,
        path: &Path,
        expected_workload_id: Option<&str>,
        expected_bundle_id: Option<&str>,
        isolation: Option<IsolationProfile>,
        capsule: Option<DependencyCapsule>,
    ) -> Result<compute_core::ExecutionResult> {
        let bundle = self.load_bundle(path)?;
        bundle.require_ids(expected_workload_id, expected_bundle_id)?;
        let verification = bundle.verification()?;
        let mut materialized = bundle.materialize()?;
        attach_dependencies(&bundle.workload, &mut materialized.request, capsule)?;
        materialized.request.isolation =
            resolve_override(bundle.workload.isolation.profile, isolation)?;
        self.run_identified(
            materialized.request,
            WorkloadIdentity::parse(verification.workload_id)?,
            Some(BundleIdentity::parse(verification.bundle_id)?),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub fn workload_from_path(
        &self,
        path: &Path,
        runtime: Option<RuntimeSpec>,
        args: Vec<String>,
        env: Vec<compute_core::EnvironmentVariable>,
        mounts: Vec<compute_core::Mount>,
        network: compute_core::NetworkPolicy,
        resources: compute_core::ResourceLimits,
        isolation: IsolationProfile,
    ) -> Result<Workload> {
        let inspection = self.inspect_path(path, runtime)?;
        if inspection.ambiguous {
            let candidates = inspection
                .candidates
                .iter()
                .map(|runtime| runtime.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ComputeError::AmbiguousRuntime(candidates));
        }

        let runtime = inspection.runtime.ok_or_else(|| {
            ComputeError::InvalidWorkload(format!(
                "could not determine runtime for {}",
                path.display()
            ))
        })?;

        Ok(Workload {
            runtime,
            entrypoint: inspection.entrypoint,
            args,
            stdin: vec![],
            env,
            inputs: vec![],
            outputs: vec![],
            mounts,
            network,
            resources,
            isolation,
            dependencies: None,
        })
    }

    fn adapter(&self, kind: RuntimeKind) -> Result<&dyn RuntimeAdapter> {
        self.adapters
            .iter()
            .find(|adapter| adapter.kind() == kind)
            .map(|adapter| adapter.as_ref())
            .ok_or_else(|| ComputeError::UnknownRuntime(kind.to_string()))
    }
}

fn resolve_override(
    declared: IsolationProfile,
    requested: Option<IsolationProfile>,
) -> Result<IsolationProfile> {
    match requested {
        Some(profile) if profile < declared => Err(ComputeError::InvalidWorkload(format!(
            "isolation override {profile} cannot weaken declared profile {declared}"
        ))),
        Some(profile) => Ok(profile),
        None => Ok(declared),
    }
}

fn attach_dependencies(
    workload: &WorkloadSpec,
    request: &mut compute_core::ExecutionRequest,
    supplied: Option<DependencyCapsule>,
) -> Result<()> {
    let Some(reference) = &workload.dependencies else {
        if supplied.is_some() || request.dependencies.is_some() {
            return Err(ComputeError::InvalidDependencyCapsule(
                "dependency capsule supplied but not declared by WorkloadSpec".into(),
            ));
        }
        return Ok(());
    };
    let capsule = match supplied.or_else(|| request.dependencies.take()) {
        Some(capsule) => capsule,
        None => load_cached_capsule(&reference.capsule)?,
    };
    let actual = capsule.capsule_id()?;
    if actual != reference.capsule {
        return Err(ComputeError::InvalidDependencyCapsule(format!(
            "dependency capsule identity mismatch: expected {}, found {actual}",
            reference.capsule
        )));
    }
    capsule.require_compatible(workload.runtime)?;
    request.dependencies = Some(capsule);
    Ok(())
}

fn attach_dependencies_for_plan(
    workload: &WorkloadSpec,
    request: &mut compute_core::ExecutionRequest,
    supplied: Option<DependencyCapsule>,
) -> Result<()> {
    if supplied.is_some() || request.dependencies.is_some() || workload.dependencies.is_none() {
        attach_dependencies(workload, request, supplied)?;
    }
    Ok(())
}

fn load_cached_capsule(identity: &str) -> Result<DependencyCapsule> {
    compute_core::validate_sha256_identity(identity)?;
    let root = std::env::var_os("COMPUTE_DEPENDENCY_CACHE")
        .map(PathBuf::from)
        .ok_or_else(|| {
            ComputeError::InvalidDependencyCapsule(format!(
                "dependency_missing: capsule {identity} is referenced but not embedded; set COMPUTE_DEPENDENCY_CACHE"
            ))
        })?;
    let digest = identity
        .strip_prefix("sha256:")
        .expect("validated identity");
    let path = root.join(format!("{digest}.deps"));
    if !path.is_file() {
        return Err(ComputeError::InvalidDependencyCapsule(format!(
            "dependency_missing: capsule {identity} was not found in {}",
            root.display()
        )));
    }
    DependencyCapsule::read(&path)
}

fn receipt_environment(
    adapter: &dyn RuntimeAdapter,
    runtime: &compute_core::ResolvedRuntime,
) -> Result<ReceiptEnvironment> {
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let executable_identity = runtime
        .executable
        .as_deref()
        .filter(|path| path.is_file())
        .map(sha256_file_identity)
        .transpose()?
        .unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| sha256_file_identity(&path).ok())
                .unwrap_or_else(|| sha256_identity(adapter.descriptor().id.as_str().as_bytes()))
        });

    if let Some(root) = distribution_root() {
        let manifest_path = root.join("runtime-manifest.json");
        let manifest_bytes = std::fs::read(&manifest_path)?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
        let string = |name: &str| {
            manifest
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    ComputeError::InvalidReceipt(format!("distribution manifest is missing {name}"))
                })
        };
        let id = string("distribution_id")?;
        compute_core::validate_sha256_identity(&id)?;
        let declared_platform = string("platform")?;
        if declared_platform != platform {
            return Err(ComputeError::InvalidReceipt(format!(
                "distribution platform mismatch: declared {declared_platform}, observed {platform}"
            )));
        }
        let schema = manifest
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                ComputeError::InvalidReceipt(
                    "distribution manifest is missing schema_version".into(),
                )
            })?;
        let lock = string("runtime_lock_sha256")?;
        let compute_version = string("compute_version")?;
        let runtimes: BTreeMap<String, ReceiptManifestRuntime> =
            serde_json::from_value(manifest.get("runtimes").cloned().ok_or_else(|| {
                ComputeError::InvalidReceipt("distribution manifest is missing runtimes".into())
            })?)?;
        let expected_distribution = sha256_identity(&serde_json::to_vec(&(
            compute_version,
            declared_platform,
            lock.clone(),
            &runtimes,
        ))?);
        if id != expected_distribution {
            return Err(ComputeError::InvalidReceipt(
                "distribution identity mismatch".into(),
            ));
        }
        let runtime_entry = manifest
            .get("runtimes")
            .and_then(|value| value.get(runtime.kind.as_str()))
            .ok_or_else(|| {
                ComputeError::InvalidReceipt(format!(
                    "distribution manifest is missing runtime {}",
                    runtime.kind
                ))
            })?;
        let payload = runtime_entry
            .get("payload_sha256")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ComputeError::InvalidReceipt(
                    "distribution runtime is missing payload identity".into(),
                )
            })?;
        let runtime_manifest = runtimes.get(runtime.kind.as_str()).expect("checked above");
        if !runtime_manifest.executable.starts_with('<') {
            let actual_payload = hash_tree(&root.join("runtimes").join(runtime.kind.as_str()))?;
            if actual_payload != runtime_manifest.payload_sha256 {
                return Err(ComputeError::InvalidReceipt(format!(
                    "runtime payload identity mismatch for {}",
                    runtime.kind
                )));
            }
        }
        return Ok(ReceiptEnvironment {
            distribution: DistributionIdentity {
                id,
                platform,
                manifest_version: schema.to_string(),
            },
            distribution_runtime_id: prefixed_digest(payload)?,
            executable_identity,
            runtime_lock_id: prefixed_digest(&lock)?,
            manifest_id: sha256_identity(&manifest_bytes),
        });
    }

    let lock_bytes = include_bytes!("../../../distribution/runtime-lock.json");
    let lock: serde_json::Value = serde_json::from_slice(lock_bytes)?;
    let runtime_entry = lock
        .get("runtimes")
        .and_then(|value| value.get(runtime.kind.as_str()))
        .ok_or_else(|| {
            ComputeError::InvalidReceipt(format!("runtime lock is missing {}", runtime.kind))
        })?;
    let descriptor = serde_json::to_vec(&serde_json::json!({
        "kind": "source-development", "compute_version": env!("CARGO_PKG_VERSION"),
        "platform": platform, "runtime_lock": sha256_identity(lock_bytes),
    }))?;
    let distribution_id = sha256_identity(&descriptor);
    Ok(ReceiptEnvironment {
        distribution: DistributionIdentity {
            id: distribution_id.clone(),
            platform,
            manifest_version: "development".into(),
        },
        distribution_runtime_id: sha256_identity(&serde_json::to_vec(runtime_entry)?),
        executable_identity,
        runtime_lock_id: sha256_identity(lock_bytes),
        manifest_id: sha256_identity(&descriptor),
    })
}

fn prefixed_digest(value: &str) -> Result<String> {
    let value = if value.starts_with("sha256:") {
        value.to_owned()
    } else {
        format!("sha256:{value}")
    };
    compute_core::validate_sha256_identity(&value)?;
    Ok(value)
}

fn distribution_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("COMPUTE_HOME") {
        return Some(root.into());
    }
    let executable = std::env::current_exe().ok()?;
    let root = executable.parent()?.parent()?.to_path_buf();
    root.join("runtime-manifest.json").is_file().then_some(root)
}

fn hash_tree(root: &Path) -> Result<String> {
    if !root.is_dir() {
        return Err(ComputeError::InvalidReceipt(format!(
            "runtime payload is unavailable: {}",
            root.display()
        )));
    }
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| ComputeError::InvalidReceipt(error.to_string()))?;
    entries.sort_by_key(|entry| entry.path().strip_prefix(root).unwrap().to_path_buf());
    let mut digest = Sha256::new();
    for entry in entries.into_iter().filter(|entry| entry.path() != root) {
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| ComputeError::InvalidReceipt(error.to_string()))?;
        digest.update(relative.to_string_lossy().as_bytes());
        if entry.file_type().is_file() {
            digest.update(b"f\0");
            let mut file = std::fs::File::open(entry.path())?;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
        } else if entry.file_type().is_symlink() {
            digest.update(b"l\0");
            digest.update(
                std::fs::read_link(entry.path())?
                    .to_string_lossy()
                    .as_bytes(),
            );
        } else {
            digest.update(b"d\0");
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn candidates_for_path(path: &Path) -> Vec<RuntimeKind> {
    match path.extension().and_then(|value| value.to_str()) {
        Some("wasm") => vec![RuntimeKind::Wasm],
        Some("py") => vec![RuntimeKind::Python],
        Some("rb") => vec![RuntimeKind::Ruby],
        Some("php") => vec![RuntimeKind::Php],
        Some("jar") => vec![RuntimeKind::Jvm],
        Some("dll") => vec![RuntimeKind::Dotnet],
        Some("sh") => vec![RuntimeKind::Shell],
        Some("js") | Some("mjs") | Some("cjs") | Some("ts") => {
            vec![RuntimeKind::Node, RuntimeKind::Bun, RuntimeKind::Deno]
        }
        _ => vec![],
    }
}

fn default_version(kind: RuntimeKind) -> Option<String> {
    match kind {
        RuntimeKind::Wasm => Some("wasi".to_string()),
        RuntimeKind::Node
        | RuntimeKind::Bun
        | RuntimeKind::Deno
        | RuntimeKind::Python
        | RuntimeKind::Ruby
        | RuntimeKind::Php
        | RuntimeKind::Jvm
        | RuntimeKind::Dotnet
        | RuntimeKind::Native
        | RuntimeKind::Shell => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn inspect_marks_javascript_as_ambiguous() {
        let compute = Compute::new();
        let inspection = compute.inspect_path(Path::new("hello.js"), None).unwrap();
        assert!(inspection.ambiguous);
        assert_eq!(
            inspection.candidates,
            vec![RuntimeKind::Node, RuntimeKind::Bun, RuntimeKind::Deno]
        );
    }

    #[test]
    fn inspect_selects_python_by_extension() {
        let compute = Compute::new();
        let inspection = compute.inspect_path(Path::new("hello.py"), None).unwrap();
        assert_eq!(inspection.runtime.unwrap().kind, RuntimeKind::Python);
    }

    #[test]
    fn inspect_reads_compute_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("compute.json");
        std::fs::write(
            &manifest,
            r#"{"runtime":{"kind":"python","version":"3.13"},"entrypoint":"main.py"}"#,
        )
        .unwrap();

        let compute = Compute::new();
        let inspection = compute.inspect_path(&manifest, None).unwrap();

        assert!(!inspection.ambiguous);
        assert!(inspection.manifest);
        assert_eq!(inspection.runtime.unwrap().kind, RuntimeKind::Python);
        assert!(inspection.entrypoint.ends_with(PathBuf::from("main.py")));
    }
}
