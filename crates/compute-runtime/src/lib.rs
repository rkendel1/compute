use std::path::Path;

use compute_core::{
    ComputeError, ComputeManifest, Inspection, Result, RuntimeAdapter, RuntimeAvailability,
    RuntimeKind, RuntimeSpec, Workload,
};
use compute_runtime_process::{BunRuntime, DenoRuntime, NodeRuntime, PythonRuntime};
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
                Box::new(WasmRuntime::default()),
                Box::new(NodeRuntime::default()),
                Box::new(BunRuntime::default()),
                Box::new(DenoRuntime::default()),
                Box::new(PythonRuntime::default()),
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
        let runtime = adapter.resolve(&workload).await?;
        adapter.execute(&workload, &runtime).await
    }

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
            env,
            inputs: vec![],
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
        Some("js") | Some("mjs") | Some("cjs") | Some("ts") => {
            vec![RuntimeKind::Node, RuntimeKind::Bun, RuntimeKind::Deno]
        }
        _ => vec![],
    }
}

fn default_version(kind: RuntimeKind) -> Option<String> {
    match kind {
        RuntimeKind::Wasm => Some("wasi".to_string()),
        _ => None,
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
