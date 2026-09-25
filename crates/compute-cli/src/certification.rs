use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use compute_core::{
    DependencyCapsule, ExecutionStatus, InputSource, NetworkPolicy, PlatformIdentity,
    ResourceLimits, RuntimeKind, RuntimeSource, WorkloadBundle, WorkloadDependencies,
    WorkloadInput, WorkloadOutput, WorkloadSpec,
};
use compute_provider::{ComputeProvider, ProviderRequest, RemoteProvider, ServerConfig};
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
    pub resource_controls: ResourceCertification,
    pub network_policy: String,
    pub filesystem_isolation: String,
    pub result: CertificationResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceCertification {
    pub timeout: String,
    pub memory: String,
    pub cpu: String,
    pub process_count: String,
}

struct CertifiedArtifact {
    bundle: PathBuf,
    workload_id: String,
    bundle_id: String,
    resources: ResourceCertification,
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
    schema_version: u32,
    compute_version: String,
    distribution_id: String,
    distribution_version: String,
    platform: String,
    os: String,
    architecture: String,
    runtime_lock_sha256: String,
    certification_status: String,
    build: serde_json::Value,
    runtimes: BTreeMap<String, ManifestRuntime>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LockedRuntime {
    version: String,
    executable: String,
    #[serde(default)]
    artifacts: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ManifestRuntime {
    version: String,
    executable: String,
    artifact_sha256: String,
    payload_sha256: String,
    reported_version: String,
    #[serde(default)]
    capabilities: Option<compute_core::RuntimeCapabilities>,
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

    let metadata_valid = lock.schema_version == 2
        && manifest.schema_version == 2
        && fixtures.schema_version == 1
        && lock.runtimes.iter().all(|(name, locked)| {
            manifest.runtimes.get(name).is_some_and(|installed| {
                installed.version == locked.version && installed.executable == locked.executable
            })
        })
        && manifest.compute_version == env!("CARGO_PKG_VERSION")
        && manifest.distribution_id.starts_with("sha256:")
        && manifest.platform == format!("{}-{}", manifest.os, manifest.architecture)
        && manifest.runtime_lock_sha256.len() == 64
        && matches!(manifest.certification_status.as_str(), "not_run" | "pass")
        && !manifest.build.is_null();
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

    match certify_negative_contracts() {
        Ok(detail) => pass_check(&mut report, "security_and_negative_cases", &detail),
        Err(error) => fail_check(&mut report, "security_and_negative_cases", error),
    }

    let inventory = match compute.inventory().await {
        Ok(inventory) => inventory,
        Err(error) => {
            fail_check(&mut report, "runtime_inventory", error.to_string());
            return report;
        }
    };
    let remote_listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(error) => {
            fail_check(&mut report, "remote_provider", error.to_string());
            return report;
        }
    };
    let remote_addr = remote_listener
        .local_addr()
        .expect("bound listener has an address");
    let remote_endpoint = format!("http://{remote_addr}");
    let remote_server_endpoint = remote_endpoint.clone();
    let remote_job_store = match tempfile::tempdir() {
        Ok(directory) => directory,
        Err(error) => {
            fail_check(&mut report, "remote_provider", error.to_string());
            return report;
        }
    };
    let remote_job_store_path = remote_job_store.path().to_path_buf();
    let remote_server = tokio::spawn(async move {
        let mut config = ServerConfig::local(remote_server_endpoint);
        config.job_store = remote_job_store_path;
        let _ = compute_provider::serve_listener(remote_listener, config).await;
    });
    let placement_harness = match compute.installed_distribution_identity() {
        Ok(distribution) => {
            crate::placement_certification::PlacementHarness::start(
                &remote_endpoint,
                distribution.id,
            )
            .await
        }
        Err(error) => Err(error.to_string()),
    };
    let mut placement_results: Vec<(RuntimeKind, Result<(), String>)> = vec![];
    let remote_provider = RemoteProvider::new(remote_endpoint);
    let mut certified_bundle: Option<CertifiedArtifact> = None;
    for kind in RuntimeKind::ALL {
        let locked = lock.runtimes.get(kind.as_str());
        let fixture = fixtures.runtimes.get(kind.as_str());
        let entry = inventory.runtimes.iter().find(|item| item.id == kind);
        let result = match (locked, fixture, entry) {
            (Some(locked), Some(fixture), Some(entry)) => {
                certify_runtime(
                    compute,
                    &remote_provider,
                    &root,
                    kind,
                    locked,
                    fixture,
                    entry,
                )
                .await
            }
            _ => Err("runtime is missing from lock, fixture manifest, or inventory".into()),
        };
        match result {
            Ok(artifact) => {
                placement_results.push((
                    kind,
                    match &placement_harness {
                        Ok(harness) => harness.certify_runtime(kind, &artifact.bundle).await,
                        Err(error) => Err(error.clone()),
                    },
                ));
                let resource_controls = artifact.resources.clone();
                if certified_bundle.is_none() {
                    certified_bundle = Some(artifact);
                } else {
                    let _ = fs::remove_file(&artifact.bundle);
                }
                report.runtimes.push(RuntimeCertification {
                    runtime: kind,
                    locked_version: locked.map(|value| value.version.clone()),
                    reported_version: entry.and_then(|value| value.detected_version.clone()),
                    executable: entry.and_then(|value| value.detected_executable.clone()),
                    source: entry.map(|value| value.source),
                    resource_controls,
                    network_policy: if entry.is_some_and(|value| {
                        value
                            .capabilities
                            .network
                            .get(&NetworkPolicy::None)
                            .is_some_and(|capability| capability.supported)
                    }) {
                        "denied_policy_executed"
                    } else {
                        "unsupported_policy_rejected"
                    }
                    .into(),
                    filesystem_isolation: if entry
                        .is_some_and(|value| value.capabilities.filesystem_isolation.supported)
                    {
                        "supported_and_enforced"
                    } else {
                        "not_supported"
                    }
                    .into(),
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
                resource_controls: resource_evidence(entry, false, false),
                network_policy: "failed".into(),
                filesystem_isolation: "failed".into(),
                result: CertificationResult::Fail,
                error: Some(error),
            }),
        }
    }
    let placement_failures = match (&placement_harness, &certified_bundle) {
        (Ok(harness), Some(artifact)) => harness.certify_failures(&artifact.bundle).await,
        (Err(error), _) => Err(error.clone()),
        (_, None) => Err("no certified bundle was available for placement".into()),
    };
    drop(placement_harness);
    remote_server.abort();
    let placement_errors = placement_results
        .iter()
        .filter_map(|(kind, result)| {
            result
                .as_ref()
                .err()
                .map(|error| format!("{kind}: {error}"))
        })
        .collect::<Vec<_>>();
    if placement_results.len() == RuntimeKind::ALL.len() && placement_errors.is_empty() {
        pass_check(
            &mut report,
            "provider_pool",
            "local and remote providers with different capabilities formed one pool; every runtime executed on its selected provider with placement bound into a verified receipt",
        );
        pass_check(
            &mut report,
            "placement",
            "requirements, capability discovery, compatibility, deterministic selection, execution, and receipt agreed for every runtime; incompatible explicit providers failed closed",
        );
    } else {
        let detail = if placement_errors.is_empty() {
            "placement was not certified for every runtime".to_string()
        } else {
            placement_errors.join("; ")
        };
        fail_check(&mut report, "provider_pool", detail.clone());
        fail_check(&mut report, "placement", detail);
    }
    match placement_failures {
        Ok(detail) => pass_check(&mut report, "placement_failures", &detail),
        Err(error) => fail_check(&mut report, "placement_failures", error),
    }
    match &certified_bundle {
        Some(artifact) => {
            for (name, result) in crate::policy_certification::certify(&artifact.bundle)
                .await
                .checks
            {
                match result {
                    Ok(detail) => pass_check(&mut report, name, &detail),
                    Err(error) => fail_check(&mut report, name, error),
                }
            }
        }
        None => {
            for name in [
                "policy",
                "admission",
                "placement_policy",
                "remote_job_policy",
                "receipt_policy",
            ] {
                fail_check(
                    &mut report,
                    name,
                    "no certified bundle was available for policy certification".into(),
                );
            }
        }
    }
    if report
        .runtimes
        .iter()
        .all(|runtime| runtime.result == CertificationResult::Pass)
    {
        pass_check(
            &mut report,
            "remote_provider",
            "every runtime passed client-to-transport-to-server execution equivalence",
        );
    } else {
        fail_check(
            &mut report,
            "remote_provider",
            "one or more remote runtime executions failed".into(),
        );
    }

    let all_runtimes_pass = report
        .runtimes
        .iter()
        .all(|runtime| runtime.result == CertificationResult::Pass);
    for (name, detail) in [
        (
            "workload_bundle",
            "all runtime fixtures executed from verified .compute bundles",
        ),
        (
            "input_output_contract",
            "all runtimes produced the canonical semantic result",
        ),
        (
            "identity_verification",
            "correct identities ran and incorrect identities were rejected",
        ),
        (
            "timeout_enforcement",
            "all runtime processes were terminated at their deadline",
        ),
        (
            "host_path_leakage",
            "poisoned PATH executables were not used",
        ),
    ] {
        if all_runtimes_pass {
            pass_check(&mut report, name, detail);
        } else {
            fail_check(
                &mut report,
                name,
                "one or more runtime certifications failed".into(),
            );
        }
    }

    if let Some(artifact) = certified_bundle {
        let appport_result = certify_appport(
            &root,
            &fixtures,
            &artifact.bundle,
            &artifact.workload_id,
            &artifact.bundle_id,
        );
        let _ = fs::remove_file(&artifact.bundle);
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
    remote_provider: &RemoteProvider,
    root: &Path,
    kind: RuntimeKind,
    locked: &LockedRuntime,
    fixture: &FixtureDefinition,
    inventory: &compute_core::RuntimeInventoryEntry,
) -> Result<CertifiedArtifact, String> {
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
    let dependency_capsule = if matches!(kind, RuntimeKind::Python | RuntimeKind::Node) {
        use std::io::Write;
        let mut entrypoint = fs::OpenOptions::new()
            .append(true)
            .open(&local_entrypoint)
            .map_err(|error| error.to_string())?;
        match kind {
            RuntimeKind::Python => writeln!(
                entrypoint,
                "\nimport certification_dependency\nassert certification_dependency.VALUE == 'packaged'"
            ),
            RuntimeKind::Node => writeln!(
                entrypoint,
                "\nif (require('certification_dependency').value !== 'packaged') process.exit(91);"
            ),
            _ => unreachable!(),
        }
        .map_err(|error| error.to_string())?;
        let payload = workspace.path().join("resolved-dependencies");
        fs::create_dir(&payload).map_err(|error| error.to_string())?;
        match kind {
            RuntimeKind::Python => fs::write(
                payload.join("certification_dependency.py"),
                "VALUE = 'packaged'\n",
            ),
            RuntimeKind::Node => {
                let module = payload.join("certification_dependency");
                fs::create_dir(&module).map_err(|error| error.to_string())?;
                fs::write(module.join("index.js"), "exports.value = 'packaged';\n")
            }
            _ => unreachable!(),
        }
        .map_err(|error| error.to_string())?;
        Some(
            DependencyCapsule::create(
                &payload,
                kind,
                Some(locked.version.clone()),
                PlatformIdentity::current(),
                vec![],
                None,
            )
            .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };

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
    if capabilities.cpu_limit.supported || capabilities.process_limit.supported {
        return Err("runtime declares CPU or process limits without a certification probe".into());
    }
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
    env.insert("CERTIFICATION_VERSION".into(), locked.version.clone());
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
        isolation: compute_core::IsolationRequirement::default(),
        dependencies: dependency_capsule
            .as_ref()
            .map(|capsule| {
                Ok(WorkloadDependencies {
                    capsule: capsule.capsule_id()?,
                })
            })
            .transpose()
            .map_err(|error: compute_core::ComputeError| error.to_string())?,
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
    let inspection = match dependency_capsule {
        Some(capsule) => {
            compute.create_bundle_with_dependencies(&workload_path, &bundle_path, capsule)
        }
        None => compute.create_bundle(&workload_path, &bundle_path),
    }
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
    let process_evidence = execution
        .isolation
        .as_ref()
        .ok_or_else(|| "execution omitted isolation evidence".to_string())?;
    if process_evidence.requested != compute_core::IsolationProfile::Process
        || process_evidence.effective != compute_core::IsolationProfile::Process
    {
        return Err("process isolation evidence is incorrect".into());
    }
    if !capabilities.isolation.filesystem_boundary
        && process_evidence.filesystem != compute_core::BoundaryStatus::Unavailable
    {
        return Err("process runtime overstated its filesystem boundary".into());
    }
    if execution.receipt.as_ref().map(|receipt| &receipt.isolation) != Some(process_evidence) {
        return Err("receipt isolation evidence differs from the execution".into());
    }
    if matches!(kind, RuntimeKind::Python | RuntimeKind::Node)
        && execution
            .dependencies
            .as_ref()
            .is_none_or(|dependencies| !dependencies.verified)
    {
        return Err(format!(
            "{kind} dependency capsule was not verified during certification"
        ));
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

    let mut remote_request =
        ProviderRequest::bundle(fs::read(&bundle_path).map_err(|error| error.to_string())?);
    remote_request.expected.workload_id = Some(inspection.workload_id.clone());
    remote_request.expected.bundle_id = Some(inspection.bundle_id.clone());
    if let Some(dependencies) = &execution.dependencies {
        remote_request.expected.dependency_id = Some(dependencies.capsule_id.clone());
    }
    let remote = remote_provider
        .execute(remote_request.clone())
        .await
        .map_err(|error| format!("remote provider execution failed: {error}"))?
        .result;
    if remote.runtime != execution.runtime
        || remote.status != execution.status
        || remote.exit_code != execution.exit_code
        || remote.stdout != execution.stdout
        || remote.stderr != execution.stderr
        || remote.outputs != execution.outputs
        || remote.dependencies != execution.dependencies
    {
        return Err("local and remote execution results differ".into());
    }
    let remote_receipt = remote
        .receipt
        .ok_or_else(|| "remote provider omitted its receipt".to_string())?;
    if remote_receipt.provider_protocol.as_deref() != Some(compute_provider::REMOTE_PROTOCOL) {
        return Err("remote receipt omitted provider protocol binding".into());
    }
    remote_receipt.verify().map_err(|error| error.to_string())?;

    let submission = remote_provider
        .submit(remote_request, None)
        .await
        .map_err(|error| format!("asynchronous provider submission failed: {error}"))?;
    let async_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let asynchronous = loop {
        let job = remote_provider
            .job_status(&submission.job_id.0)
            .await
            .map_err(|error| format!("asynchronous provider status failed: {error}"))?;
        if job.status.is_terminal() {
            break remote_provider
                .job_result(&submission.job_id.0)
                .await
                .map_err(|error| format!("asynchronous provider result failed: {error}"))?;
        }
        if tokio::time::Instant::now() >= async_deadline {
            return Err("asynchronous provider job did not become terminal".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    if asynchronous.status != compute_core::JobStatus::Succeeded
        || asynchronous.result.runtime != execution.runtime
        || asynchronous.result.status != execution.status
        || asynchronous.result.exit_code != execution.exit_code
        || asynchronous.result.stdout != execution.stdout
        || asynchronous.result.stderr != execution.stderr
        || asynchronous.result.outputs != execution.outputs
        || asynchronous.result.dependencies != execution.dependencies
    {
        return Err("local and asynchronous remote execution results differ".into());
    }
    remote_provider
        .job_receipt(&submission.job_id.0)
        .await
        .map_err(|error| format!("asynchronous receipt retrieval failed: {error}"))?;
    remote_provider
        .job_artifacts(&submission.job_id.0)
        .await
        .map_err(|error| format!("asynchronous artifact retrieval failed: {error}"))?;

    let bundle = compute
        .load_bundle(&bundle_path)
        .map_err(|error| error.to_string())?;
    let supports_stronger_isolation = capabilities.isolation.filesystem_boundary
        && capabilities.isolation.network_boundary
        && capabilities.isolation.environment_boundary
        && capabilities.isolation.timeout_enforcement;
    if supports_stronger_isolation {
        for profile in [
            compute_core::IsolationProfile::Sandboxed,
            compute_core::IsolationProfile::Strict,
        ] {
            let mut isolated = bundle.materialize().map_err(|error| error.to_string())?;
            isolated.request.isolation = profile;
            let result = compute
                .run(isolated.request)
                .await
                .map_err(|error| format!("{profile} isolation certification failed: {error}"))?;
            let evidence = result
                .isolation
                .ok_or_else(|| format!("{profile} execution omitted isolation evidence"))?;
            if evidence.requested != profile || evidence.effective != profile {
                return Err(format!("{profile} isolation evidence is incorrect"));
            }
        }
    }
    let mut stdin_materialized = bundle.materialize().map_err(|error| error.to_string())?;
    let stdin_request = &mut stdin_materialized.request;
    stdin_request.args = vec!["stdin".into()];
    stdin_request.stdin = b"certification-stdin".to_vec();
    stdin_request.outputs.clear();
    let stdin_result = compute
        .run(stdin_request.clone())
        .await
        .map_err(|error| error.to_string())?;
    if stdin_result.stdout.text != "certification-stdin" {
        return Err("stdin contract failed".into());
    }

    if capabilities.filesystem_isolation.supported {
        let mut filesystem_materialized =
            bundle.materialize().map_err(|error| error.to_string())?;
        let filesystem_request = &mut filesystem_materialized.request;
        filesystem_request.args = vec!["filesystem".into()];
        filesystem_request.outputs.clear();
        if supports_stronger_isolation {
            filesystem_request.isolation = compute_core::IsolationProfile::Strict;
        }
        let filesystem_result = compute
            .run(filesystem_request.clone())
            .await
            .map_err(|error| error.to_string())?;
        if filesystem_result.stdout.text.trim() != "blocked" {
            return Err("host filesystem probe was not blocked".into());
        }
    }

    if !capabilities
        .network
        .get(&NetworkPolicy::None)
        .is_some_and(|value| value.supported)
    {
        let mut network_materialized = bundle.materialize().map_err(|error| error.to_string())?;
        let denied_network = &mut network_materialized.request;
        denied_network.network = NetworkPolicy::None;
        denied_network.outputs.clear();
        if compute.run(denied_network.clone()).await.is_ok() {
            return Err("unsupported denied-network policy was silently accepted".into());
        }
    } else {
        let mut network_materialized = bundle.materialize().map_err(|error| error.to_string())?;
        let denied_network = &mut network_materialized.request;
        denied_network.args = vec!["network".into()];
        denied_network.outputs.clear();
        if supports_stronger_isolation {
            denied_network.isolation = compute_core::IsolationProfile::Strict;
        }
        let network_result = compute
            .run(denied_network.clone())
            .await
            .map_err(|error| error.to_string())?;
        if network_result.stdout.text.trim() != "blocked" {
            return Err("denied network probe was not blocked".into());
        }
    }

    let mut exit_materialized = bundle.materialize().map_err(|error| error.to_string())?;
    let exit_request = &mut exit_materialized.request;
    exit_request.args = vec!["exit".into()];
    exit_request.outputs.clear();
    let exit_result = compute
        .run(exit_request.clone())
        .await
        .map_err(|error| error.to_string())?;
    if exit_result.status != ExecutionStatus::Completed || exit_result.exit_code != Some(7) {
        return Err("exit-code contract failed".into());
    }

    let mut timeout_materialized = bundle.materialize().map_err(|error| error.to_string())?;
    let timeout_request = &mut timeout_materialized.request;
    timeout_request.args = vec!["sleep".into()];
    timeout_request.outputs.clear();
    timeout_request.resources.wall_time = Some(Duration::from_millis(50));
    let timeout_result = compute
        .run(timeout_request.clone())
        .await
        .map_err(|error| error.to_string())?;
    if timeout_result.status != ExecutionStatus::TimedOut {
        return Err("timeout was not enforced".into());
    }

    let memory = if capabilities.memory_limit.supported {
        let mut memory_materialized = bundle.materialize().map_err(|error| error.to_string())?;
        let memory_request = &mut memory_materialized.request;
        memory_request.args = vec!["memory".into()];
        memory_request.outputs.clear();
        memory_request.resources.memory_bytes = Some(4 * 65_536);
        if let Ok(memory_result) = compute.run(memory_request.clone()).await
            && memory_result.status == ExecutionStatus::Completed
            && memory_result.exit_code == Some(0)
        {
            return Err("declared memory limit was not enforced".into());
        }
        "supported_and_enforced"
    } else {
        "not_supported"
    };

    let persistent_bundle = tempfile::Builder::new()
        .prefix(&format!("compute-certified-{}-", kind.as_str()))
        .suffix(".compute")
        .tempfile()
        .map_err(|error| error.to_string())?;
    let (_file, persistent_path) = persistent_bundle
        .keep()
        .map_err(|error| error.error.to_string())?;
    fs::copy(&bundle_path, &persistent_path).map_err(|error| error.to_string())?;
    Ok(CertifiedArtifact {
        bundle: persistent_path,
        workload_id: inspection.workload_id,
        bundle_id: inspection.bundle_id,
        resources: ResourceCertification {
            timeout: "supported_and_enforced".into(),
            memory: memory.into(),
            cpu: if capabilities.cpu_limit.supported {
                "supported_not_certified"
            } else {
                "not_supported"
            }
            .into(),
            process_count: if capabilities.process_limit.supported {
                "supported_not_certified"
            } else {
                "not_supported"
            }
            .into(),
        },
    })
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

fn certify_negative_contracts() -> Result<String, String> {
    let root = tempfile::tempdir().map_err(|error| error.to_string())?;
    fs::write(root.path().join("entry.py"), "print('must not execute')\n")
        .map_err(|error| error.to_string())?;
    let base = WorkloadSpec {
        version: "1".into(),
        runtime: RuntimeKind::Python,
        runtime_version: None,
        entrypoint: PathBuf::from("entry.py"),
        args: vec![],
        env: BTreeMap::new(),
        inputs: vec![],
        outputs: vec![],
        resources: ResourceLimits::default(),
        network: NetworkPolicy::Network,
        isolation: compute_core::IsolationRequirement::default(),
        dependencies: None,
    };
    let mut traversal = base.clone();
    traversal.inputs.push(WorkloadInput {
        path: PathBuf::from("../secret"),
        source: InputSource::Inline { data: vec![] },
    });
    let mut absolute = base.clone();
    absolute.outputs.push(WorkloadOutput {
        path: PathBuf::from("/tmp/result"),
        required: true,
    });
    let mut output_traversal = base.clone();
    output_traversal.outputs.push(WorkloadOutput {
        path: PathBuf::from("../../result"),
        required: true,
    });
    if traversal.validate().is_ok()
        || absolute.validate().is_ok()
        || output_traversal.validate().is_ok()
    {
        return Err("unsafe input or output path was accepted".into());
    }

    let workload_path = root.path().join("workload.json");
    fs::write(
        &workload_path,
        base.to_pretty_json().map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let bundle = WorkloadBundle::create(&workload_path).map_err(|error| error.to_string())?;
    let mut bytes = bundle.to_bytes().map_err(|error| error.to_string())?;
    let last = bytes
        .last_mut()
        .ok_or_else(|| "empty certification bundle".to_string())?;
    *last ^= 0xff;
    if WorkloadBundle::from_bytes(&bytes).is_ok() {
        return Err("tampered bundle was accepted".into());
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("../outside", root.path().join("escape"))
            .map_err(|error| error.to_string())?;
        let mut symlink = base;
        symlink.inputs.push(WorkloadInput {
            path: PathBuf::from("data.txt"),
            source: InputSource::File {
                path: PathBuf::from("escape"),
            },
        });
        fs::write(
            &workload_path,
            symlink
                .to_pretty_json()
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if WorkloadBundle::create(&workload_path).is_ok() {
            return Err("symlink escape was accepted".into());
        }
    }
    Ok("traversal, absolute paths, symlink escape, and tampered bundles were rejected".into())
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

fn resource_evidence(
    entry: Option<&compute_core::RuntimeInventoryEntry>,
    timeout_enforced: bool,
    memory_enforced: bool,
) -> ResourceCertification {
    let capabilities = entry.map(|value| &value.capabilities);
    let evidence = |supported: bool, enforced: bool| {
        if !supported {
            "not_supported"
        } else if enforced {
            "supported_and_enforced"
        } else {
            "failed"
        }
        .to_string()
    };
    ResourceCertification {
        timeout: evidence(
            capabilities.is_some_and(|value| value.timeout.supported),
            timeout_enforced,
        ),
        memory: evidence(
            capabilities.is_some_and(|value| value.memory_limit.supported),
            memory_enforced,
        ),
        cpu: if capabilities.is_some_and(|value| value.cpu_limit.supported) {
            "supported_not_certified"
        } else {
            "not_supported"
        }
        .into(),
        process_count: if capabilities.is_some_and(|value| value.process_limit.supported) {
            "supported_not_certified"
        } else {
            "not_supported"
        }
        .into(),
    }
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
            "{}\t{}\t{}",
            runtime.runtime,
            runtime.locked_version.as_deref().unwrap_or("-"),
            result_label(runtime.result)
        );
        if let Some(error) = &runtime.error {
            println!("  {error}");
        }
    }
    for check in &report.checks {
        println!("{}\t{}", check.name, result_label(check.result));
    }
    println!(
        "CERTIFICATION: {}",
        if report.passed { "PASS" } else { "FAIL" }
    );
}

fn result_label(result: CertificationResult) -> &'static str {
    match result {
        CertificationResult::Pass => "PASS",
        CertificationResult::Fail => "FAIL",
    }
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
