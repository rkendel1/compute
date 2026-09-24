use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use compute_core::{
    ExecutionStatus, InputSource, NetworkPolicy, ResourceLimits, RuntimeKind, RuntimeSource,
    WorkloadInput, WorkloadOutput, WorkloadSpec,
};
use compute_runtime::Compute;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct CertificationReport {
    pub certification_version: u32,
    pub distribution: Option<String>,
    pub platform: Option<String>,
    pub require_all_runtimes: bool,
    pub runtimes: Vec<RuntimeCertification>,
    pub checks: Vec<CertificationCheck>,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeCertification {
    pub runtime: RuntimeKind,
    pub locked_version: Option<String>,
    pub reported_version: Option<String>,
    pub executable: Option<PathBuf>,
    pub source: Option<RuntimeSource>,
    pub result: CertificationResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CertificationCheck {
    pub name: String,
    pub result: CertificationResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificationResult {
    Pass,
    Fail,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeLock {
    schema_version: u32,
    runtimes: BTreeMap<String, LockedRuntime>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DistributionManifest {
    compute_version: String,
    distribution_version: String,
    platform: String,
    runtimes: BTreeMap<String, LockedRuntime>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LockedRuntime {
    version: String,
    executable: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureManifest {
    schema_version: u32,
    runtimes: BTreeMap<String, FixtureDefinition>,
    #[serde(default)]
    appport_runner: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureDefinition {
    entrypoint: String,
    #[serde(default)]
    files: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct FixtureIdentity {
    runtime: String,
    runtime_version: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct SemanticOutput {
    input: String,
    success: bool,
    argument: String,
    environment: String,
    host_environment: String,
}

pub fn spawn_clean_certification(json: bool) -> compute_core::Result<()> {
    let root = distribution_root()?;
    let temporary = tempfile::tempdir()?;
    let fake_bin = temporary.path().join("poisoned-path");
    let home = temporary.path().join("home");
    let tmp = temporary.path().join("tmp");
    fs::create_dir_all(&fake_bin)?;
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&tmp)?;
    create_poisoned_path(&fake_bin)?;

    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command.args(["certify", "--internal-clean-environment"]);
    if json {
        command.arg("--json");
    }
    let path = std::env::join_paths([fake_bin, root.join("bin")])
        .map_err(|error| compute_core::ComputeError::Runtime(error.to_string()))?;
    command
        .env_clear()
        .env("PATH", path)
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("COMPUTE_HOME", &root)
        .env("COMPUTE_CERTIFICATION_CLEAN", "1")
        .env("COMPUTE_HOST_SECRET", "must-not-leak");
    if std::env::var("COMPUTE_REQUIRE_ALL_RUNTIMES").as_deref() == Ok("1") {
        command.env("COMPUTE_REQUIRE_ALL_RUNTIMES", "1");
    }
    let output = command.output()?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Err(compute_core::ComputeError::Runtime(
            "clean-environment certification failed".into(),
        ));
    }
    Ok(())
}

pub async fn certify(compute: &Compute) -> CertificationReport {
    let require_all = std::env::var("COMPUTE_REQUIRE_ALL_RUNTIMES").as_deref() == Ok("1");
    let mut report = CertificationReport {
        certification_version: 1,
        distribution: None,
        platform: None,
        require_all_runtimes: require_all,
        runtimes: vec![],
        checks: vec![],
        passed: false,
    };

    let root = match distribution_root() {
        Ok(root) => root,
        Err(error) => {
            fail_check(&mut report, "assembled_distribution", error.to_string());
            return report;
        }
    };
    let loaded = load_artifact_metadata(&root);
    let (lock, manifest, fixtures) = match loaded {
        Ok(value) => value,
        Err(error) => {
            fail_check(&mut report, "artifact_metadata", error);
            return report;
        }
    };
    report.distribution = Some(manifest.distribution_version.clone());
    report.platform = Some(manifest.platform.clone());
    pass_check(
        &mut report,
        "clean_environment",
        "PATH and host environment are poisoned",
    );

    let metadata_valid = lock.schema_version == 1
        && fixtures.schema_version == 1
        && lock.runtimes == manifest.runtimes
        && manifest.compute_version == env!("CARGO_PKG_VERSION");
    if metadata_valid {
        pass_check(
            &mut report,
            "runtime_lock",
            "runtime lock exactly matches the assembled manifest",
        );
    } else {
        fail_check(
            &mut report,
            "runtime_lock",
            "runtime lock, distribution manifest, or Compute version differs".into(),
        );
    }

    let inventory = compute.inventory().await;
    let mut certified_bundle: Option<(PathBuf, String, String)> = None;
    for kind in RuntimeKind::ALL {
        let locked = lock.runtimes.get(kind.as_str());
        let fixture = fixtures.runtimes.get(kind.as_str());
        let entry = inventory.runtimes.iter().find(|item| item.id == kind);
        let result = match (locked, fixture, entry) {
            (Some(locked), Some(fixture), Some(entry)) => {
                certify_runtime(compute, &root, kind, locked, fixture, entry).await
            }
            _ => Err("runtime is missing from lock, fixture manifest, or inventory".into()),
        };
        match result {
            Ok(bundle) => {
                if certified_bundle.is_none() {
                    certified_bundle = Some(bundle);
                } else {
                    let _ = fs::remove_file(&bundle.0);
                }
                report.runtimes.push(RuntimeCertification {
                    runtime: kind,
                    locked_version: locked.map(|value| value.version.clone()),
                    reported_version: entry.and_then(|value| value.detected_version.clone()),
                    executable: entry.and_then(|value| value.detected_executable.clone()),
                    source: entry.map(|value| value.source),
                    result: CertificationResult::Pass,
                    error: None,
                });
            }
            Err(error) => report.runtimes.push(RuntimeCertification {
                runtime: kind,
                locked_version: locked.map(|value| value.version.clone()),
                reported_version: entry.and_then(|value| value.detected_version.clone()),
                executable: entry.and_then(|value| value.detected_executable.clone()),
                source: entry.map(|value| value.source),
                result: CertificationResult::Fail,
                error: Some(error),
            }),
        }
    }

    if let Some((bundle, workload_id, bundle_id)) = certified_bundle {
        let appport_result = certify_appport(&root, &fixtures, &bundle, &workload_id, &bundle_id);
        let _ = fs::remove_file(&bundle);
        match appport_result {
            Ok(detail) => pass_check(&mut report, "appport_authorization", &detail),
            Err(error) => fail_check(&mut report, "appport_authorization", error),
        }
    } else {
        fail_check(
            &mut report,
            "appport_authorization",
            "no certified bundle was available for AppPort".into(),
        );
    }

    report.passed = metadata_valid
        && report
            .runtimes
            .iter()
            .all(|runtime| runtime.result == CertificationResult::Pass)
        && report
            .checks
            .iter()
            .all(|check| check.result == CertificationResult::Pass);
    report
}

async fn certify_runtime(
    compute: &Compute,
    root: &Path,
    kind: RuntimeKind,
    locked: &LockedRuntime,
    fixture: &FixtureDefinition,
    inventory: &compute_core::RuntimeInventoryEntry,
) -> Result<(PathBuf, String, String), String> {
    if !inventory.available || !inventory.compatible {
        return Err(inventory
            .remediation
            .clone()
            .unwrap_or_else(|| "runtime is unavailable".into()));
    }
    if inventory.version != locked.version || inventory.executable != locked.executable {
        return Err("descriptor differs from runtime lock".into());
    }
    if !matches!(kind, RuntimeKind::Wasm | RuntimeKind::Native)
        && inventory.source != RuntimeSource::Distribution
    {
        return Err(format!(
            "runtime provenance is {:?}, not compute-distribution",
            inventory.source
        ));
    }
    let detected = inventory
        .detected_version
        .as_deref()
        .ok_or_else(|| "runtime did not report a detected version".to_string())?;
    if !matches!(kind, RuntimeKind::Native | RuntimeKind::Wasm)
        && !detected.contains(&locked.version)
    {
        return Err(format!(
            "detected version {detected:?} does not contain locked version {}",
            locked.version
        ));
    }

    let fixture_root = root.join("certification");
    let entrypoint_source = safe_join(&fixture_root, &fixture.entrypoint)?;
    if !entrypoint_source.is_file() {
        return Err(format!(
            "certification entrypoint is missing: {}",
            entrypoint_source.display()
        ));
    }
    let workspace = tempfile::tempdir().map_err(|error| error.to_string())?;
    let entry_name = entrypoint_source
        .file_name()
        .ok_or_else(|| "fixture entrypoint has no file name".to_string())?;
    let local_entrypoint = workspace.path().join(entry_name);
    fs::copy(&entrypoint_source, &local_entrypoint).map_err(|error| error.to_string())?;

    let mut inputs = vec![WorkloadInput {
        path: PathBuf::from("hello.txt"),
        source: InputSource::Inline {
            data: b"hello".to_vec(),
        },
    }];
    let mut seen = BTreeSet::new();
    for file in &fixture.files {
        let source = safe_join(&fixture_root, file)?;
        let name = source
            .file_name()
            .ok_or_else(|| format!("fixture companion has no file name: {file}"))?;
        let local = workspace.path().join(name);
        fs::copy(&source, &local).map_err(|error| error.to_string())?;
        let destination = PathBuf::from(name);
        if !seen.insert(destination.clone()) {
            return Err(format!("duplicate fixture companion: {file}"));
        }
        inputs.push(WorkloadInput {
            path: destination.clone(),
            source: InputSource::File { path: destination },
        });
    }

    let capabilities = &inventory.capabilities;
    let network = if capabilities
        .network
        .get(&NetworkPolicy::None)
        .is_some_and(|value| value.supported)
    {
        NetworkPolicy::None
    } else {
        NetworkPolicy::Network
    };
    let mut env = BTreeMap::new();
    env.insert("CERTIFICATION_ENV".into(), "controlled".into());
    let specification = WorkloadSpec {
        version: "1".into(),
        runtime: kind,
        runtime_version: (kind == RuntimeKind::Wasm).then(|| "wasi".into()),
        entrypoint: PathBuf::from(entry_name),
        args: vec!["certify".into(), "argument-value".into()],
        env,
        inputs,
        outputs: vec![WorkloadOutput {
            path: PathBuf::from("result.json"),
            required: true,
        }],
        resources: ResourceLimits {
            wall_time: Some(Duration::from_secs(5)),
            stdout_bytes: Some(64 * 1024),
            stderr_bytes: Some(64 * 1024),
            ..ResourceLimits::default()
        },
        network,
    };
    let workload_path = workspace.path().join("workload.json");
    fs::write(
        &workload_path,
        specification
            .to_pretty_json()
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let bundle_path = workspace.path().join("workload.compute");
    let inspection = compute
        .create_bundle(&workload_path, &bundle_path)
        .map_err(|error| error.to_string())?;
    compute
        .verify_bundle(&bundle_path)
        .map_err(|error| error.to_string())?;
    if compute
        .plan_bundle(&bundle_path, Some("sha256:incorrect"), None)
        .is_ok()
        || compute
            .plan_bundle(&bundle_path, None, Some("sha256:incorrect"))
            .is_ok()
    {
        return Err("incorrect expected identity was accepted".into());
    }
    let execution = compute
        .run_bundle(
            &bundle_path,
            Some(&inspection.workload_id),
            Some(&inspection.bundle_id),
        )
        .await
        .map_err(|error| error.to_string())?;
    if execution.status != ExecutionStatus::Completed || execution.exit_code != Some(0) {
        return Err(format!("universal workload failed: {execution:?}"));
    }
    if execution.stderr.text != "certification-stderr\n" {
        return Err(format!("unexpected stderr: {:?}", execution.stderr.text));
    }
    let identity: FixtureIdentity =
        serde_json::from_str(execution.stdout.text.trim()).map_err(|error| error.to_string())?;
    if identity.runtime != kind.as_str() || identity.runtime_version != locked.version {
        return Err(format!(
            "fixture self-identification mismatch: {} {}",
            identity.runtime, identity.runtime_version
        ));
    }
    let output = execution
        .outputs
        .iter()
        .find(|output| output.path == Path::new("result.json"))
        .ok_or_else(|| "declared result.json was not collected".to_string())?;
    let semantic: SemanticOutput =
        serde_json::from_slice(&output.data).map_err(|error| error.to_string())?;
    let expected = SemanticOutput {
        input: "hello".into(),
        success: true,
        argument: "argument-value".into(),
        environment: "controlled".into(),
        host_environment: "missing".into(),
    };
    if semantic != expected {
        return Err(format!("semantic output mismatch: {semantic:?}"));
    }
    if execution.stdout.text.contains("HOST_RUNTIME_USED")
        || execution.stderr.text.contains("HOST_RUNTIME_USED")
    {
        return Err("poisoned host PATH runtime was executed".into());
    }

    let bundle = compute
        .load_bundle(&bundle_path)
        .map_err(|error| error.to_string())?;
    let mut stdin_request = bundle
        .materialize()
        .map_err(|error| error.to_string())?
        .request;
    stdin_request.args = vec!["stdin".into()];
    stdin_request.stdin = b"certification-stdin".to_vec();
    stdin_request.outputs.clear();
    let stdin_result = compute
        .run(stdin_request)
        .await
        .map_err(|error| error.to_string())?;
    if stdin_result.stdout.text != "certification-stdin" {
        return Err("stdin contract failed".into());
    }

    let mut exit_request = bundle
        .materialize()
        .map_err(|error| error.to_string())?
        .request;
    exit_request.args = vec!["exit".into()];
    exit_request.outputs.clear();
    let exit_result = compute
        .run(exit_request)
        .await
        .map_err(|error| error.to_string())?;
    if exit_result.status != ExecutionStatus::Completed || exit_result.exit_code != Some(7) {
        return Err("exit-code contract failed".into());
    }

    let mut timeout_request = bundle
        .materialize()
        .map_err(|error| error.to_string())?
        .request;
    timeout_request.args = vec!["sleep".into()];
    timeout_request.outputs.clear();
    timeout_request.resources.wall_time = Some(Duration::from_millis(50));
    let timeout_result = compute
        .run(timeout_request)
        .await
        .map_err(|error| error.to_string())?;
    if timeout_result.status != ExecutionStatus::TimedOut {
        return Err("timeout was not enforced".into());
    }

    let persistent_bundle = tempfile::Builder::new()
        .prefix(&format!("compute-certified-{}-", kind.as_str()))
        .suffix(".compute")
        .tempfile()
        .map_err(|error| error.to_string())?;
    let (_file, persistent_path) = persistent_bundle
        .keep()
        .map_err(|error| error.error.to_string())?;
    fs::copy(&bundle_path, &persistent_path).map_err(|error| error.to_string())?;
    Ok((
        persistent_path,
        inspection.workload_id,
        inspection.bundle_id,
    ))
}

fn certify_appport(
    root: &Path,
    fixtures: &FixtureManifest,
    bundle: &Path,
    workload_id: &str,
    bundle_id: &str,
) -> Result<String, String> {
    let runner = fixtures
        .appport_runner
        .as_deref()
        .ok_or_else(|| "certification manifest does not declare an AppPort runner".to_string())?;
    let runner = safe_join(&root.join("certification"), runner)?;
    let node = root.join("runtimes/node/bin/node");
    let compute = root.join("bin/compute");
    let output = Command::new(node)
        .arg(runner)
        .arg("--compute")
        .arg(compute)
        .arg("--bundle")
        .arg(bundle)
        .arg("--workload-id")
        .arg(workload_id)
        .arg("--bundle-id")
        .arg(bundle_id)
        .env_clear()
        .env("COMPUTE_HOME", root)
        .env("PATH", root.join("bin"))
        .output()
        .map_err(|error| format!("run AppPort certification: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success()
        || !stdout.contains("\"authorized\":true")
        || !stdout.contains("\"unauthorized\":true")
    {
        return Err(format!(
            "AppPort certification failed: stdout={stdout:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok("authorized bundle executed and unauthorized bundle did not start".into())
}

fn load_artifact_metadata(
    root: &Path,
) -> Result<(RuntimeLock, DistributionManifest, FixtureManifest), String> {
    let lock = read_json::<RuntimeLock>(&root.join("runtime-lock.json"))?;
    let manifest = read_json::<DistributionManifest>(&root.join("runtime-manifest.json"))?;
    let fixtures = read_json::<FixtureManifest>(&root.join("certification/fixtures.json"))?;
    Ok((lock, manifest, fixtures))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))
}

fn distribution_root() -> compute_core::Result<PathBuf> {
    if let Some(root) = std::env::var_os("COMPUTE_HOME") {
        let root = PathBuf::from(root);
        if root.join("runtime-manifest.json").is_file() {
            return Ok(root);
        }
    }
    let executable = std::env::current_exe()?;
    let root = executable
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| {
            compute_core::ComputeError::Runtime("cannot locate distribution root".into())
        })?
        .to_path_buf();
    if !root.join("runtime-manifest.json").is_file() {
        return Err(compute_core::ComputeError::Runtime(
            "compute certify requires an assembled distribution".into(),
        ));
    }
    Ok(root)
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("non-portable certification path: {relative}"));
    }
    Ok(root.join(path))
}

fn create_poisoned_path(directory: &Path) -> compute_core::Result<()> {
    for name in [
        "python", "python3", "node", "ruby", "java", "dotnet", "bun", "deno", "php", "sh",
    ] {
        let path = directory.join(name);
        fs::write(&path, "#!/bin/sh\necho HOST_RUNTIME_USED\nexit 99\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions)?;
        }
    }
    Ok(())
}

fn pass_check(report: &mut CertificationReport, name: &str, detail: &str) {
    report.checks.push(CertificationCheck {
        name: name.into(),
        result: CertificationResult::Pass,
        detail: Some(detail.into()),
    });
}

fn fail_check(report: &mut CertificationReport, name: &str, detail: String) {
    report.checks.push(CertificationCheck {
        name: name.into(),
        result: CertificationResult::Fail,
        detail: Some(detail),
    });
}

pub fn print_report(report: &CertificationReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).expect("serialize certification")
        );
        return;
    }
    println!("Compute Certification");
    println!(
        "Distribution: {}",
        report.distribution.as_deref().unwrap_or("unavailable")
    );
    println!(
        "Platform: {}",
        report.platform.as_deref().unwrap_or("unknown")
    );
    println!("Runtime\tVersion\tResult");
    for runtime in &report.runtimes {
        println!(
            "{}\t{}\t{:?}",
            runtime.runtime,
            runtime.locked_version.as_deref().unwrap_or("-"),
            runtime.result
        );
        if let Some(error) = &runtime.error {
            println!("  {error}");
        }
    }
    for check in &report.checks {
        println!("{}\t{:?}", check.name, check.result);
    }
    println!(
        "CERTIFICATION: {}",
        if report.passed { "PASS" } else { "FAIL" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_certification_paths() {
        let root = Path::new("/distribution/certification");
        assert!(safe_join(root, "python/main.py").is_ok());
        assert!(safe_join(root, "../host").is_err());
        assert!(safe_join(root, "/host").is_err());
    }

    #[test]
    fn broken_distribution_metadata_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("runtime-lock.json"), b"{}").unwrap();
        assert!(load_artifact_metadata(root.path()).is_err());
    }
}
