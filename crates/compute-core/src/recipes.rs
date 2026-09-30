//! Recipes: lifecycle policy, declared as data.
//!
//! A Recipe is not an execution path. It is a durable, user-owned
//! declaration of *how existing Compute primitives should be used*, and it
//! is built entirely from existing vocabulary: [`ComputerLifecycle`],
//! [`ComputerRequirements`] (which already carry resources, network,
//! isolation, session capabilities, target features, and runtimes), and an
//! execution policy. It resolves, deterministically and without side
//! effects, to the request Compute already accepts for an environment's
//! computer. There is no Recipe computer, workload, or lifecycle.
//!
//! Provider and target identity are deliberately not part of a Recipe: it
//! states requirements, and placement finds the target.

use serde::{Deserialize, Serialize};

use crate::{ComputerLifecycle, ComputerRequirements};

/// The document format of a recipe's spec.
pub const RECIPE_FORMAT: &str = "compute.recipe@1";

/// The lifetime an ephemeral computer gets when a recipe names none: the
/// same default `compute environment create` uses.
pub const DEFAULT_RECIPE_TTL_SECONDS: u64 = 3600;

/// What a recipe declares. Every field is an existing Compute contract.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeSpec {
    /// What the recipe is for, in the author's words. Not interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the computer outlives the work: the existing
    /// `ComputerLifecycle`. Persistent computers are kept until destroyed;
    /// ephemeral ones are torn down by Compute when their TTL passes.
    #[serde(default)]
    pub lifecycle: ComputerLifecycle,
    /// An ephemeral computer's lifetime. Absent: the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    /// What the computer must be. Resources, network, isolation, session
    /// capabilities (`persistent_storage`, `terminal`, ...), target
    /// features, and runtimes are all here, exactly as an environment
    /// states them.
    #[serde(default)]
    pub requirements: ComputerRequirements,
    /// An execution policy (`compute.policy@1`) intersected into every
    /// admission in environments made from the recipe. It can only
    /// restrict. Validated by the control plane against the policy model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<serde_json::Value>,
}

impl RecipeSpec {
    /// Whether the spec is well formed and its lifecycle combination is one
    /// Compute supports. This is the pure part of validation; whether any
    /// target can satisfy the requirements is a different question, asked
    /// of placement.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = vec![];
        if let Err(error) = self.requirements.validate() {
            problems.push(error.to_string());
        }
        match (self.lifecycle, self.ttl_seconds) {
            (ComputerLifecycle::Persistent, Some(_)) => {
                problems.push("a persistent computer has no TTL".into())
            }
            (_, Some(0)) => problems.push("a computer's TTL must be greater than zero".into()),
            _ => {}
        }
        if let Some(description) = &self.description
            && description.len() > 1024
        {
            problems.push("the description is longer than 1024 bytes".into());
        }
        problems
    }

    /// The TTL the computer will get: only an ephemeral computer has one.
    pub fn effective_ttl_seconds(&self) -> Option<u64> {
        (self.lifecycle == ComputerLifecycle::Ephemeral)
            .then(|| self.ttl_seconds.unwrap_or(DEFAULT_RECIPE_TTL_SECONDS))
    }
}

/// The identity of one immutable version of a recipe. An environment made
/// from a recipe records it, so what produced it never depends on whatever
/// the recipe says now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeRef {
    pub name: String,
    pub version: u64,
    /// `sha256:` of the version's canonical spec.
    pub digest: String,
}

/// The digest that names a spec's content.
pub fn recipe_digest(spec: &RecipeSpec) -> String {
    crate::sha256_identity(&serde_json::to_vec(spec).expect("a recipe spec serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_with_a_ttl_is_a_problem() {
        let spec = RecipeSpec {
            ttl_seconds: Some(60),
            ..RecipeSpec::default()
        };
        assert_eq!(spec.problems(), ["a persistent computer has no TTL"]);
    }

    #[test]
    fn only_ephemeral_computers_have_a_ttl() {
        let mut spec = RecipeSpec::default();
        assert_eq!(spec.effective_ttl_seconds(), None);
        spec.lifecycle = ComputerLifecycle::Ephemeral;
        assert_eq!(spec.effective_ttl_seconds(), Some(3600));
        spec.ttl_seconds = Some(90);
        assert_eq!(spec.effective_ttl_seconds(), Some(90));
    }

    #[test]
    fn the_digest_names_the_content() {
        let one = RecipeSpec::default();
        let mut two = one.clone();
        assert_eq!(recipe_digest(&one), recipe_digest(&two));
        two.description = Some("changed".into());
        assert_ne!(recipe_digest(&one), recipe_digest(&two));
    }
}
