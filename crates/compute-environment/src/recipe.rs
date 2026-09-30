//! Recipes: the pure half. A recipe spec resolves, without side effects, to
//! the request the control plane already accepts for an environment's
//! computer. The daemon half (`daemon/recipes.rs`) stores versions and asks
//! placement whether the resolution can be satisfied; neither half executes
//! anything.
//!
//! ```text
//! RecipeSpec ──resolve──▶ ComputerRequest + Policy      what `POST /environments` takes
//!                              │
//!                              └─ placement (read-only) ─▶ can any target host it?
//! ```

use compute_core::{ComputerLifecycle, RecipeRef, RecipeSpec};
use compute_placement::PlacementReport;
use compute_policy::Policy;
use serde::{Deserialize, Serialize};

use crate::model::{ComputerRequest, validate_name};

/// Write a recipe: create it, or edit it into its next version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeDefinition {
    pub name: String,
    pub spec: RecipeSpec,
    /// The version being edited. Absent to create; an edit is refused with
    /// `conflict` unless it names the current version, so nobody overwrites
    /// a change they have not seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<u64>,
}

/// One version of a recipe, as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeView {
    pub name: String,
    pub version: u64,
    /// `current` or `superseded`.
    pub status: String,
    pub digest: String,
    pub spec: RecipeSpec,
    pub author: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<compute_state::RecipeRecord> for RecipeView {
    fn from(record: compute_state::RecipeRecord) -> Self {
        Self {
            name: record.name,
            version: record.version,
            status: match record.status {
                compute_state::RecipeStatus::Current => "current",
                compute_state::RecipeStatus::Superseded => "superseded",
            }
            .into(),
            digest: record.digest,
            spec: record.spec,
            author: record.author,
            created_at: record.created_at,
        }
    }
}

/// Preview a recipe: a stored one by name in the path, or a draft spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RecipeResolveRequest {
    pub spec: Option<RecipeSpec>,
    /// Constrain placement to one target, as `environment create --target`
    /// does. Never part of a recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// What a recipe resolves to: the existing contracts, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRecipe {
    /// The computer an environment made from the recipe asks for: the same
    /// `ComputerRequest` `POST /environments` and work sessions take.
    pub computer: ComputerRequest,
    /// The execution policy intersected into every admission there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Policy>,
}

/// Whether a recipe can be used, in the order the questions are asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeVerdict {
    /// The recipe is malformed or cannot resolve. Nothing was asked of
    /// placement.
    Invalid,
    /// The recipe is valid and no current target satisfies its
    /// requirements.
    Unsatisfied,
    /// The recipe is valid and a target can host it now.
    Satisfiable,
}

/// One existing operation a recipe's resolution asks of Compute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrimitiveStep {
    /// The existing operation (`environment create`, `environment destroy`).
    pub operation: String,
    pub effect: String,
}

/// What a recipe will cause Compute to do, before anything is done. Read
/// only: resolving acquires nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeResolution {
    /// The recipe version resolved. Absent for a draft spec that is not
    /// stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<RecipeRef>,
    pub verdict: RecipeVerdict,
    /// Why the recipe is invalid: every problem, not the first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    /// Present unless invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedRecipe>,
    /// Session capabilities placement will require beyond the recipe's own:
    /// what the lifecycle itself needs from the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implied_capabilities: Vec<String>,
    /// The lifecycle the resolution requests, in Compute's words.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lifecycle: Vec<String>,
    /// The existing operations that carry it out, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub primitives: Vec<PrimitiveStep>,
    /// Placement's own report (the evidence a real placement records):
    /// every target, compatible or not, with reasons. Absent when invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementReport>,
}

/// Every problem that makes a spec unusable, or the resolution.
pub fn resolve(spec: &RecipeSpec) -> Result<ResolvedRecipe, Vec<String>> {
    let mut problems = spec.problems();
    let policy = match &spec.policy {
        None => None,
        Some(value) => match serde_json::to_vec(value)
            .map_err(|error| error.to_string())
            .and_then(|bytes| Policy::from_json(&bytes).map_err(|error| error.to_string()))
        {
            Ok(policy) => Some(policy),
            Err(error) => {
                problems.push(format!("the policy is invalid: {error}"));
                None
            }
        },
    };
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(ResolvedRecipe {
        computer: ComputerRequest {
            lifecycle: spec.lifecycle,
            requirements: spec.requirements.clone(),
            target: None,
            ttl_seconds: spec.effective_ttl_seconds(),
        },
        policy,
    })
}

/// A recipe's name is a name like an environment's.
pub fn validate_recipe_name(name: &str) -> Result<(), crate::EnvironmentError> {
    validate_name("recipe", name)
}

/// The capabilities the lifecycle itself needs from a target, which
/// placement adds to the recipe's own.
pub fn implied_capabilities(lifecycle: ComputerLifecycle) -> Vec<String> {
    match lifecycle {
        // A persistent computer must never expire, so its target must be
        // able to hold a session without a TTL.
        ComputerLifecycle::Persistent => vec!["claim".into()],
        ComputerLifecycle::Ephemeral => vec![],
    }
}

/// The lifecycle a resolution requests, in Compute's existing vocabulary,
/// and the existing operations that carry it out. Derived from the spec
/// alone, so it is the same before and after anything runs.
pub fn explain(spec: &RecipeSpec) -> (Vec<String>, Vec<PrimitiveStep>) {
    let mut lifecycle = vec![];
    let mut primitives = vec![PrimitiveStep {
        operation: "environment create (computer)".into(),
        effect: "records the environment and its computer, places it on a target that satisfies \
                 the requirements, and provisions a session there"
            .into(),
    }];
    match spec.lifecycle {
        ComputerLifecycle::Persistent => {
            lifecycle.push(
                "persistent: the computer is kept until it is destroyed; Compute never expires it"
                    .into(),
            );
            if !spec
                .requirements
                .capabilities
                .iter()
                .any(|name| name == "persistent_storage")
            {
                lifecycle.push(
                    "no `persistent_storage` capability is required: the computer does not \
                     expire, but the recipe does not ask the target to promise the disk survives \
                     the machine"
                        .into(),
                );
            }
            primitives.push(PrimitiveStep {
                operation: "environment destroy".into(),
                effect: "the only release: explicit; the record stays as evidence".into(),
            });
        }
        ComputerLifecycle::Ephemeral => {
            let ttl = spec.effective_ttl_seconds().unwrap_or_default();
            lifecycle.push(format!(
                "ephemeral: Compute tears the computer down when its {ttl}s TTL passes \
                 (`expired`); the record stays as evidence"
            ));
            primitives.push(PrimitiveStep {
                operation: "computer expiry (TTL)".into(),
                effect: "automatic release by the existing controller".into(),
            });
            primitives.push(PrimitiveStep {
                operation: "environment destroy".into(),
                effect: "explicit release, at any time before expiry".into(),
            });
        }
    }
    lifecycle.push(format!(
        "isolation: {}; network: {}",
        spec.requirements.isolation.as_str(),
        match spec.requirements.network {
            compute_core::NetworkPolicy::None => "none",
            compute_core::NetworkPolicy::Localhost => "localhost",
            compute_core::NetworkPolicy::Network => "network",
        }
    ));
    if spec.policy.is_some() {
        lifecycle.push("an execution policy is intersected into every admission".into());
    }
    (lifecycle, primitives)
}

/// Whether an environment's requested computer is what `recipe` resolves
/// to. The placement constraint (`target`) is the caller's, never the
/// recipe's, so it is not compared.
pub fn matches_resolution(
    resolved: &ResolvedRecipe,
    computer: &ComputerRequest,
    policy: Option<&Policy>,
) -> bool {
    let mut requested = computer.clone();
    requested.target = None;
    requested == resolved.computer
        && policy.cloned().map(Policy::canonical) == resolved.policy.clone().map(Policy::canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use compute_core::{ComputerRequirements, IsolationProfile, NetworkPolicy};

    #[test]
    fn a_recipe_resolves_to_the_existing_computer_request() {
        let spec = RecipeSpec {
            lifecycle: ComputerLifecycle::Ephemeral,
            ttl_seconds: Some(900),
            requirements: ComputerRequirements {
                cpu_count: Some(2),
                isolation: IsolationProfile::Sandboxed,
                network: NetworkPolicy::None,
                ..Default::default()
            },
            ..Default::default()
        };
        let resolved = resolve(&spec).unwrap();
        assert_eq!(resolved.computer.lifecycle, ComputerLifecycle::Ephemeral);
        assert_eq!(resolved.computer.ttl_seconds, Some(900));
        assert_eq!(resolved.computer.requirements, spec.requirements);
        assert_eq!(resolved.computer.target, None, "no target in a recipe");
    }

    #[test]
    fn every_problem_is_reported_not_the_first() {
        let spec = RecipeSpec {
            ttl_seconds: Some(60),
            requirements: ComputerRequirements {
                features: vec!["quantum".into()],
                ..Default::default()
            },
            policy: Some(serde_json::json!({ "nonsense": true })),
            ..Default::default()
        };
        let problems = resolve(&spec).unwrap_err();
        assert!(problems.len() >= 3, "{problems:?}");
    }

    #[test]
    fn a_persistent_recipe_says_what_it_does_not_promise() {
        let (lifecycle, _) = explain(&RecipeSpec::default());
        assert!(
            lifecycle
                .iter()
                .any(|line| line.contains("persistent_storage"))
        );
        let spec = RecipeSpec {
            requirements: ComputerRequirements {
                capabilities: vec!["persistent_storage".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let (lifecycle, _) = explain(&spec);
        assert!(!lifecycle.iter().any(|line| line.contains("does not ask")));
    }
}
