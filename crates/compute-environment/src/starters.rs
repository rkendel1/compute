//! Immutable recipe templates shipped with Compute.
//!
//! The JSON documents under `recipes/starters` are the single source of
//! truth. They are embedded for controller-free CLI discovery and copied
//! byte-for-byte into distributions for inspection and certification.

use compute_core::RecipeSpec;
use serde::{Deserialize, Serialize};

pub const STARTER_PLATFORMS: [&str; 2] = ["linux-x86_64", "macos-aarch64"];

const ASSETS: [(&str, &str, &str); 7] = [
    (
        "agent-task",
        "Agent task",
        include_str!("../../../recipes/starters/agent-task.json"),
    ),
    (
        "ci",
        "CI",
        include_str!("../../../recipes/starters/ci.json"),
    ),
    (
        "dev",
        "Development",
        include_str!("../../../recipes/starters/dev.json"),
    ),
    (
        "migration",
        "Migration",
        include_str!("../../../recipes/starters/migration.json"),
    ),
    (
        "preview",
        "Preview",
        include_str!("../../../recipes/starters/preview.json"),
    ),
    (
        "production",
        "Production",
        include_str!("../../../recipes/starters/production.json"),
    ),
    (
        "staging",
        "Staging",
        include_str!("../../../recipes/starters/staging.json"),
    ),
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeStarter {
    pub id: String,
    pub display_name: String,
    pub source: String,
    pub template_status: String,
    pub platforms: Vec<String>,
    pub spec: RecipeSpec,
}

pub fn recipe_starter_assets() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    ASSETS
        .map(|(id, _, json)| (id, json.as_bytes()))
        .into_iter()
}

pub fn recipe_starters() -> Vec<RecipeStarter> {
    ASSETS
        .map(|(id, display_name, json)| RecipeStarter {
            id: id.into(),
            display_name: display_name.into(),
            source: "compute-distribution".into(),
            template_status: "immutable".into(),
            platforms: STARTER_PLATFORMS.map(str::to_owned).into(),
            spec: serde_json::from_str(json).expect("shipped starter recipe must be valid"),
        })
        .into()
}

pub fn recipe_starter(id: &str) -> Option<RecipeStarter> {
    recipe_starters()
        .into_iter()
        .find(|starter| starter.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_complete_ordered_and_valid() {
        let starters = recipe_starters();
        assert_eq!(
            starters
                .iter()
                .map(|starter| starter.id.as_str())
                .collect::<Vec<_>>(),
            [
                "agent-task",
                "ci",
                "dev",
                "migration",
                "preview",
                "production",
                "staging"
            ]
        );
        assert!(
            starters
                .iter()
                .all(|starter| starter.spec.problems().is_empty())
        );
    }

    #[test]
    fn assets_and_catalog_have_one_to_one_identity() {
        assert_eq!(
            recipe_starter_assets()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            recipe_starters()
                .iter()
                .map(|starter| starter.id.as_str())
                .collect::<Vec<_>>()
        );
    }
}
