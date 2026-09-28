//! A PAX project as a Compute workload: discovery, requirement resolution,
//! and the failure report for a project no target can run.
//!
//! PAX describes what the project requires; this module asks the adapter
//! (`compute-project`) for those requirements, lets Compute's own resolver
//! turn them into a workload, materializes the environment they describe,
//! and hands placement a request that carries the project. Placement then
//! decides where and how it runs.

use compute_core::{ComputeError, ProjectBinding, RuntimeKind};
use compute_placement::{PlacementReport, ReasonCode};
use compute_project::{
    CommandSelection, EnvironmentInputs, FailureKind, PaxExecutable, ProjectError,
};

use crate::direct;
use crate::pool::PlacementArtifact;

pub(crate) struct ProjectRun {
    pub resolved: direct::ResolvedDirect,
    pub binding: ProjectBinding,
}

pub(crate) fn error(error: ProjectError) -> ComputeError {
    ComputeError::InvalidWorkload(error.to_string())
}

/// Discover the project named by `artifact.project` and resolve it, with
/// Compute's own configuration layered over what PAX declared.
pub(crate) fn resolve(
    artifact: &PlacementArtifact,
    policy: &crate::admission::PolicyLocation,
) -> compute_core::Result<ProjectRun> {
    let project = artifact.project.as_deref().expect("project mode");
    let discovered =
        compute_project::discover(project, &PaxExecutable::from_environment()).map_err(error)?;
    let selection = match &artifact.project_command {
        Some(name) => CommandSelection::Named(name.clone()),
        None => CommandSelection::Default,
    };
    let selected =
        compute_project::select_command(&discovered.requirements, &selection).map_err(error)?;

    // An explicit --runtime is checked against the project's requirements
    // by `materialize`; it is never silently reconciled.
    let runtime = match artifact.runtime.as_deref() {
        Some(value) => value.parse::<RuntimeKind>()?,
        None => selected
            .runtime
            .or(discovered
                .requirements
                .runtimes
                .first()
                .map(|need| need.kind))
            .ok_or_else(|| {
                error(ProjectError::new(
                    FailureKind::RequirementsUnresolved,
                    "the project requires no runtime Compute can run",
                ))
            })?,
    };
    let mut args = selected.args.clone();
    args.extend(artifact.args.iter().cloned());
    let resolved = direct::resolve(direct::DirectOptions {
        path: discovered.root.clone(),
        runtime: Some(runtime.to_string()),
        args,
        env: artifact.env.clone(),
        env_file: artifact.env_file.clone(),
        inputs: artifact.inputs.clone(),
        outputs: artifact.outputs.clone(),
        cwd: artifact.cwd.clone(),
        entrypoint: artifact.entrypoint.clone().or(selected.entrypoint.clone()),
        deps: artifact.deps.clone(),
        network: artifact.network.clone(),
        isolation: artifact.isolation,
        memory: artifact.memory,
        timeout: artifact.timeout,
        defaults: policy.defaults()?,
    })?;

    let workload = &resolved.workload;
    let environment_names: Vec<String> = workload.env.keys().cloned().collect();
    let architecture = workload
        .architecture
        .clone()
        .or(artifact.platform.as_ref().map(|p| p.architecture.clone()));
    let binding = compute_project::materialize(
        &discovered.requirements,
        &EnvironmentInputs {
            runtime: workload.runtime,
            runtime_constraint: workload.runtime_version.as_deref(),
            os: artifact.platform.as_ref().map(|p| p.os.as_str()),
            architecture: architecture.as_deref(),
            capsule: resolved.dependency_capsule.as_ref(),
            environment_names,
            entrypoint: workload.entrypoint.to_string_lossy().replace('\\', "/"),
            argument_count: workload.args.len(),
            command_name: selected.name.clone(),
            offered_tools: &[],
        },
    )
    .map_err(error)?;
    Ok(ProjectRun { resolved, binding })
}

/// Why no target can run the project: what it requires, what each target
/// offers, and `unsupported`. The failure kind follows the dimension that
/// excluded every target.
pub(crate) fn placement_failure(
    binding: &ProjectBinding,
    report: &PlacementReport,
) -> ProjectError {
    let dimensions: Vec<&str> = report
        .providers
        .iter()
        .flat_map(|provider| provider.reasons.iter())
        .map(|reason| reason.code.dimension())
        .collect();
    let only = |wanted: &str| {
        !dimensions.is_empty() && dimensions.iter().all(|dimension| *dimension == wanted)
    };
    let kind = if only("runtime") {
        FailureKind::RuntimeUnavailable
    } else if only("dependencies") {
        FailureKind::DependencyUnavailable
    } else {
        FailureKind::NoTargetSatisfiesRequirements
    };
    let resolved = &binding.resolved;
    let mut failure = ProjectError::new(
        kind,
        format!("no target can run project `{}`", binding.identity.name),
    )
    .require(
        "runtime",
        match &resolved.runtime_constraint {
            Some(constraint) => format!("{} {constraint}", resolved.runtime),
            None => resolved.runtime.to_string(),
        },
    );
    if let Some(architecture) = &resolved.architecture {
        failure = failure.require("architecture", architecture);
    }
    if let Some(os) = &resolved.os {
        failure = failure.require("platform", os);
    }
    if let Some(capsule) = &resolved.capsule_id {
        failure = failure.require("dependencies", format!("capsule {capsule}"));
    }
    for tool in &binding.declared.tools {
        failure = failure.require(
            &format!("tool {}", tool.name),
            match tool.role {
                compute_core::ToolRole::Provisioning => "provisioning (not needed on a target)",
                compute_core::ToolRole::Execution => "execution",
            },
        );
    }
    for provider in &report.providers {
        let mut facts = vec![provider.status.as_str().to_owned()];
        if let Some(platform) = &provider.platform {
            facts.push(format!("platform {}", platform.label()));
        }
        for reason in &provider.reasons {
            let code: ReasonCode = reason.code;
            facts.push(format!(
                "{} (required {}, available {})",
                code.as_str(),
                reason.required,
                reason.available
            ));
        }
        failure = failure.found(
            &format!("target {}", provider.provider_id),
            facts.join("; "),
        );
    }
    if report.providers.is_empty() {
        failure = failure.found("targets", "none configured");
    }
    failure
}

/// A project's execution is only complete when its receipt carries the
/// project evidence and that evidence verifies. Otherwise the run reports
/// `receipt_evidence_incomplete` rather than a result it cannot prove.
pub(crate) fn require_project_evidence(
    result: &compute_core::ExecutionResult,
) -> compute_core::Result<()> {
    let incomplete = |message: &str| {
        error(ProjectError::new(
            FailureKind::ReceiptEvidenceIncomplete,
            message.to_owned(),
        ))
    };
    let receipt = result
        .receipt
        .as_ref()
        .ok_or_else(|| incomplete("the execution produced no receipt"))?;
    if receipt.project.is_none() {
        return Err(incomplete(
            "the receipt does not identify the project it executed",
        ));
    }
    receipt
        .verify()
        .map_err(|source| incomplete(&format!("the receipt does not verify: {source}")))
}
