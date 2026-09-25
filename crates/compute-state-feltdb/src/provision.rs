//! Install or upgrade the Compute control model in Managed FeltDB.
//!
//! FeltDB applications evolve through drafts: a draft holds a manifest, is
//! validated, is committed as an immutable revision, and a revision is
//! promoted to an environment. Compute uses exactly that path.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{COMPUTE_MANIFEST, FeltDbConfig, FeltDbState, encode};
use compute_state::StateError;

#[derive(Debug, Clone)]
pub struct ProvisionRequest {
    pub url: String,
    pub token: String,
    /// Upgrade this existing Compute application instead of creating one.
    pub application_id: Option<String>,
    /// The tenant to create the application in. When absent, a tenant is
    /// created with `tenant_name`.
    pub tenant_id: Option<String>,
    pub tenant_name: String,
    /// The FeltDB environment to promote the model to.
    pub environment: String,
    /// A PEM certificate authority to trust, as in `FeltDbConfig`.
    pub ca_certificate: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provisioned {
    pub tenant_id: String,
    pub application_id: String,
    pub environment: String,
    /// The FeltDB revision of the Compute model now active.
    pub revision_id: String,
    /// Whether the model changed; `false` when it was already current.
    pub changed: bool,
}

fn failure(step: &str, refusal: Result<crate::Refusal, StateError>) -> StateError {
    match refusal {
        Ok(refusal) => StateError::Unavailable(format!(
            "FeltDB refused to {step} ({} {}): {}",
            refusal.status, refusal.code, refusal.message
        )),
        Err(error) => error,
    }
}

/// Install the Compute control model, or upgrade an existing Compute
/// application to it. Idempotent: an existing tenant and Compute
/// application are reused, and an application already on this model is
/// left unchanged.
pub async fn provision(request: ProvisionRequest) -> Result<Provisioned, StateError> {
    provision_manifest(request, COMPUTE_MANIFEST).await
}

/// [`provision`] with an explicit manifest; certification uses it to prove
/// that an application on an older model is upgraded.
#[doc(hidden)]
pub async fn provision_manifest(
    request: ProvisionRequest,
    compiled: &str,
) -> Result<Provisioned, StateError> {
    let client = FeltDbState::new(FeltDbConfig {
        url: request.url.clone(),
        token: request.token.clone(),
        application_id: request.application_id.clone().unwrap_or_default(),
        environment: request.environment.clone(),
        ca_certificate: request.ca_certificate.clone(),
    })?;
    let post = |path: String, body: Value| {
        let client = &client;
        async move { client.send(reqwest::Method::POST, &path, Some(&body)).await }
    };
    let (tenant_id, application_id) = match &request.application_id {
        Some(application_id) => {
            let application = client
                .send(
                    reqwest::Method::GET,
                    &format!("/api/applications/{}", encode(application_id)),
                    None,
                )
                .await
                .map_err(|refusal| failure("read the Compute application", refusal))?;
            let tenant = application["tenant_id"]
                .as_str()
                .ok_or_else(|| StateError::Invalid("the application has no tenant".into()))?;
            if application["name"].as_str() != Some("compute") {
                return Err(StateError::Invalid(format!(
                    "FeltDB application {application_id} is not named compute"
                )));
            }
            (tenant.to_string(), application_id.clone())
        }
        None => {
            // Adopt what already exists, so provisioning twice never
            // duplicates a tenant or an application.
            let tenant_id = match &request.tenant_id {
                Some(tenant) => tenant.clone(),
                None => {
                    let tenants = client
                        .send(reqwest::Method::GET, "/api/tenants", None)
                        .await
                        .map_err(|refusal| failure("list tenants", refusal))?;
                    let existing = tenants.as_array().into_iter().flatten().find(|tenant| {
                        tenant["name"].as_str() == Some(request.tenant_name.as_str())
                    });
                    match existing.and_then(|tenant| tenant["id"].as_str()) {
                        Some(id) => id.to_string(),
                        None => post(
                            "/api/tenants".into(),
                            json!({ "name": request.tenant_name }),
                        )
                        .await
                        .map_err(|refusal| failure("create a tenant", refusal))?["id"]
                            .as_str()
                            .ok_or_else(|| {
                                StateError::Invalid("FeltDB returned no tenant ID".into())
                            })?
                            .to_string(),
                    }
                }
            };
            let applications = client
                .send(
                    reqwest::Method::GET,
                    &format!("/api/tenants/{}/applications", encode(&tenant_id)),
                    None,
                )
                .await
                .map_err(|refusal| failure("list applications", refusal))?;
            let existing = applications
                .as_array()
                .into_iter()
                .flatten()
                .find(|application| application["name"].as_str() == Some("compute"))
                .and_then(|application| application["id"].as_str())
                .map(str::to_owned);
            let application_id = match existing {
                Some(id) => id,
                None => post(
                    format!("/api/tenants/{}/applications", encode(&tenant_id)),
                    json!({ "name": "compute" }),
                )
                .await
                .map_err(|refusal| failure("create the Compute application", refusal))?["id"]
                    .as_str()
                    .ok_or_else(|| StateError::Invalid("FeltDB returned no application ID".into()))?
                    .to_string(),
            };
            (tenant_id, application_id)
        }
    };
    let mut manifest: Value = serde_json::from_str(compiled)
        .map_err(|error| StateError::Invalid(format!("embedded manifest: {error}")))?;
    manifest["tenant_id"] = json!(tenant_id);
    manifest["application_id"] = json!(application_id);

    // Already current? Compare with the active revision's collections.
    let current = client
        .send(
            reqwest::Method::GET,
            &format!(
                "/v1/application?application_id={}&environment={}",
                encode(&application_id),
                encode(&request.environment)
            ),
            None,
        )
        .await
        .ok();
    // The active revision, when there is one.
    let active = match current
        .as_ref()
        .and_then(|current| current["revision_id"].as_str())
    {
        Some(revision_id) => client
            .send(
                reqwest::Method::GET,
                &format!(
                    "/api/applications/{}/revisions/{}",
                    encode(&application_id),
                    encode(revision_id)
                ),
                None,
            )
            .await
            .ok()
            .map(|revision| (revision_id.to_string(), revision)),
        None => None,
    };
    if let Some((revision_id, revision)) = &active
        && same_model(&revision["manifest"], &manifest)
    {
        return Ok(Provisioned {
            tenant_id,
            application_id,
            environment: request.environment,
            revision_id: revision_id.clone(),
            changed: false,
        });
    }
    // FeltDB requires the state schema version to increase with every
    // schema change.
    if let Some((_, revision)) = &active {
        let version = revision["manifest"]["state_schema_version"]
            .as_u64()
            .unwrap_or(1);
        manifest["state_schema_version"] = json!(version + 1);
    }

    let base = current
        .as_ref()
        .and_then(|current| current["revision_id"].as_str())
        .map(str::to_owned);
    let draft = post(
        format!("/api/applications/{}/drafts", encode(&application_id)),
        json!({ "base_revision_id": base }),
    )
    .await
    .map_err(|refusal| failure("create a draft", refusal))?;
    let draft_id = draft["draft_id"]
        .as_str()
        .ok_or_else(|| StateError::Invalid("FeltDB returned no draft ID".into()))?
        .to_string();
    let draft_version = draft["version"].as_u64().unwrap_or(1);
    let put = client
        .http
        .put(format!(
            "{}/api/applications/{}/drafts/{}",
            client.config.url,
            encode(&application_id),
            encode(&draft_id)
        ))
        .bearer_auth(&request.token)
        .header("FeltDB-Protocol", "1")
        .header("If-Version", draft_version.to_string())
        .json(&json!({ "manifest": manifest }))
        .send()
        .await
        .map_err(|error| StateError::Unavailable(error.to_string()))?;
    if !put.status().is_success() {
        let status = put.status();
        let body = put.text().await.unwrap_or_default();
        return Err(StateError::Invalid(format!(
            "FeltDB refused the Compute model ({status}): {body}"
        )));
    }
    let validation = post(
        format!(
            "/api/applications/{}/drafts/{}/validate",
            encode(&application_id),
            encode(&draft_id)
        ),
        json!({}),
    )
    .await
    .map_err(|refusal| failure("validate the Compute model", refusal))?;
    if validation["valid"].as_bool() != Some(true) {
        return Err(StateError::Invalid(format!(
            "FeltDB rejected the Compute model: {}",
            validation["issues"]
        )));
    }
    let revision = post(
        format!(
            "/api/applications/{}/drafts/{}/commit",
            encode(&application_id),
            encode(&draft_id)
        ),
        json!({}),
    )
    .await
    .map_err(|refusal| failure("commit the Compute model", refusal))?;
    let revision_id = revision["revision_id"]
        .as_str()
        .ok_or_else(|| StateError::Invalid("FeltDB returned no revision ID".into()))?
        .to_string();
    post(
        format!(
            "/api/applications/{}/revisions/{}/promote",
            encode(&application_id),
            encode(&revision_id)
        ),
        json!({
            "environment": request.environment,
            "reason": format!("Compute control model {}", compute_state::STATE_VERSION),
            "expected_current_revision": base,
        }),
    )
    .await
    .map_err(|refusal| failure("promote the Compute model", refusal))?;
    Ok(Provisioned {
        tenant_id,
        application_id,
        environment: request.environment,
        revision_id,
        changed: true,
    })
}

/// Whether a stored manifest declares the same collections, fields,
/// indexes, and policies as ours. FeltDB normalizes manifests, so compare
/// what Compute defines rather than the whole document.
fn same_model(stored: &Value, ours: &Value) -> bool {
    let fields = |manifest: &Value| {
        let mut collections = manifest["collections"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|collection| {
                let mut fields = collection["fields"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|field| {
                        (
                            field["name"].as_str().unwrap_or_default().to_string(),
                            field["type"].as_str().unwrap_or_default().to_string(),
                            field["required"].as_bool().unwrap_or_default(),
                        )
                    })
                    .collect::<Vec<_>>();
                fields.sort();
                (
                    collection["name"].as_str().unwrap_or_default().to_string(),
                    fields,
                )
            })
            .collect::<Vec<_>>();
        collections.sort();
        collections
    };
    let names = |manifest: &Value, key: &str| {
        let mut names = manifest[key]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| item["name"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    fields(stored) == fields(ours)
        && names(stored, "indexes") == names(ours, "indexes")
        && names(stored, "policies") == names(ours, "policies")
}
