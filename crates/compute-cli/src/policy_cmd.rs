//! `compute policy` and `compute explain`.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use compute_core::ComputeError;
use compute_policy::{AdmissionDecision, EffectivePolicy, Policy, ReasonKind};
use compute_provider::{Admission, ComputeProvider, LocalProvider};

use crate::admission::{PolicyLocation, request_policy};
use crate::pool::{PLACEMENT_FAILED_EXIT, PlacementArtifact, PoolLocation};

#[derive(Args, Debug)]
pub struct PolicyCommand {
    #[command(subcommand)]
    pub command: PolicyCommands,
    #[command(flatten)]
    pub policy: PolicyLocation,
    #[command(flatten)]
    pub location: PoolLocation,
}

#[derive(Subcommand, Debug)]
pub enum PolicyCommands {
    /// Show the effective policy: the baseline intersected with local
    /// configuration and --policy.
    Inspect {
        #[arg(long)]
        json: bool,
    },
    /// Statically validate a compute.policy@1 document.
    Validate {
        policy_file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Decide admission for a workload without executing it. Exits 0 when
    /// admitted and 2 when denied.
    Check(Box<PlacementArtifact>),
    /// Explain every policy dimension of an admission decision.
    Explain(Box<PlacementArtifact>),
}

/// `compute explain`: the full decision chain for a workload.
#[derive(Args, Debug)]
pub struct ExplainCommand {
    #[command(flatten)]
    pub artifact: Box<PlacementArtifact>,
    #[command(flatten)]
    pub policy: PolicyLocation,
    #[command(flatten)]
    pub location: PoolLocation,
}

fn print_json(value: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("policy values are serializable")
    );
}

pub async fn policy(command: PolicyCommand) -> compute_core::Result<()> {
    match command.command {
        PolicyCommands::Inspect { json } => {
            let effective = EffectivePolicy::compose(&command.policy.sources()?);
            if json {
                print_json(&serde_json::json!({
                    "policy_id": effective.policy_id,
                    "policy": effective.policy,
                    "sources": effective.sources,
                    "baseline": Policy::baseline(),
                }));
            } else {
                println!("Effective policy: {}", effective.policy_id);
                println!("Sources (intersected):");
                for source in &effective.sources {
                    println!(
                        "  {}\t{}\t{}",
                        crate::pool::enum_label(&source.kind),
                        source.label.as_deref().unwrap_or("-"),
                        source.policy_id
                    );
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&effective.policy)
                        .expect("policies are serializable")
                );
            }
        }
        PolicyCommands::Validate { policy_file, json } => {
            let bytes = std::fs::read(&policy_file)?;
            match Policy::from_json(&bytes) {
                Ok(policy) => {
                    if json {
                        print_json(&serde_json::json!({
                            "valid": true,
                            "format": compute_policy::POLICY_FORMAT,
                            "policy_id": policy.policy_id(),
                            "label": policy.label(),
                            "policy": policy,
                        }));
                    } else {
                        println!("Valid: {}", compute_policy::POLICY_FORMAT);
                        println!("Policy: {}", policy.label());
                        println!("Policy ID: {}", policy.policy_id());
                    }
                }
                Err(error) => {
                    if json {
                        print_json(&serde_json::json!({
                            "valid": false,
                            "error": error.to_string(),
                        }));
                    }
                    return Err(ComputeError::InvalidWorkload(error.to_string()));
                }
            }
        }
        PolicyCommands::Check(artifact) => {
            let admission = admit(&command.policy, &command.location, &artifact).await?;
            if artifact.json {
                print_json(&evidence(&admission));
            } else {
                print_decision(&admission);
            }
            if !admission.decision.admitted {
                std::process::exit(PLACEMENT_FAILED_EXIT);
            }
        }
        PolicyCommands::Explain(artifact) => {
            let admission = admit(&command.policy, &command.location, &artifact).await?;
            if artifact.json {
                print_json(&serde_json::json!({
                    "evidence": evidence(&admission),
                    "explanation": explanation(&admission),
                }));
            } else {
                for line in explanation(&admission) {
                    println!("{line}");
                }
            }
            if !admission.decision.admitted {
                std::process::exit(PLACEMENT_FAILED_EXIT);
            }
        }
    }
    Ok(())
}

/// Admission of the workload on one provider: `--provider` (a pool member)
/// or the local provider.
async fn admit(
    policy: &PolicyLocation,
    location: &PoolLocation,
    artifact: &PlacementArtifact,
) -> compute_core::Result<Admission> {
    if artifact.receipt.is_some() || artifact.idempotency_key.is_some() {
        return Err(ComputeError::InvalidWorkload(
            "admission checks never execute; --receipt and --idempotency-key are not valid".into(),
        ));
    }
    let (_, mut request) = crate::pool::prepare(artifact, policy)?;
    request.execution.isolation = artifact.isolation;
    request.expected.distribution_id = artifact.distribution.clone();
    request.execution.policy = request_policy(&policy.sources()?);
    let admission = match artifact.provider.as_deref() {
        None => LocalProvider::new().admit(request).await,
        Some(id) => match location.pool()?.member(id) {
            Some(member) => member.provider.admit(request).await,
            None if id == "local" => LocalProvider::new().admit(request).await,
            None => {
                return Err(ComputeError::InvalidWorkload(format!(
                    "provider {id} is not configured in this pool"
                )));
            }
        },
    };
    admission.map_err(crate::provider_error)
}

fn evidence(admission: &Admission) -> serde_json::Value {
    let decision = &admission.decision;
    serde_json::json!({
        "policy": admission.policy.sources,
        "policy_id": decision.policy_id,
        "requirements": decision.contract,
        "provider": decision.provider,
        "admission": {
            "admission_id": decision.admission_id,
            "status": decision.status,
            "admitted": decision.admitted,
            "capability": decision.capability,
        },
        "reasons": decision.reasons,
        "effective_policy": admission.policy.policy,
        "decision": decision,
    })
}

fn print_decision(admission: &Admission) {
    let decision = &admission.decision;
    println!("Policy: {}", policy_label(&admission.policy));
    println!("Admission: {}", decision.status.as_str());
    println!("Admission ID: {}", decision.admission_id);
    for reason in &decision.reasons {
        println!(
            "Reason: [{}] {}: {}",
            crate::pool::enum_label(&reason.kind),
            reason.code,
            reason.message
        );
    }
}

/// A readable name for an effective policy: the labels of its non-baseline
/// sources, or the baseline, with the effective identity.
pub fn policy_label(policy: &EffectivePolicy) -> String {
    let labels = policy
        .sources
        .iter()
        .filter(|source| source.kind != compute_policy::PolicySourceKind::Baseline)
        .map(|source| {
            source
                .label
                .clone()
                .unwrap_or_else(|| source.policy_id.clone())
        })
        .collect::<Vec<_>>();
    let name = if labels.is_empty() {
        "compute-baseline@1".to_string()
    } else {
        format!("compute-baseline@1 ∩ {}", labels.join(" ∩ "))
    };
    format!("{name} ({})", policy.policy_id)
}

fn dimension_status(decision: &AdmissionDecision, dimensions: &[&str]) -> String {
    let reasons = decision
        .reasons
        .iter()
        .filter(|reason| {
            reason.kind == ReasonKind::Policy && dimensions.contains(&reason.dimension.as_str())
        })
        .map(|reason| reason.message.clone())
        .collect::<Vec<_>>();
    if reasons.is_empty() {
        "allowed".into()
    } else {
        format!("denied: {}", reasons.join("; "))
    }
}

fn explanation(admission: &Admission) -> Vec<String> {
    let decision = &admission.decision;
    let contract = &decision.contract;
    let policy = &admission.policy.policy;
    let mut lines = vec![
        format!("Workload: {}", contract.workload_id),
        format!("Bundle: {}", contract.bundle_id),
        format!(
            "Provider: {}",
            match &decision.provider.identity {
                compute_core::ProviderIdentity::Local { id } => format!("{id} (local)"),
                compute_core::ProviderIdentity::Remote { id, .. } => format!("{id} (remote)"),
            }
        ),
        format!(
            "Runtime: {}{} — {}",
            contract.runtime.kind,
            decision
                .provider
                .runtime_version
                .as_deref()
                .map(|version| format!(" {}", version.lines().next().unwrap_or(version)))
                .unwrap_or_default(),
            dimension_status(decision, &["runtime"])
        ),
        format!(
            "Distribution: {} — {}",
            decision
                .provider
                .distribution_id
                .as_deref()
                .unwrap_or("unknown"),
            dimension_status(decision, &["distribution"])
        ),
        format!(
            "Dependencies: {} — {}",
            contract.dependency_id.as_deref().unwrap_or("none"),
            dimension_status(decision, &["dependencies"])
        ),
        format!(
            "Isolation: {} (policy minimum: {}) — {}",
            contract.isolation,
            policy
                .minimum_isolation
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".into()),
            dimension_status(decision, &["isolation"])
        ),
        format!(
            "Network: {} — {}",
            contract.network,
            dimension_status(decision, &["network"])
        ),
        format!(
            "Resources: timeout {}, memory {}, output {}, input {} bytes, artifact {} bytes — {}",
            contract
                .resources
                .timeout_ms
                .map(|value| format!("{value}ms"))
                .unwrap_or_else(|| "unbounded".into()),
            contract
                .resources
                .memory_bytes
                .map(|value| format!("{value} bytes"))
                .unwrap_or_else(|| "unbounded".into()),
            contract
                .resources
                .output_bound()
                .map(|value| format!("{value} bytes"))
                .unwrap_or_else(|| "unbounded".into()),
            contract.input_bytes,
            contract.artifact_bytes,
            dimension_status(
                decision,
                &["timeout", "memory", "output", "input", "artifact"]
            )
        ),
        format!(
            "Platform: {} — {}",
            decision
                .provider
                .platform
                .as_ref()
                .map(|platform| platform.label())
                .unwrap_or_else(|| "unknown".into()),
            dimension_status(decision, &["platform"])
        ),
        format!(
            "Capability: {}",
            match &decision.capability {
                compute_policy::CapabilityStatus::Compatible => "compatible".to_string(),
                compute_policy::CapabilityStatus::Incompatible { codes } =>
                    format!("incompatible: {}", codes.join(", ")),
                compute_policy::CapabilityStatus::Unknown => "unknown".into(),
            }
        ),
        format!("Policy: {}", policy_label(&admission.policy)),
        format!("Admission: {}", decision.status.as_str()),
    ];
    for reason in &decision.reasons {
        lines.push(format!("Reason: {}", reason.message));
    }
    lines.push(format!("Admission ID: {}", decision.admission_id));
    lines
}

/// `compute explain`: workload → requirements → capabilities → policy →
/// admission → placement.
pub async fn explain(command: ExplainCommand) -> compute_core::Result<()> {
    let artifact = command.artifact;
    if artifact.receipt.is_some() || artifact.idempotency_key.is_some() {
        return Err(ComputeError::InvalidWorkload(
            "compute explain never executes; --receipt and --idempotency-key are not valid".into(),
        ));
    }
    let submission = if artifact.submit {
        compute_placement::SubmissionMode::Job
    } else {
        compute_placement::SubmissionMode::Synchronous
    };
    let (_, report, _) =
        crate::pool::evaluate(&command.location, &command.policy, &artifact, submission).await?;
    if artifact.json {
        let selected = report
            .selected
            .as_ref()
            .and_then(|selected| selected_admission(selected, &report));
        print_json(&serde_json::json!({
            "workload": {
                "workload_id": report.admission.contract.workload_id,
                "bundle_id": report.admission.contract.bundle_id,
            },
            "requirements": report.requirements,
            "contract": report.admission.contract,
            "policy": {
                "policy_id": report.policy_id,
                "sources": report.admission.policy.sources,
            },
            "providers": report.providers.iter().map(|provider| serde_json::json!({
                "provider_id": provider.provider_id,
                "status": provider.status,
                "capability": provider.reasons,
                "admission": provider.admission,
                "error": provider.error,
            })).collect::<Vec<_>>(),
            "admission": selected,
            "placement": {
                "placement_id": report.placement_id,
                "outcome": report.outcome,
                "selected_provider": report.selected.as_ref().map(|selected| &selected.provider_id),
                "failure": report.failure,
                "explanation": report.explanation,
            },
        }));
    } else {
        let contract = &report.admission.contract;
        println!("Workload");
        println!("  ID: {}", contract.workload_id);
        println!("  Bundle: {}", contract.bundle_id);
        println!("Runtime requirements");
        println!(
            "  {} {}",
            contract.runtime.kind,
            contract
                .runtime
                .version
                .as_deref()
                .unwrap_or("(any version)")
        );
        println!("Dependency requirements");
        println!("  {}", contract.dependency_id.as_deref().unwrap_or("none"));
        println!("Isolation requirements");
        println!("  Isolation: {}", contract.isolation);
        println!("  Network: {}", contract.network);
        println!("Provider capabilities");
        for provider in &report.providers {
            let capability = if provider.reasons.is_empty() {
                match provider.status {
                    compute_placement::EvaluationStatus::CapabilitiesInvalid
                    | compute_placement::EvaluationStatus::CapabilitiesUnknown
                    | compute_placement::EvaluationStatus::ProviderUnavailable => {
                        provider.status.as_str().to_string()
                    }
                    _ => "compatible".into(),
                }
            } else {
                format!(
                    "incompatible: {}",
                    provider
                        .reasons
                        .iter()
                        .map(|reason| reason.code.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            println!("  {}: {capability}", provider.provider_id);
        }
        println!("Policy");
        println!("  {}", policy_label(&report.admission.policy));
        println!("Admission");
        for provider in &report.providers {
            match &provider.admission {
                Some(decision) => {
                    let policy_reasons = decision
                        .reasons
                        .iter()
                        .filter(|reason| reason.kind != ReasonKind::Capability)
                        .map(|reason| reason.message.clone())
                        .collect::<Vec<_>>();
                    if policy_reasons.is_empty() {
                        println!("  {}: policy admits", provider.provider_id);
                    } else {
                        println!(
                            "  {}: policy denies: {}",
                            provider.provider_id,
                            policy_reasons.join("; ")
                        );
                    }
                }
                None => println!(
                    "  {}: not evaluated (no valid capabilities)",
                    provider.provider_id
                ),
            }
        }
        println!("Placement");
        match (&report.selected, &report.failure) {
            (Some(selected), _) => {
                println!("  Provider: {}", selected.provider_id);
                if let Some(decision) = selected_admission(selected, &report) {
                    if let Some(version) = &decision.provider.runtime_version {
                        println!(
                            "  Runtime: {} {}",
                            contract.runtime.kind,
                            version.lines().next().unwrap_or(version)
                        );
                    }
                    println!(
                        "  Distribution: {}",
                        decision
                            .provider
                            .distribution_id
                            .as_deref()
                            .unwrap_or("unknown")
                    );
                }
                println!("  Isolation: {}", contract.isolation);
                println!("  Network: {}", contract.network);
                println!("  Policy: {}", selected.policy_id);
                println!("  Admission: admitted ({})", selected.admission_id);
                println!("  Placement: {}", report.placement_id);
            }
            (None, Some(failure)) => {
                println!("  Admission: denied or not established");
                println!("  Reason: {}: {}", failure.code, failure.message);
                println!("  Placement: {}", report.placement_id);
            }
            (None, None) => println!("  placement_failed"),
        }
    }
    if report.outcome == compute_placement::PlacementOutcome::PlacementFailed {
        std::process::exit(PLACEMENT_FAILED_EXIT);
    }
    Ok(())
}

fn selected_admission<'a>(
    selected: &compute_placement::SelectedProvider,
    report: &'a compute_placement::PlacementReport,
) -> Option<&'a AdmissionDecision> {
    report
        .providers
        .iter()
        .find(|provider| provider.provider_id == selected.provider_id)
        .and_then(|provider| provider.admission.as_ref())
}
