//! Install or upgrade the Compute control model in Managed FeltDB.
//!
//! FeltDB applications evolve through drafts: a draft holds a manifest, is
//! validated, is committed as an immutable revision, and a revision is
//! promoted to an environment. Compute uses exactly that path.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{COMPUTE_MANIFEST, FeltDbConfig, FeltDbState};
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
    let state = FeltDbState::new(FeltDbConfig {
        url: request.url.clone(),
        token: request.token.clone(),
        application_id: request.application_id.clone().unwrap_or_default(),
        environment: request.environment.clone(),
        ca_certificate: request.ca_certificate.clone(),
    })?;
    let control = state.client.control();
    let refused = |step: &'static str| {
        let state = &state;
        move |error: feltdb_client::Error| failure(step, state.refusal(error))
    };
    let (tenant_id, application_id) = match &request.application_id {
        Some(application_id) => {
            let application = control
                .application(application_id)
                .await
                .map_err(refused("read the Compute application"))?;
            if application.name != "compute" {
                return Err(StateError::Invalid(format!(
                    "FeltDB application {application_id} is not named compute"
                )));
            }
            (application.tenant_id, application_id.clone())
        }
        None => {
            // Adopt what already exists, so provisioning twice never
            // duplicates a tenant or an application.
            let tenant_id = match &request.tenant_id {
                Some(tenant) => tenant.clone(),
                None => {
                    let tenants = control.tenants().await.map_err(refused("list tenants"))?;
                    match tenants
                        .into_iter()
                        .find(|tenant| tenant.name == request.tenant_name)
                    {
                        Some(tenant) => tenant.id,
                        None => {
                            control
                                .create_tenant(&request.tenant_name)
                                .await
                                .map_err(refused("create a tenant"))?
                                .id
                        }
                    }
                }
            };
            let existing = control
                .applications(&tenant_id)
                .await
                .map_err(refused("list applications"))?
                .into_iter()
                .find(|application| application.name == "compute");
            let application_id = match existing {
                Some(application) => application.id,
                None => {
                    control
                        .create_application(&tenant_id, "compute")
                        .await
                        .map_err(refused("create the Compute application"))?
                        .id
                }
            };
            (tenant_id, application_id)
        }
    };
    let mut manifest: Value = serde_json::from_str(compiled)
        .map_err(|error| StateError::Invalid(format!("embedded manifest: {error}")))?;
    manifest["tenant_id"] = json!(tenant_id);
    manifest["application_id"] = json!(application_id);

    // Already current? Compare with the active revision's collections.
    let base = state
        .client
        .application(application_id.clone())
        .environment(request.environment.clone())
        .discover()
        .await
        .ok()
        .map(|current| current.revision_id);
    // The active revision, when there is one, with its manifest as FeltDB
    // serializes it.
    let active = match &base {
        Some(revision_id) => control
            .revision(&application_id, revision_id)
            .await
            .ok()
            .and_then(|revision| serde_json::to_value(revision.manifest).ok())
            .map(|manifest| (revision_id.clone(), manifest)),
        None => None,
    };
    // Never replace a model this build does not fully know: that would
    // drop collections, fields, or indexes a newer Compute relies on.
    if let Some((revision_id, active_manifest)) = &active {
        let comparison = compare_models(active_manifest, &manifest);
        if !comparison.unknown.is_empty() {
            return Err(StateError::Invalid(format!(
                "refusing to downgrade the Compute model: the active revision {revision_id} declares {} that this build does not ({}); upgrade Compute instead",
                comparison.unknown.len(),
                comparison.unknown.join(", ")
            )));
        }
    }
    if let Some((revision_id, active_manifest)) = &active
        && same_model(active_manifest, &manifest)
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
    if let Some((_, active_manifest)) = &active {
        let version = active_manifest["state_schema_version"]
            .as_u64()
            .unwrap_or(1);
        manifest["state_schema_version"] = json!(version + 1);
    }
    let manifest: feltdb_client::feltdb::application::ApplicationManifest =
        serde_json::from_value(manifest).map_err(|error| {
            StateError::Invalid(format!(
                "the Compute model is not a FeltDB manifest: {error}"
            ))
        })?;

    let draft = control
        .create_draft(&application_id, base.as_deref())
        .await
        .map_err(refused("create a draft"))?;
    // The draft is replaced only at the version just read.
    control
        .update_draft(&application_id, &draft.draft_id, draft.version, &manifest)
        .await
        .map_err(|error| match state.refusal(error) {
            Ok(refusal) => StateError::Invalid(format!(
                "FeltDB refused the Compute model ({} {}): {}",
                refusal.status, refusal.code, refusal.message
            )),
            Err(error) => error,
        })?;
    let validation = control
        .validate_draft(&application_id, &draft.draft_id)
        .await
        .map_err(refused("validate the Compute model"))?;
    if !validation.valid {
        return Err(StateError::Invalid(format!(
            "FeltDB rejected the Compute model: {}",
            serde_json::to_value(&validation.issues).unwrap_or_default()
        )));
    }
    let revision_id = control
        .commit_draft(&application_id, &draft.draft_id)
        .await
        .map_err(refused("commit the Compute model"))?
        .revision_id;
    control
        .promote(
            &application_id,
            &revision_id,
            &feltdb_client::Promotion::new(request.environment.clone())
                .reason(format!(
                    "Compute control model {}",
                    compute_state::STATE_VERSION
                ))
                .expect_current(base.clone()),
        )
        .await
        .map_err(refused("promote the Compute model"))?;
    Ok(Provisioned {
        tenant_id,
        application_id,
        environment: request.environment,
        revision_id,
        changed: true,
    })
}

/// How the active model relates to this build's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRelation {
    /// No Compute model is active.
    Absent,
    /// The active model is this build's.
    Current,
    /// This build only adds to the active model: upgrade.
    Older,
    /// The active model declares what this build does not: a newer Compute
    /// installed it. Refused: a downgrade would discard it.
    Newer,
    /// Each declares something the other does not. Refused.
    Divergent,
}

/// The difference between the active model and this build's, element by
/// element (`Collection`, `Collection.field:type:required`, index and
/// policy names).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelComparison {
    pub relation: ModelRelation,
    /// What this build adds to the active model.
    pub additions: Vec<String>,
    /// What the active model declares that this build does not.
    pub unknown: Vec<String>,
}

fn model_elements(manifest: &Value) -> std::collections::BTreeSet<String> {
    let mut elements = std::collections::BTreeSet::new();
    for collection in manifest["collections"].as_array().into_iter().flatten() {
        let name = collection["name"].as_str().unwrap_or_default();
        elements.insert(format!("collection {name}"));
        for field in collection["fields"].as_array().into_iter().flatten() {
            elements.insert(format!(
                "field {name}.{}:{}{}",
                field["name"].as_str().unwrap_or_default(),
                field["type"].as_str().unwrap_or_default(),
                if field["required"].as_bool().unwrap_or_default() {
                    ""
                } else {
                    "?"
                }
            ));
        }
    }
    for (key, kind) in [("indexes", "index"), ("policies", "policy")] {
        for item in manifest[key].as_array().into_iter().flatten() {
            elements.insert(format!(
                "{kind} {}",
                item["name"].as_str().unwrap_or_default()
            ));
        }
    }
    elements
}

/// Compare the active model (`stored`, or `Value::Null` when none) with
/// this build's.
pub fn compare_models(stored: &Value, ours: &Value) -> ModelComparison {
    if stored.is_null() || stored["collections"].is_null() {
        return ModelComparison {
            relation: ModelRelation::Absent,
            additions: vec![],
            unknown: vec![],
        };
    }
    let active = model_elements(stored);
    let wanted = model_elements(ours);
    let additions = wanted.difference(&active).cloned().collect::<Vec<_>>();
    let unknown = active.difference(&wanted).cloned().collect::<Vec<_>>();
    let relation = match (additions.is_empty(), unknown.is_empty()) {
        (true, true) => ModelRelation::Current,
        (false, true) => ModelRelation::Older,
        (true, false) => ModelRelation::Newer,
        (false, false) => ModelRelation::Divergent,
    };
    ModelComparison {
        relation,
        additions,
        unknown,
    }
}

/// The Compute model active in a FeltDB environment, compared with this
/// build's. Read-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInspection {
    pub application_id: String,
    pub environment: String,
    pub active_revision: Option<String>,
    pub active_schema_version: Option<u64>,
    /// This build's model.
    pub required: String,
    pub required_generation: u32,
    pub comparison: ModelComparison,
}

/// Inspect the active Compute model without changing anything.
pub async fn inspect_model(config: &FeltDbConfig) -> Result<ModelInspection, StateError> {
    let state = FeltDbState::new(config.clone())?;
    let ours: Value = serde_json::from_str(COMPUTE_MANIFEST)
        .map_err(|error| StateError::Invalid(format!("embedded manifest: {error}")))?;
    let active_revision = state
        .application
        .discover()
        .await
        .ok()
        .map(|current| current.revision_id);
    let stored = match &active_revision {
        Some(revision_id) => {
            let revision = state
                .client
                .control()
                .revision(&config.application_id, revision_id)
                .await
                .map_err(|error| failure("read the active Compute model", state.refusal(error)))?;
            json!({ "manifest": serde_json::to_value(revision.manifest).unwrap_or_default() })
        }
        None => Value::Null,
    };
    Ok(ModelInspection {
        application_id: config.application_id.clone(),
        environment: config.environment.clone(),
        active_schema_version: stored["manifest"]["state_schema_version"].as_u64(),
        active_revision,
        required: compute_state::STATE_VERSION.into(),
        required_generation: compute_state::MODEL_GENERATION,
        comparison: compare_models(&stored["manifest"], &ours),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The model is sent to FeltDB as its own `ApplicationManifest`, the
    /// struct the server stores it as: nothing Compute declares may be lost
    /// on the way.
    #[test]
    fn the_compute_model_is_a_feltdb_manifest_without_loss() {
        let compiled: Value = serde_json::from_str(COMPUTE_MANIFEST).unwrap();
        let typed: feltdb_client::feltdb::application::ApplicationManifest =
            serde_json::from_value(compiled.clone()).unwrap();
        let round_trip = serde_json::to_value(&typed).unwrap();
        assert!(same_model(&round_trip, &compiled));
        assert!(compare_models(&round_trip, &compiled).unknown.is_empty());
        assert!(compare_models(&compiled, &round_trip).unknown.is_empty());
        for key in ["collections", "indexes", "policies"] {
            assert_eq!(
                round_trip[key].as_array().map(Vec::len),
                compiled[key].as_array().map(Vec::len),
                "{key}"
            );
        }
    }
}
