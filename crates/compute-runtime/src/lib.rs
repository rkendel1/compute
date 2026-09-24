use std::path::Path;

use compute_core::{
    BundleInspection, BundleVerification, BundleWorkloadPlan, ComputeCapability, ComputeError,
    ComputeManifest, Inspection, PortableDataFlow, Result, RuntimeAdapter, RuntimeAvailability,
    RuntimeKind, RuntimeSpec, Workload, WorkloadBundle, WorkloadPlan, WorkloadSpec,
    WorkloadValidationStatus,
};
use compute_runtime_process::{
    BunRuntime, DenoRuntime, DotnetRuntime, JvmRuntime, NativeRuntime, NodeRuntime, PhpRuntime,
    PythonRuntime, RubyRuntime, ShellRuntime,
};
use compute_runtime_wasm::WasmRuntime;

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
        let adapter = self.adapter(workload.runtime.kind)?;
        adapter.capabilities().validate(adapter.kind(), &workload)?;
        let runtime = adapter.resolve(&workload).await?;
        adapter.execute(&workload, &runtime).await
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

    pub async fn plan_workload(&self, path: &Path) -> Result<WorkloadPlan> {
        let workload = self.load_workload(path)?;
        self.plan_loaded_workload(path, workload)
    }

    pub async fn plan_workload_with_id(
        &self,
        path: &Path,
        expected_workload_id: &str,
    ) -> Result<WorkloadPlan> {
        let workload = self.load_workload(path)?;
        workload.require_id(expected_workload_id)?;
        self.plan_loaded_workload(path, workload)
    }

    fn plan_loaded_workload(&self, path: &Path, workload: WorkloadSpec) -> Result<WorkloadPlan> {
        let workload_id = workload.workload_id()?;
        let request = self.execution_request(path, &workload)?;
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
        let capability_error = capabilities
            .validate(workload.runtime, request)
            .err()
            .map(|error| error.to_string());
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
            workload,
            validation: WorkloadValidationStatus::Valid,
            resolved_runtime: request.runtime.clone(),
            backend_capabilities: capabilities,
            capability_compatible: capability_error.is_none(),
            capability_error,
            output_root: "/output".into(),
        })
    }

    pub async fn run_workload(&self, path: &Path) -> Result<compute_core::ExecutionResult> {
        let workload = self.load_workload(path)?;
        let request = self.execution_request(path, &workload)?;
        self.run(request).await
    }

    pub async fn run_workload_with_id(
        &self,
        path: &Path,
        expected_workload_id: &str,
    ) -> Result<compute_core::ExecutionResult> {
        let workload = self.load_workload(path)?;
        workload.require_id(expected_workload_id)?;
        let request = self.execution_request(path, &workload)?;
        self.run(request).await
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

    pub fn plan_bundle(
        &self,
        path: &Path,
        expected_workload_id: Option<&str>,
        expected_bundle_id: Option<&str>,
    ) -> Result<BundleWorkloadPlan> {
        let bundle = self.load_bundle(path)?;
        bundle.require_ids(expected_workload_id, expected_bundle_id)?;
        let verification = bundle.verification()?;
        let materialized = bundle.materialize()?;
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
        let bundle = self.load_bundle(path)?;
        bundle.require_ids(expected_workload_id, expected_bundle_id)?;
        let materialized = bundle.materialize()?;
        self.run(materialized.request).await
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
