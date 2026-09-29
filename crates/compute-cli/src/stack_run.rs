//! A stack, realized on a Computer.
//!
//! A stack (`compute-project`) is desired state: artifacts by identity. This
//! module supplies what Compute contributes: it finds the stack, resolves its
//! components against the dependency capsule and the files supplied to the
//! workload, and — after placement has chosen a Computer — verifies the
//! components *on that Computer* with ordinary workloads (probes), each with
//! a receipt of its own. Nothing here is a special execution path: probes are
//! placed, dispatched, and receipted like any other workload.

use std::path::{Path, PathBuf};

use compute_core::{
    AppBundleBinding, AppInspection, ComponentState, ComputeError, DependencyCapsule,
    ExecutionReceipt, InputSource, IsolationProfile, IsolationRequirement, NetworkPolicy,
    ProbeEvidence, ProbeKind, ReceiptStack, ResourceLimits, RuntimeKind, StackBinding,
    WORKLOAD_SPEC_VERSION, WorkloadBundle, WorkloadDependencies, WorkloadSpec,
};
use compute_placement::SubmissionMode;
use compute_project::{
    FailureKind, PROBE, PROBE_ENTRYPOINT, ProjectError, Stack, SuppliedFile, WASM_PROBE,
    WASM_PROBE_ENTRYPOINT, find_stack, parse_inspection, parse_probe_report, probe_identity,
    wasm_probe_identity,
};
use compute_provider::ProviderRequest;

use crate::admission::PolicyLocation;
use crate::pool::{self, PlacementArtifact, PoolLocation};

fn error(error: ProjectError) -> ComputeError {
    crate::project_run::error(error)
}

/// The stack a run selects: `--stack`, else `[stack] name` in the project's
/// `compute.toml`. Names are looked up in `<project>/stacks` and the working
/// directory's `stacks`; nothing is searched upward.
pub(crate) fn select(
    explicit: Option<&str>,
    project_root: Option<&Path>,
) -> compute_core::Result<Option<Stack>> {
    let mut roots: Vec<PathBuf> = project_root.into_iter().map(Path::to_path_buf).collect();
    roots.push(std::env::current_dir()?);
    let configured = || -> Option<String> {
        let root = project_root?;
        let text = std::fs::read_to_string(root.join("compute.toml")).ok()?;
        let value: toml::Value = toml::from_str(&text).ok()?;
        value.get("stack")?.get("name")?.as_str().map(str::to_owned)
    };
    let Some(selection) = explicit.map(str::to_owned).or_else(configured) else {
        return Ok(None);
    };
    find_stack(&selection, &roots).map(Some).map_err(error)
}

/// The files a workload declares as inputs, with their portable paths.
pub(crate) fn supplied_files(
    workload: &WorkloadSpec,
    root: &Path,
) -> compute_core::Result<Vec<SuppliedFile>> {
    workload
        .inputs
        .iter()
        .map(|input| {
            let bytes = match &input.source {
                InputSource::File { path } => std::fs::read(root.join(path))?,
                InputSource::Inline { data } => data.clone(),
            };
            Ok(SuppliedFile {
                path: input.path.to_string_lossy().replace('\\', "/"),
                bytes,
            })
        })
        .collect()
}

/// The files a bundle carries as declared inputs, by portable path.
pub(crate) fn files_of_bundle(bundle: &WorkloadBundle) -> Vec<SuppliedFile> {
    bundle
        .inputs
        .iter()
        .map(|input| SuppliedFile {
            path: input.path.to_string_lossy().replace('\\', "/"),
            bytes: input.data.clone(),
        })
        .collect()
}

/// Every credential the stack references must be among the names the
/// caller supplied to the workload (`--env`, `--env-file`). The values stay
/// where the caller put them; the stack and its evidence hold names only.
pub(crate) fn require_credentials(stack: &Stack, supplied: &[String]) -> Result<(), ProjectError> {
    let missing: Vec<_> = stack
        .credentials()
        .into_iter()
        .filter(|name| !supplied.contains(name))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut failure = ProjectError::new(
        FailureKind::CredentialUnavailable,
        format!(
            "stack `{}` references credentials that were not supplied; pass them with --env or --env-file (values are never part of a stack)",
            stack.name
        ),
    );
    for name in &missing {
        failure = failure.require("credential", name.clone());
    }
    Err(failure.found("supplied", supplied.join(", ")))
}

/// Refuse a stack the run cannot honor: a component declared unsupported.
/// Nothing is dropped from the stack to make it fit. (A package no capsule
/// supplies is caught by materialization, as any unmet dependency is.)
pub(crate) fn require_realizable(binding: &StackBinding) -> Result<(), ProjectError> {
    let unsupported: Vec<_> = binding
        .components
        .iter()
        .filter_map(|component| {
            component
                .declared
                .unsupported
                .as_ref()
                .map(|reason| (&component.declared.name, reason))
        })
        .collect();
    if unsupported.is_empty() {
        return Ok(());
    }
    let mut failure = ProjectError::new(
        FailureKind::StackComponentUnsupported,
        format!(
            "stack `{}` has components Compute cannot realize",
            binding.identity.name
        ),
    );
    for (name, reason) in unsupported {
        failure = failure.require(name, reason.clone());
    }
    Err(failure)
}

fn probe_spec(
    runtime: RuntimeKind,
    entrypoint: &str,
    args: Vec<String>,
    like: Option<&WorkloadSpec>,
    capsule_id: Option<&str>,
    inputs: Vec<compute_core::WorkloadInput>,
) -> compute_core::Result<WorkloadSpec> {
    let mut spec = WorkloadSpec {
        version: WORKLOAD_SPEC_VERSION.into(),
        runtime,
        runtime_version: None,
        architecture: like.and_then(|workload| workload.architecture.clone()),
        entrypoint: PathBuf::from(entrypoint),
        args,
        env: Default::default(),
        inputs,
        outputs: vec![],
        resources: ResourceLimits::default(),
        // Process runtimes cannot enforce `none`; the probe needs no network
        // and asks for the least a Node process can be given.
        network: if runtime == RuntimeKind::Wasm {
            NetworkPolicy::None
        } else {
            NetworkPolicy::Network
        },
        isolation: IsolationRequirement {
            profile: IsolationProfile::Process,
            host: Default::default(),
        },
        dependencies: None,
    };
    if let Some(id) = capsule_id {
        spec.dependencies = Some(WorkloadDependencies {
            capsule: id.to_owned(),
        });
        spec.runtime_version = like
            .filter(|workload| workload.runtime == runtime)
            .and_then(|workload| workload.runtime_version.clone());
    }
    Ok(spec)
}

/// One probe, run as an ordinary placed execution. `provider` pins the
/// Computer; without it placement chooses, and the choice is returned.
async fn run_probe(
    location: &PoolLocation,
    policy: &PolicyLocation,
    provider: Option<&str>,
    bundle: WorkloadBundle,
) -> compute_core::Result<(String, compute_core::ExecutionResult)> {
    let mut request = ProviderRequest::bundle(bundle.to_bytes()?);
    request.expected.workload_id = Some(bundle.workload_id()?);
    request.expected.bundle_id = Some(bundle.bundle_id()?);
    request.expected.dependency_id = bundle
        .dependency_capsule
        .as_ref()
        .map(DependencyCapsule::capsule_id)
        .transpose()?;
    let artifact = PlacementArtifact {
        provider: provider.map(str::to_owned),
        ..PlacementArtifact::default()
    };
    let (pool, report, request) = pool::evaluate_bundle(
        location,
        policy,
        &artifact,
        &bundle,
        request,
        &crate::direct::DirectPlacement::default(),
        SubmissionMode::Synchronous,
    )
    .await?;
    if report.outcome != compute_placement::PlacementOutcome::Placed {
        let reasons: Vec<String> = report
            .providers
            .iter()
            .flat_map(|provider| {
                provider.reasons.iter().map(move |reason| {
                    format!(
                        "{}: {} (required {}, available {})",
                        provider.provider_id,
                        reason.code.as_str(),
                        reason.required,
                        reason.available
                    )
                })
            })
            .collect();
        return Err(error(
            ProjectError::new(
                FailureKind::NoTargetSatisfiesRequirements,
                "no Computer can run the stack's verification",
            )
            .found("reasons", reasons.join("; ")),
        ));
    }
    let selected = report
        .selected
        .as_ref()
        .expect("placed")
        .provider_id
        .clone();
    let (result, _) = pool::execute_placed(&pool, &report, request, false).await?;
    Ok((selected, result))
}

fn completed(result: &compute_core::ExecutionResult, what: &str) -> compute_core::Result<()> {
    if matches!(result.status, compute_core::ExecutionStatus::Completed)
        && result.exit_code.is_none_or(|code| code == 0)
        && result.receipt.is_some()
    {
        return Ok(());
    }
    Err(error(ProjectError::new(
        FailureKind::ExecutionFailed,
        format!(
            "the stack's {what} probe did not complete ({:?}); its components cannot be verified",
            result.status
        ),
    )))
}

/// What probing a Computer produced: evidence for the bindings, the
/// Computer used, and the probe receipt that carried the environment (its
/// capsule and the application's files).
pub(crate) struct Probed {
    pub provider: String,
    pub package_probe: Option<ProbeEvidence>,
    pub runtime_probe: Option<ProbeEvidence>,
    pub inspection: Option<AppInspection>,
    pub carrier: Option<ExecutionReceipt>,
}

/// What to verify on a Computer.
pub(crate) struct ProbePlan<'a> {
    pub stack: Option<&'a StackBinding>,
    pub app: Option<&'a AppBundleBinding>,
    /// The capsule when it travels with the probes (embedded).
    pub capsule: Option<&'a DependencyCapsule>,
    /// The capsule's identity, embedded or referenced.
    pub capsule_id: Option<&'a str>,
    /// The workload the probes accompany (its architecture and runtime
    /// constraint carry over).
    pub like: Option<&'a WorkloadSpec>,
    /// The files the workload was given.
    pub files: &'a [SuppliedFile],
    pub inputs: &'a [compute_core::WorkloadInput],
}

/// Verify on a Computer, with ordinary placed workloads: the stack's packages
/// (a probe inside the materialized capsule), the application bundle (the
/// environment's own `@appport/appboundry` inspects it), and — when an
/// application needs one — the target's runtime for it (a minimal module).
/// `provider` pins the Computer; `None` lets placement choose for the first
/// probe and pins the rest to that choice.
pub(crate) async fn probe(
    location: &PoolLocation,
    policy: &PolicyLocation,
    provider: Option<&str>,
    plan: ProbePlan<'_>,
) -> compute_core::Result<Probed> {
    let mut chosen = provider.map(str::to_owned);
    let mut package_probe = None;
    let mut runtime_probe = None;
    let mut inspection = None;
    let mut carrier = None;

    let packages: Vec<String> = plan
        .stack
        .into_iter()
        .flat_map(|stack| &stack.components)
        .map(|component| {
            let compute_core::ComponentSource::Package { package, .. } = &component.declared.source;
            package.clone()
        })
        .collect();
    let app_directory = plan
        .app
        .and_then(|app| app.resolved.as_ref())
        .filter(|_| plan.capsule_id.is_some())
        .and_then(|resolved| {
            Path::new(&resolved.manifest_path)
                .parent()
                .map(|directory| directory.to_string_lossy().into_owned())
        });

    if !packages.is_empty() || app_directory.is_some() {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join(PROBE_ENTRYPOINT), PROBE)?;
        // The application's files travel with this probe: AppBoundry inspects
        // them where they were materialized, and the receipt records them.
        let mut inputs = vec![];
        if app_directory.is_some() {
            for file in plan.files {
                let Some(input) = plan
                    .inputs
                    .iter()
                    .find(|input| input.path.to_string_lossy().replace('\\', "/") == file.path)
                else {
                    continue;
                };
                let destination = root.path().join(&file.path);
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&destination, &file.bytes)?;
                inputs.push(compute_core::WorkloadInput {
                    path: input.path.clone(),
                    source: InputSource::File {
                        path: PathBuf::from(&file.path),
                    },
                });
            }
        }
        let mut args = vec![];
        if let Some(directory) = &app_directory {
            args.push("--app".to_owned());
            args.push(if directory.is_empty() {
                ".".into()
            } else {
                directory.clone()
            });
        }
        args.extend(packages.iter().cloned());
        let spec = probe_spec(
            RuntimeKind::Node,
            PROBE_ENTRYPOINT,
            args,
            plan.like,
            plan.capsule_id,
            inputs,
        )?;
        let bundle =
            WorkloadBundle::create_from_with_capsule(spec, root.path(), plan.capsule.cloned())?;
        let (selected, result) = run_probe(location, policy, chosen.as_deref(), bundle).await?;
        completed(&result, "environment")?;
        chosen = Some(selected);
        let receipt = result.receipt.clone().expect("checked");
        if let Some(stack) = plan.stack {
            package_probe = Some(ProbeEvidence {
                kind: ProbeKind::PackageVersions,
                receipt: receipt.receipt_hash.to_string(),
                program: probe_identity(),
                runtime: receipt.runtime.observed,
                observed: parse_probe_report(&result.stdout.text, stack),
            });
        }
        if app_directory.is_some() {
            inspection = parse_inspection(&result.stdout.text, receipt.receipt_hash.as_str());
        }
        carrier = Some(receipt);
    }
    if let Some(app) = plan.app {
        let kind = app.declared.runtime;
        if kind == RuntimeKind::Wasm {
            let root = tempfile::tempdir()?;
            std::fs::write(root.path().join(WASM_PROBE_ENTRYPOINT), WASM_PROBE)?;
            let spec = probe_spec(kind, WASM_PROBE_ENTRYPOINT, vec![], plan.like, None, vec![])?;
            let bundle = WorkloadBundle::create_from_with_capsule(spec, root.path(), None)?;
            let (selected, result) = run_probe(location, policy, chosen.as_deref(), bundle).await?;
            completed(&result, "WASM runtime")?;
            chosen = Some(selected);
            let receipt = result.receipt.clone().expect("checked");
            runtime_probe = Some(ProbeEvidence {
                kind: ProbeKind::WasmRuntime,
                receipt: receipt.receipt_hash.to_string(),
                program: wasm_probe_identity(),
                runtime: receipt.runtime.observed,
                observed: [(
                    "wasm-runtime".to_owned(),
                    Some(receipt.runtime.version.clone()),
                )]
                .into_iter()
                .collect(),
            });
            if carrier.is_none() {
                carrier = Some(receipt);
            }
        }
    }
    Ok(Probed {
        provider: chosen.ok_or_else(|| {
            error(ProjectError::new(
                FailureKind::StackInvalid,
                "there is nothing to verify",
            ))
        })?,
        package_probe,
        runtime_probe,
        inspection,
        carrier,
    })
}

// ---- `compute up --stack` -------------------------------------------------

/// A component's name, where it stands, and the checks behind that.
type ComponentReport = (String, ComponentState, Vec<(String, String)>);

pub(crate) struct UpStack {
    pub selection: String,
    pub deps: Option<PathBuf>,
    pub by_reference: bool,
}

/// Realize the stack on a Computer and report what is actually true of it.
/// Returns whether nothing failed and nothing is unresolved.
pub(crate) async fn up(
    request: UpStack,
    location: &PoolLocation,
    policy: &PolicyLocation,
) -> compute_core::Result<bool> {
    let cwd = std::env::current_dir()?;
    let stack = select(Some(&request.selection), Some(&cwd))?.expect("selected");
    let capsule = request
        .deps
        .as_deref()
        .map(DependencyCapsule::read)
        .transpose()?;
    let mut binding = stack.bind(capsule.as_ref()).map_err(error)?;
    let capsule_id = binding.capsule_id.clone();
    let embedded = capsule.as_ref().filter(|_| !request.by_reference);
    let identity = binding.identity.clone();
    println!(
        "Stack: {} {} ({})",
        identity.name, identity.version, identity.fingerprint
    );

    let mut carrier = None;
    let mut target = None;
    let unresolved = binding.components.iter().any(|c| c.resolved.is_none());
    let unsupported = binding
        .components
        .iter()
        .any(|c| c.declared.unsupported.is_some());
    if unresolved || unsupported {
        eprintln!(
            "nothing was placed: the stack is not resolvable from what was supplied (--deps)"
        );
    } else {
        match probe(
            location,
            policy,
            None,
            ProbePlan {
                stack: Some(&binding),
                app: None,
                capsule: embedded,
                capsule_id: capsule_id.as_deref(),
                like: None,
                files: &[],
                inputs: &[],
            },
        )
        .await
        {
            Ok(probed) => {
                target = Some(probed.provider);
                binding.probes = probed.package_probe.into_iter().collect();
                carrier = probed.carrier;
            }
            Err(failure) => eprintln!("verification did not run: {failure}"),
        }
    }
    // With no probe receipt there is no execution to attest from; report
    // the declared/resolved state only.
    let states: Vec<ComponentReport> = match &carrier {
        Some(receipt) => {
            let evidence = ReceiptStack::attest(&binding, receipt);
            binding
                .components
                .iter()
                .zip(&evidence.components)
                .map(|(component, evidence)| {
                    (
                        component.declared.name.clone(),
                        evidence.state(&component.declared),
                        evidence
                            .gates
                            .iter()
                            .map(|gate| {
                                (
                                    gate.check.clone(),
                                    format!(
                                        "{:?}: {}",
                                        gate.verification.status, gate.verification.evidence
                                    ),
                                )
                            })
                            .collect(),
                    )
                })
                .collect()
        }
        None => binding
            .components
            .iter()
            .map(|component| {
                let state = if component.declared.unsupported.is_some() {
                    ComponentState::Unsupported
                } else if component.resolved.is_some() {
                    ComponentState::Resolved
                } else {
                    ComponentState::Declared
                };
                (component.declared.name.clone(), state, vec![])
            })
            .collect(),
    };
    let ready = states
        .iter()
        .all(|(_, state, _)| *state == ComponentState::Verified);
    println!(
        "Target: {}",
        target.as_deref().unwrap_or("none (nothing was placed)")
    );
    println!(
        "Status: {}",
        if ready {
            "ready for applications"
        } else {
            "not ready"
        }
    );
    for (name, state, gates) in &states {
        let mark = match state {
            ComponentState::Verified => "✓",
            ComponentState::VerificationUnavailable => "?",
            ComponentState::Failed => "✗",
            ComponentState::Unsupported => "–",
            _ => "·",
        };
        println!("  {mark} {name:<18} {}", state.label());
        for (check, detail) in gates {
            println!("      {check}: {detail}");
        }
    }
    Ok(ready)
}
