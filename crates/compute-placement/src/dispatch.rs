//! Hand a placed workload to its selected provider — once.
//!
//! Dispatch is not execution: the selected provider executes through the
//! existing provider contract. Dispatch binds the placement into the request,
//! refuses to proceed without a successful placement, never tries another
//! provider, and checks that the returned evidence names the provider the
//! placement selected.

use compute_core::{JobSubmission, ProviderIdentity};
use compute_provider::{ExecuteResponse, ProviderErrorKind, ProviderRequest};
use serde::{Deserialize, Serialize};

use crate::placement::{PlacementOutcome, PlacementReport};
use crate::pool::ProviderPool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchErrorCode {
    /// Placement did not select a provider; nothing was sent anywhere.
    PlacementFailed,
    /// Placement succeeded, but the selected provider could not be reached.
    ProviderUnavailable,
    /// The selected provider refused or failed the request.
    ProviderRejected,
    /// The selected provider's admission denied the execution; nothing ran.
    AdmissionDenied,
    /// The selected provider does not accept durable jobs.
    JobsUnsupported,
    /// Returned evidence does not prove execution at the selected provider.
    EvidenceInvalid,
}

/// A dispatch failure. `retried` is always false: Compute never resubmits a
/// workload to another provider. A caller that wants failover must make a
/// new, explicit placement and submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchError {
    pub code: DispatchErrorCode,
    pub placement_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_error: Option<ProviderErrorKind>,
    pub message: String,
    /// Admission evidence when the provider denied the execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<Box<compute_policy::AdmissionDecision>>,
    pub retried: bool,
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = serde_json::to_value(self.code)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        write!(formatter, "{code}: {}", self.message)
    }
}

impl std::error::Error for DispatchError {}

/// A submitted job and the provider that owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacedSubmission {
    pub placement_id: String,
    pub provider_id: String,
    pub provider_identity: ProviderIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub job: JobSubmission,
}

fn prepare<'a>(
    pool: &'a ProviderPool,
    report: &PlacementReport,
    mut request: ProviderRequest,
) -> Result<(&'a crate::pool::PoolMember, ProviderRequest), DispatchError> {
    let failure = |code, provider_id: Option<String>, message: String| DispatchError {
        code,
        placement_id: report.placement_id.clone(),
        provider_id,
        provider_error: None,
        message,
        admission: None,
        retried: false,
    };
    let (PlacementOutcome::Placed, Some(selected), Some(binding)) =
        (report.outcome, &report.selected, report.receipt_binding())
    else {
        return Err(failure(
            DispatchErrorCode::PlacementFailed,
            None,
            report
                .failure
                .as_ref()
                .map(|failure| format!("{}: {}", failure.code, failure.message))
                .unwrap_or_else(|| "placement did not select a provider".into()),
        ));
    };
    let member = pool.member(&selected.provider_id).ok_or_else(|| {
        failure(
            DispatchErrorCode::PlacementFailed,
            Some(selected.provider_id.clone()),
            "the selected provider is no longer in the pool".into(),
        )
    })?;
    if let Some(distribution) = &report.requirements.distribution {
        request.expected.distribution_id = Some(distribution.id.clone());
    }
    // An embedded capsule is pinned in transport. A resident capsule is
    // resolved and identity-checked by the provider at execution; the
    // receipt check below covers both.
    if let Some(dependencies) = &report.requirements.dependencies
        && dependencies.embedded
    {
        request.expected.dependency_id = Some(dependencies.id.clone());
    }
    request.execution.isolation = Some(report.requirements.isolation);
    request.execution.placement = Some(binding);
    request.execution.policy = report.admission.request_policy.clone();
    Ok((member, request))
}

fn provider_failure(
    report: &PlacementReport,
    provider_id: &str,
    error: compute_provider::ProviderError,
) -> DispatchError {
    let unreachable = matches!(
        error.kind,
        ProviderErrorKind::ProviderUnavailable | ProviderErrorKind::TransportFailure
    );
    DispatchError {
        code: if unreachable {
            DispatchErrorCode::ProviderUnavailable
        } else if error.kind == ProviderErrorKind::AdmissionDenied {
            DispatchErrorCode::AdmissionDenied
        } else {
            DispatchErrorCode::ProviderRejected
        },
        placement_id: report.placement_id.clone(),
        provider_id: Some(provider_id.into()),
        provider_error: Some(error.kind),
        message: error.message,
        admission: error.admission,
        retried: false,
    }
}

async fn prepare_selected_runtime(
    report: &PlacementReport,
    member: &crate::pool::PoolMember,
    mut request: ProviderRequest,
) -> Result<ProviderRequest, DispatchError> {
    let selected = report
        .selected
        .as_ref()
        .expect("placement selected provider");
    // Pre-lifecycle providers and unmanaged host runtimes are already
    // executable and have no provider-managed distribution to prepare.
    if selected.runtime_distribution.is_none() {
        return Ok(request);
    }
    let requirement = compute_core::ProviderRuntimeRequirement {
        runtime: report.requirements.runtime.kind,
        version: report.requirements.runtime.version.clone(),
        platform: report.requirements.platform.clone(),
    };
    let resolution = member
        .provider
        .resolve_runtime(requirement)
        .await
        .map_err(|error| provider_failure(report, &member.id, error))?;
    if !resolution.status.can_satisfy() {
        return Err(provider_failure(
            report,
            &member.id,
            compute_provider::ProviderError::new(
                ProviderErrorKind::DistributionUnavailable,
                resolution.detail.unwrap_or_else(|| {
                    "selected provider can no longer satisfy the runtime".into()
                }),
            ),
        ));
    }
    if resolution.distribution != selected.runtime_distribution {
        return Err(DispatchError {
            code: DispatchErrorCode::EvidenceInvalid,
            placement_id: report.placement_id.clone(),
            provider_id: Some(member.id.clone()),
            provider_error: Some(ProviderErrorKind::EvidenceInvalid),
            message: "runtime resolution changed after placement".into(),
            admission: None,
            retried: false,
        });
    }
    if let Some(distribution) = resolution.distribution {
        let preparation = member
            .provider
            .prepare_runtime(distribution.clone())
            .await
            .map_err(|error| provider_failure(report, &member.id, error))?;
        if preparation.status != compute_core::RuntimeLifecycleStatus::Ready
            || !preparation.verified
            || preparation.distribution != distribution
        {
            return Err(provider_failure(
                report,
                &member.id,
                compute_provider::ProviderError::new(
                    ProviderErrorKind::EvidenceInvalid,
                    "provider did not prove the resolved runtime was verified and prepared",
                ),
            ));
        }
        request.expected.runtime_distribution_id = Some(distribution.id);
        request.expected.runtime_distribution_digest = Some(distribution.digest);
    }
    Ok(request)
}

/// Execute synchronously on the selected provider and verify the receipt.
pub async fn execute(
    pool: &ProviderPool,
    report: &PlacementReport,
    request: ProviderRequest,
) -> Result<ExecuteResponse, DispatchError> {
    let (member, request) = prepare(pool, report, request)?;
    let request = prepare_selected_runtime(report, member, request).await?;
    let response = member
        .provider
        .execute(request)
        .await
        .map_err(|error| provider_failure(report, &member.id, error))?;
    let evidence = |message: String| DispatchError {
        code: DispatchErrorCode::EvidenceInvalid,
        placement_id: report.placement_id.clone(),
        provider_id: Some(member.id.clone()),
        provider_error: None,
        message,
        admission: None,
        retried: false,
    };
    let receipt = response
        .result
        .receipt
        .as_ref()
        .ok_or_else(|| evidence("the provider returned no receipt".into()))?;
    report.verify_receipt(receipt).map_err(evidence)?;
    if response.result.provider.as_ref() != receipt.provider.as_ref() {
        return Err(evidence(
            "result provider differs from receipt provider".into(),
        ));
    }
    Ok(response)
}

/// Submit a durable job to the selected provider.
pub async fn submit(
    pool: &ProviderPool,
    report: &PlacementReport,
    request: ProviderRequest,
    idempotency_key: Option<&str>,
) -> Result<PlacedSubmission, DispatchError> {
    let (member, request) = prepare(pool, report, request)?;
    let request = prepare_selected_runtime(report, member, request).await?;
    let Some(jobs) = &member.jobs else {
        return Err(DispatchError {
            code: DispatchErrorCode::JobsUnsupported,
            placement_id: report.placement_id.clone(),
            provider_id: Some(member.id.clone()),
            provider_error: None,
            message: "the selected provider does not accept durable jobs".into(),
            admission: None,
            retried: false,
        });
    };
    let job = jobs
        .submit(request, idempotency_key)
        .await
        .map_err(|error| provider_failure(report, &member.id, error))?;
    let selected = report.selected.as_ref().expect("checked by prepare");
    Ok(PlacedSubmission {
        placement_id: report.placement_id.clone(),
        provider_id: member.id.clone(),
        provider_identity: selected.provider_identity.clone(),
        endpoint: member.config.endpoint.clone(),
        job,
    })
}
