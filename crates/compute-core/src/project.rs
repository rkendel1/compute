//! Project requirements: the normalized, tool-neutral description of what a
//! project needs, and the receipt evidence that separates what was
//! *declared* from what was *resolved*, what was *verified*, and what
//! *executed*.
//!
//! This module knows nothing about PAX or any other project tool. An adapter
//! (`compute-project`) translates a tool's observation into
//! [`ProjectRequirements`]; everything downstream of the adapter — planning,
//! placement, materialization, receipts — consumes only these types.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ComputeError, ExecutionReceipt, PlatformIdentity, Result, RuntimeKind, runtime_version_matches,
    validate_sha256_identity,
};

pub const PROJECT_REQUIREMENTS_VERSION: &str = "compute.project.requirements@1";

/// Which tool described the project, and the project's own name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectIdentity {
    /// The describing tool: `pax`.
    pub source: String,
    pub name: String,
    /// The schema version of the describing tool's observation.
    pub source_schema: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeNeed {
    pub kind: RuntimeKind,
    /// A version constraint, when the source states one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// What implied the requirement, e.g. `ecosystem:javascript`.
    pub origin: String,
}

/// Why a project needs a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRole {
    /// Used to produce the project's environment (a package manager). Compute
    /// consumes the product (a dependency capsule) and never runs the tool.
    Provisioning,
    /// Must exist on the target when the project runs.
    Execution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolNeed {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub role: ToolRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyGroup {
    /// Needed when the project runs.
    Runtime,
    Development,
    Optional,
    Peer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyNeed {
    pub name: String,
    pub specifier: String,
    pub group: DependencyGroup,
    /// What asked for it when it is not the project itself: `stack:<name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl DependencyNeed {
    /// The exact version this specifier pins, when it pins one. Ranges and
    /// tags pin nothing.
    pub fn pinned_version(&self) -> Option<&str> {
        let spec = self.specifier.trim();
        let spec = spec.strip_prefix('=').unwrap_or(spec).trim();
        // `major.minor.patch`, optionally followed by a `-pre` or `+build`
        // suffix. Anything else is a range, a tag, or partial.
        let core = spec.split(['-', '+']).next().unwrap_or_default();
        let numeric = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        let parts: Vec<_> = core.split('.').collect();
        (parts.len() == 3 && parts.iter().all(|part| numeric(part))).then_some(spec)
    }
}

/// A `major.minor.patch` version with any `-pre`/`+build` suffix kept apart.
fn parse_version(text: &str) -> Option<([u64; 3], bool)> {
    let core = text.split(['-', '+']).next()?;
    let prerelease = text.len() > core.len() && text[core.len()..].starts_with('-');
    let mut numbers = [0_u64; 3];
    let mut parts = core.split('.');
    for slot in &mut numbers {
        *slot = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some((numbers, prerelease))
}

/// Whether `text` is a plain `major.minor.patch` version, optionally with a
/// `-pre` or `+build` suffix.
pub fn is_version(text: &str) -> bool {
    parse_version(text).is_some()
}

/// A possibly partial version (`1`, `1.2`, `1.2.3`) and how many parts it had.
fn parse_partial(text: &str) -> Option<([u64; 3], usize)> {
    let mut numbers = [0_u64; 3];
    let mut count = 0;
    for part in text.split('.') {
        if count == 3 || part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        numbers[count] = part.parse().ok()?;
        count += 1;
    }
    (count > 0).then_some((numbers, count))
}

/// Whether `version` satisfies one version constraint: an exact
/// `major.minor.patch`, or `=`, `>=`, `>`, `<=`, `<`, `^`, `~` with a
/// version. `None` when the constraint is outside this grammar (a tag, a
/// union, a wildcard): callers must not treat that as satisfied *or*
/// unsatisfied.
pub fn version_satisfies(constraint: &str, version: &str) -> Option<bool> {
    let constraint = constraint.trim();
    let (actual, prerelease) = parse_version(version.trim())?;
    let operator_end = constraint
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(constraint.len());
    let (operator, wanted) = constraint.split_at(operator_end);
    let operator = operator.trim();
    if operator.is_empty() || operator == "=" {
        let (exact, _) = parse_version(wanted.trim())?;
        return Some(actual == exact && !prerelease || constraint_text_equals(wanted, version));
    }
    let (base, parts) = parse_partial(wanted.trim())?;
    // A prerelease satisfies only what names it exactly.
    if prerelease {
        return Some(false);
    }
    let above = |floor: [u64; 3]| actual >= floor;
    Some(match operator {
        ">=" => above(base),
        ">" => actual > base,
        "<=" => actual <= base,
        "<" => actual < base,
        "^" => {
            let ceiling = if base[0] > 0 || parts == 1 {
                [base[0] + 1, 0, 0]
            } else if base[1] > 0 || parts == 2 {
                [0, base[1] + 1, 0]
            } else {
                [0, 0, base[2] + 1]
            };
            above(base) && actual < ceiling
        }
        "~" => {
            let ceiling = if parts == 1 {
                [base[0] + 1, 0, 0]
            } else {
                [base[0], base[1] + 1, 0]
            };
            above(base) && actual < ceiling
        }
        _ => return None,
    })
}

fn constraint_text_equals(wanted: &str, version: &str) -> bool {
    wanted.trim() == version.trim()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PlatformNeed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandNeed {
    pub name: String,
    pub command: String,
}

/// Something the source declared that Compute cannot normalize. It is never
/// dropped: a project with unresolved needs does not run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedNeed {
    pub subject: String,
    pub reason: String,
}

/// The normalized requirements of one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRequirements {
    pub requirements_version: String,
    pub project: ProjectIdentity,
    pub runtimes: Vec<RuntimeNeed>,
    pub tools: Vec<ToolNeed>,
    pub dependencies: Vec<DependencyNeed>,
    #[serde(default)]
    pub platform: PlatformNeed,
    /// Environment variable names the project requires (never values).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment: Vec<String>,
    pub commands: Vec<CommandNeed>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<UnresolvedNeed>,
}

impl ProjectRequirements {
    /// Sort every collection so that equal requirements have equal bytes.
    pub fn normalize(&mut self) {
        self.runtimes.sort_by(|a, b| a.kind.cmp(&b.kind));
        self.tools
            .sort_by(|a, b| (&a.name, a.role).cmp(&(&b.name, b.role)));
        self.dependencies
            .sort_by(|a, b| (&a.name, a.group).cmp(&(&b.name, b.group)));
        self.environment.sort();
        self.environment.dedup();
        self.commands.sort_by(|a, b| a.name.cmp(&b.name));
        self.unresolved
            .sort_by(|a, b| (&a.subject, &a.reason).cmp(&(&b.subject, &b.reason)));
    }

    /// Identity of the normalized requirements.
    pub fn requirements_id(&self) -> Result<String> {
        let mut normalized = self.clone();
        normalized.normalize();
        Ok(format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&normalized)?)
        ))
    }

    pub fn runtime_dependencies(&self) -> impl Iterator<Item = &DependencyNeed> {
        self.dependencies
            .iter()
            .filter(|dependency| dependency.group == DependencyGroup::Runtime)
    }

    /// Identity of the dependencies the project needs when it runs.
    pub fn runtime_dependencies_id(&self) -> Result<String> {
        let mut needed: Vec<_> = self.runtime_dependencies().collect();
        needed.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&needed)?)
        ))
    }
}

/// What the source declared, summarized for a receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredRequirements {
    pub runtimes: Vec<RuntimeNeed>,
    pub tools: Vec<ToolNeed>,
    /// Dependencies the project needs when it runs, without copying the
    /// inventory into every receipt.
    pub dependency_count: u64,
    pub dependencies_id: String,
}

/// The command a run executes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedCommand {
    /// The project's name for the command, when one was selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Portable path of the entrypoint the runtime executes.
    pub entrypoint: String,
    pub argument_count: u64,
}

/// What Compute resolved the declared requirements to: the runtime and
/// platform placement must satisfy and the environment it materialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedRequirements {
    pub runtime: RuntimeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_constraint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    /// The dependency capsule that satisfies the runtime dependencies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capsule_id: Option<String>,
    pub command: ResolvedCommand,
    pub environment_names: Vec<String>,
}

/// Everything about a project that is known before execution. A provider
/// binds it into the receipt of the execution it performs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectBinding {
    pub identity: ProjectIdentity,
    pub requirements_id: String,
    pub declared: DeclaredRequirements,
    pub resolved: ResolvedRequirements,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// The execution's own evidence shows the requirement was met.
    Satisfied,
    /// The execution's evidence contradicts the requirement.
    Unsatisfied,
    /// Nothing required it.
    NotRequired,
    /// Declared, but no execution evidence can confirm it. Never a claim of
    /// satisfaction.
    NotEvaluated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    pub status: VerificationStatus,
    pub evidence: String,
}

impl Verification {
    fn new(status: VerificationStatus, evidence: impl Into<String>) -> Self {
        Self {
            status,
            evidence: evidence.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolVerification {
    pub name: String,
    pub verification: Verification,
}

/// What the execution's own receipt evidence says about each requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedRequirements {
    pub runtime: Verification,
    pub platform: Verification,
    pub dependencies: Verification,
    /// That the executed entrypoint is the resolved command's.
    pub command: Verification,
    pub tools: Vec<ToolVerification>,
}

/// Project evidence in an execution receipt: declared → resolved →
/// verified, next to the receipt's own placement, runtime, dependency, and
/// execution facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptProject {
    pub binding: ProjectBinding,
    pub verified: VerifiedRequirements,
}

impl ReceiptProject {
    /// Derive verification from the receipt's observed facts. A requirement
    /// is `satisfied` only when the receipt shows it; declaration alone
    /// never is.
    pub fn attest(binding: &ProjectBinding, receipt: &ExecutionReceipt) -> Self {
        Self {
            binding: binding.clone(),
            verified: verify_requirements(binding, receipt),
        }
    }

    pub fn verify(&self, receipt: &ExecutionReceipt) -> Result<()> {
        let invalid = |message: &str| ComputeError::InvalidReceipt(message.into());
        validate_sha256_identity(&self.binding.requirements_id)?;
        validate_sha256_identity(&self.binding.declared.dependencies_id)?;
        if let Some(capsule) = &self.binding.resolved.capsule_id {
            validate_sha256_identity(capsule)?;
        }
        if self.binding.identity.name.is_empty() || self.binding.identity.source.is_empty() {
            return Err(invalid("project identity is incomplete"));
        }
        let recomputed = verify_requirements(&self.binding, receipt);
        if recomputed != self.verified {
            return Err(invalid(
                "project verification does not follow from the receipt's evidence",
            ));
        }
        let unsatisfied = [
            &self.verified.runtime,
            &self.verified.platform,
            &self.verified.dependencies,
            &self.verified.command,
        ]
        .into_iter()
        .chain(self.verified.tools.iter().map(|tool| &tool.verification))
        .any(|verification| verification.status == VerificationStatus::Unsatisfied);
        if unsatisfied {
            return Err(invalid(
                "an execution receipt cannot record an unsatisfied project requirement",
            ));
        }
        Ok(())
    }
}

fn verify_requirements(
    binding: &ProjectBinding,
    receipt: &ExecutionReceipt,
) -> VerifiedRequirements {
    use VerificationStatus::*;
    let resolved = &binding.resolved;

    let observed = &receipt.runtime;
    let runtime = if observed.observed != resolved.runtime {
        Verification::new(
            Unsatisfied,
            format!(
                "observed {} where {} is required",
                observed.observed, resolved.runtime
            ),
        )
    } else if let Some(constraint) = &resolved.runtime_constraint
        && !runtime_version_matches(resolved.runtime, constraint, &observed.version)
    {
        Verification::new(
            Unsatisfied,
            format!(
                "observed {} {} where {constraint} is required",
                observed.observed, observed.version
            ),
        )
    } else {
        Verification::new(
            Satisfied,
            format!("observed {} {}", observed.observed, observed.version),
        )
    };

    let platform = if resolved.os.is_none() && resolved.architecture.is_none() {
        Verification::new(NotRequired, "no platform constraint")
    } else if let Some(actual) = receipt
        .placement
        .as_ref()
        .and_then(|placement| placement.execution_platform.as_ref())
    {
        platform_verification(resolved, actual)
    } else {
        Verification::new(NotEvaluated, "the receipt records no execution platform")
    };

    let dependencies = match (&resolved.capsule_id, &receipt.dependencies) {
        (None, None) if binding.declared.dependency_count == 0 => {
            Verification::new(NotRequired, "the project needs no runtime dependencies")
        }
        (None, _) => Verification::new(
            Unsatisfied,
            "the project needs runtime dependencies and no capsule was resolved",
        ),
        (Some(expected), Some(actual)) if actual.verified && &actual.capsule_id == expected => {
            Verification::new(Satisfied, format!("verified capsule {expected}"))
        }
        (Some(expected), _) => Verification::new(
            Unsatisfied,
            format!("capsule {expected} was not verified at execution"),
        ),
    };

    let entrypoint_name = std::path::Path::new(&resolved.command.entrypoint)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let command = if receipt.request.entrypoint == entrypoint_name
        && receipt.request.argument_count == resolved.command.argument_count
    {
        Verification::new(
            Satisfied,
            format!("executed {}", receipt.request.entrypoint),
        )
    } else {
        Verification::new(
            Unsatisfied,
            format!("executed {} instead", receipt.request.entrypoint),
        )
    };

    let tools = binding
        .declared
        .tools
        .iter()
        .map(|tool| ToolVerification {
            name: tool.name.clone(),
            verification: match tool.role {
                ToolRole::Provisioning => Verification::new(
                    NotEvaluated,
                    "provisioning tool: produced the environment before execution, not observed",
                ),
                ToolRole::Execution => Verification::new(
                    NotEvaluated,
                    "no execution evidence records tools on the target",
                ),
            },
        })
        .collect();

    VerifiedRequirements {
        runtime,
        platform,
        dependencies,
        command,
        tools,
    }
}

fn platform_verification(
    resolved: &ResolvedRequirements,
    actual: &PlatformIdentity,
) -> Verification {
    fn canonical(value: &str) -> String {
        match value.to_ascii_lowercase().as_str() {
            "arm64" | "aarch64" => "arm64".into(),
            "x86_64" | "amd64" => "x86_64".into(),
            other => other.into(),
        }
    }
    let os_ok = resolved
        .os
        .as_ref()
        .is_none_or(|os| canonical(os) == canonical(&actual.os));
    let arch_ok = resolved
        .architecture
        .as_ref()
        .is_none_or(|arch| canonical(arch) == canonical(&actual.architecture));
    if os_ok && arch_ok {
        Verification::new(
            VerificationStatus::Satisfied,
            format!("executed on {}", actual.label()),
        )
    } else {
        Verification::new(
            VerificationStatus::Unsatisfied,
            format!("executed on {}", actual.label()),
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        BoundaryStatus, DistributionIdentity, ExecutionRequest, ExecutionResult, ExecutionStatus,
        IsolationEvidence, IsolationProfile, NetworkPolicy, Output, ReceiptDependencies,
        ReceiptEnvironment, ResolvedRuntime, ResourceLimits, ResourceUsage, RuntimeSpec,
        WorkloadIdentity, create_execution_receipt, sha256_identity,
    };
    use std::time::Duration;

    pub(crate) fn receipt() -> ExecutionReceipt {
        let request = ExecutionRequest {
            runtime: RuntimeSpec {
                kind: RuntimeKind::Python,
                version: Some("3".into()),
            },
            entrypoint: "main.py".into(),
            args: vec!["--port".into()],
            stdin: vec![],
            env: vec![],
            inputs: vec![],
            outputs: vec![],
            mounts: vec![],
            network: NetworkPolicy::Network,
            resources: ResourceLimits::default(),
            isolation: IsolationProfile::Process,
            host_isolation: crate::HostProfile::Trusted,
            dependencies: None,
        };
        let resolved = ResolvedRuntime {
            kind: RuntimeKind::Python,
            requested_version: Some("3".into()),
            resolved_version: Some("3.13.0".into()),
            executable: None,
        };
        let result = ExecutionResult {
            execution_id: "exec_test_1".into(),
            runtime: RuntimeKind::Python,
            network: NetworkPolicy::Network,
            lifecycle: vec![ExecutionStatus::Created, ExecutionStatus::Completed],
            status: ExecutionStatus::Completed,
            exit_code: Some(0),
            stdout: Output::from_bytes(vec![], None),
            stderr: Output::from_bytes(vec![], None),
            duration: Duration::ZERO,
            resource_usage: ResourceUsage::default(),
            artifacts: vec![],
            outputs: vec![],
            missing_outputs: vec![],
            error: None,
            isolation: Some(IsolationEvidence {
                profile: IsolationProfile::Process,
                requested: IsolationProfile::Process,
                effective: IsolationProfile::Process,
                filesystem: BoundaryStatus::Unavailable,
                network: BoundaryStatus::NotRequested,
                environment: BoundaryStatus::Enforced,
                resources: BoundaryStatus::NotRequested,
                host: None,
            }),
            dependencies: None,
            provider: None,
            admission: None,
            receipt: None,
        };
        let environment = ReceiptEnvironment {
            distribution: DistributionIdentity {
                id: sha256_identity(b"distribution"),
                platform: "linux-x86_64".into(),
                manifest_version: "2".into(),
            },
            distribution_runtime_id: sha256_identity(b"runtime"),
            executable_identity: sha256_identity(b"executable"),
            runtime_distribution_id: sha256_identity(b"runtime-distribution"),
            runtime_distribution_digest: sha256_identity(b"runtime-artifact"),
            runtime_lock_id: sha256_identity(b"lock"),
            manifest_id: sha256_identity(b"manifest"),
        };
        let now = chrono::Utc::now();
        create_execution_receipt(
            &request,
            &resolved,
            &result,
            WorkloadIdentity::sha256(b"workload"),
            None,
            vec![],
            environment,
            now,
            now,
        )
        .unwrap()
    }

    fn binding() -> ProjectBinding {
        ProjectBinding {
            identity: ProjectIdentity {
                source: "pax".into(),
                name: "svc".into(),
                source_schema: "1".into(),
            },
            requirements_id: sha256_identity(b"requirements"),
            declared: DeclaredRequirements {
                runtimes: vec![RuntimeNeed {
                    kind: RuntimeKind::Python,
                    version: None,
                    origin: "ecosystem:python".into(),
                }],
                tools: vec![ToolNeed {
                    name: "uv".into(),
                    version: None,
                    role: ToolRole::Provisioning,
                }],
                dependency_count: 0,
                dependencies_id: sha256_identity(b"dependencies"),
            },
            resolved: ResolvedRequirements {
                runtime: RuntimeKind::Python,
                runtime_constraint: Some("3".into()),
                os: None,
                architecture: None,
                capsule_id: None,
                command: ResolvedCommand {
                    name: Some("start".into()),
                    entrypoint: "src/main.py".into(),
                    argument_count: 1,
                },
                environment_names: vec![],
            },
        }
    }

    fn sealed(mut receipt: ExecutionReceipt, binding: &ProjectBinding) -> ExecutionReceipt {
        receipt.project = Some(ReceiptProject::attest(binding, &receipt));
        receipt.seal().unwrap();
        receipt
    }

    #[test]
    fn a_receipt_without_a_project_is_byte_for_byte_what_it_was() {
        let plain = receipt();
        let text = String::from_utf8(plain.canonical_bytes().unwrap()).unwrap();
        assert!(!text.contains("project"));
        plain.verify().unwrap();
        let parsed: ExecutionReceipt =
            serde_json::from_slice(&plain.encoded_bytes().unwrap()).unwrap();
        assert_eq!(parsed.project, None);
        parsed.verify().unwrap();
    }

    #[test]
    fn verification_follows_the_receipts_own_evidence() {
        let receipt = sealed(receipt(), &binding());
        receipt.verify().unwrap();
        let verified = &receipt.project.as_ref().unwrap().verified;
        assert_eq!(verified.runtime.status, VerificationStatus::Satisfied);
        assert_eq!(verified.runtime.evidence, "observed python 3.13.0");
        assert_eq!(verified.platform.status, VerificationStatus::NotRequired);
        assert_eq!(
            verified.dependencies.status,
            VerificationStatus::NotRequired
        );
        assert_eq!(verified.command.status, VerificationStatus::Satisfied);
        // A declared tool is never claimed as satisfied.
        assert_eq!(
            verified.tools[0].verification.status,
            VerificationStatus::NotEvaluated
        );
        // It survives the portable encoding.
        let parsed: ExecutionReceipt =
            serde_json::from_slice(&receipt.encoded_bytes().unwrap()).unwrap();
        parsed.verify().unwrap();
        assert_eq!(parsed.project, receipt.project);
    }

    #[test]
    fn a_declared_requirement_is_not_a_satisfied_one() {
        // The project needs a capsule; the execution recorded none.
        let mut needs_capsule = binding();
        needs_capsule.resolved.capsule_id = Some(sha256_identity(b"capsule"));
        needs_capsule.declared.dependency_count = 2;
        let receipt = sealed(receipt(), &needs_capsule);
        let verified = &receipt.project.as_ref().unwrap().verified;
        assert_eq!(
            verified.dependencies.status,
            VerificationStatus::Unsatisfied
        );
        assert!(
            receipt
                .verify()
                .unwrap_err()
                .to_string()
                .contains("unsatisfied")
        );

        // With the capsule verified at execution, it is satisfied.
        let mut with_capsule = receipt.clone();
        with_capsule.dependencies = Some(ReceiptDependencies {
            capsule_id: sha256_identity(b"capsule"),
            verified: true,
        });
        let with_capsule = sealed(with_capsule, &needs_capsule);
        with_capsule.verify().unwrap();
        assert_eq!(
            with_capsule.project.unwrap().verified.dependencies.status,
            VerificationStatus::Satisfied
        );
    }

    #[test]
    fn runtime_and_command_mismatches_are_unsatisfied() {
        let mut wrong_version = binding();
        wrong_version.resolved.runtime_constraint = Some(">=99".into());
        assert!(sealed(receipt(), &wrong_version).verify().is_err());

        let mut wrong_runtime = binding();
        wrong_runtime.resolved.runtime = RuntimeKind::Node;
        wrong_runtime.resolved.runtime_constraint = None;
        assert!(sealed(receipt(), &wrong_runtime).verify().is_err());

        let mut wrong_command = binding();
        wrong_command.resolved.command.entrypoint = "other.py".into();
        assert!(sealed(receipt(), &wrong_command).verify().is_err());

        let mut wrong_arguments = binding();
        wrong_arguments.resolved.command.argument_count = 0;
        assert!(sealed(receipt(), &wrong_arguments).verify().is_err());
    }

    #[test]
    fn a_forged_verification_does_not_verify() {
        let mut receipt = sealed(receipt(), &binding());
        // Claim the runtime observed something else, then reseal honestly.
        receipt.project.as_mut().unwrap().verified.runtime.evidence = "observed node 99".into();
        receipt.seal().unwrap();
        assert!(
            receipt
                .verify()
                .unwrap_err()
                .to_string()
                .contains("does not follow")
        );

        // Changing the project after sealing breaks the receipt hash.
        let mut tampered = sealed(self::receipt(), &binding());
        tampered.project.as_mut().unwrap().binding.identity.name = "other".into();
        assert!(tampered.verify().unwrap_err().to_string().contains("hash"));
    }

    #[test]
    fn platform_is_verified_against_where_execution_happened() {
        let mut resolved = binding().resolved;
        resolved.architecture = Some("aarch64".into());
        let platform = |os: &str, architecture: &str| PlatformIdentity {
            os: os.into(),
            architecture: architecture.into(),
            runtime_abi: None,
        };
        // Architecture aliases are the same architecture.
        assert_eq!(
            platform_verification(&resolved, &platform("linux", "arm64")).status,
            VerificationStatus::Satisfied
        );
        assert_eq!(
            platform_verification(&resolved, &platform("linux", "x86_64")).status,
            VerificationStatus::Unsatisfied
        );
        resolved.os = Some("macos".into());
        assert_eq!(
            platform_verification(&resolved, &platform("linux", "arm64")).status,
            VerificationStatus::Unsatisfied
        );
        // A platform constraint with no execution platform recorded is
        // reported as not evaluated, never as satisfied.
        let mut constrained = binding();
        constrained.resolved.architecture = Some("x86_64".into());
        let receipt = sealed(receipt(), &constrained);
        assert_eq!(
            receipt.project.unwrap().verified.platform.status,
            VerificationStatus::NotEvaluated
        );
    }

    #[test]
    fn version_constraints_are_evaluated_or_declared_unknown() {
        let yes = |c: &str, v: &str| assert_eq!(version_satisfies(c, v), Some(true), "{c} {v}");
        let no = |c: &str, v: &str| assert_eq!(version_satisfies(c, v), Some(false), "{c} {v}");
        yes("1.3.0", "1.3.0");
        yes("=1.3.0", "1.3.0");
        no("1.3.0", "1.3.1");
        yes("^4.17.0", "4.17.21");
        yes("^4.17.0", "4.99.0");
        no("^4.17.0", "5.0.0");
        no("^4.17.0", "4.16.9");
        yes("^0.2.3", "0.2.9");
        no("^0.2.3", "0.3.0");
        no("^0.0.3", "0.0.4");
        yes("~1.2.3", "1.2.9");
        no("~1.2.3", "1.3.0");
        yes(">=22", "22.12.0");
        yes(">= 20.17", "24.18.0");
        no(">=24", "22.12.0");
        yes("<2", "1.9.9");
        no(">1.0.0", "1.0.0");
        no("^1.0.0", "1.2.0-beta");
        yes("1.2.3-beta", "1.2.3-beta");
        for unknown in ["latest", "*", "1.x", "^1 || ^2", "workspace:*", ""] {
            assert_eq!(version_satisfies(unknown, "1.2.3"), None, "{unknown}");
        }
        assert_eq!(version_satisfies("^1.0.0", "not-a-version"), None);
    }

    #[test]
    fn dependency_pins_are_exact_versions_only() {
        let need = |specifier: &str| DependencyNeed {
            name: "x".into(),
            specifier: specifier.into(),
            group: DependencyGroup::Runtime,
            origin: None,
        };
        assert_eq!(need("1.2.3").pinned_version(), Some("1.2.3"));
        assert_eq!(need("=1.2.3").pinned_version(), Some("1.2.3"));
        assert_eq!(need("1.2.3-beta").pinned_version(), Some("1.2.3-beta"));
        for range in [
            "^1.2.3", "~1.2.3", ">=1", "1.x", "1.2.x", "*", "latest", "1.2", "",
        ] {
            assert_eq!(need(range).pinned_version(), None, "{range}");
        }
    }
}
