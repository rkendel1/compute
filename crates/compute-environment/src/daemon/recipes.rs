//! Recipes: store versions of a lifecycle policy, and explain what one
//! resolves to. Nothing here executes, provisions, supervises, or releases
//! anything: a recipe reaches Compute only as the ordinary
//! `ComputerRequest` of `create_computer_environment`, which places, owns,
//! and releases the computer exactly as it does for any caller.
//!
//! **Versions are immutable.** Editing writes the next version and marks the
//! previous one superseded in the same transaction, fenced by the version
//! the editor read. An environment records the version (name, number,
//! digest) it was made from, so what produced it never depends on what the
//! recipe says now.
//!
//! **State access.** A version is read by identity (`rcp_` + name and
//! version). The current version of a name, and the list of recipes, are
//! indexed equality queries (`name` and `status`), bounded and limited by
//! FeltDB; nothing scans the collection.

use std::sync::Arc;

use chrono::Utc;
use compute_core::{ComputerSpec, RecipeRef, RecipeSpec, recipe_digest};
use compute_placement::PlacementOutcome;
use compute_state::{Collection, Query, RecipeRecord, RecipeStatus, Stored, events, ids};
use serde_json::json;

use super::{Change, Daemon, Scope};
use crate::EnvironmentError;
use crate::recipe::*;

/// Bound on the recipes listed: current versions only.
const LIST_LIMIT: usize = 500;

impl Daemon {
    /// Every recipe, at its current version.
    pub async fn recipes(&self) -> Result<Vec<RecipeView>, EnvironmentError> {
        let mut recipes = self
            .control()
            .query::<RecipeRecord>(
                Query::all(Collection::Recipe)
                    .eq("status", "current")
                    .limit(LIST_LIMIT),
            )
            .await?
            .into_iter()
            .map(|stored| RecipeView::from(stored.value))
            .collect::<Vec<_>>();
        recipes.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(recipes)
    }

    /// One recipe: a version by identity, or the current one.
    pub async fn recipe(
        &self,
        name: &str,
        version: Option<u64>,
    ) -> Result<RecipeView, EnvironmentError> {
        Ok(self.stored_recipe(name, version).await?.value.into())
    }

    async fn stored_recipe(
        &self,
        name: &str,
        version: Option<u64>,
    ) -> Result<Stored<RecipeRecord>, EnvironmentError> {
        let found = match version {
            Some(version) => {
                self.control()
                    .get::<RecipeRecord>(&ids::recipe(name, version))
                    .await?
            }
            None => self
                .control()
                .query::<RecipeRecord>(
                    Query::all(Collection::Recipe)
                        .eq("name", name)
                        .eq("status", "current")
                        .limit(1),
                )
                .await?
                .into_iter()
                .next(),
        };
        found.ok_or_else(|| {
            EnvironmentError::NotFound(match version {
                Some(version) => format!("recipe {name} version {version}"),
                None => format!("recipe {name}"),
            })
        })
    }

    /// Create a recipe, or edit it into its next version. An invalid spec
    /// is refused before anything is written.
    pub async fn write_recipe(
        self: &Arc<Self>,
        operator: &str,
        definition: RecipeDefinition,
    ) -> Result<RecipeView, EnvironmentError> {
        validate_recipe_name(&definition.name)?;
        resolve(&definition.spec)
            .map_err(|problems| EnvironmentError::Invalid(problems.join("; ")))?;
        let current = match self.stored_recipe(&definition.name, None).await {
            Ok(current) => Some(current),
            Err(EnvironmentError::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let version = match (&current, definition.expected_version) {
            (None, None) => 1,
            (None, Some(_)) => {
                return Err(EnvironmentError::Conflict(format!(
                    "recipe {} does not exist; create it without expected_version",
                    definition.name
                )));
            }
            (Some(current), None) => {
                return Err(EnvironmentError::Conflict(format!(
                    "recipe {} exists at version {}; edit it with expected_version {}",
                    definition.name, current.value.version, current.value.version
                )));
            }
            (Some(current), Some(expected)) if expected != current.value.version => {
                return Err(EnvironmentError::Conflict(format!(
                    "recipe {} is at version {}, not {expected}; read it again before editing",
                    definition.name, current.value.version
                )));
            }
            (Some(current), Some(_)) => current.value.version + 1,
        };
        let digest = recipe_digest(&definition.spec);
        // An edit that changes nothing is not a new version.
        if let Some(current) = &current
            && current.value.digest == digest
        {
            return Ok(current.value.clone().into());
        }
        let id = ids::recipe(&definition.name, version);
        let record = RecipeRecord {
            recipe_id: id.clone(),
            name: definition.name.clone(),
            version,
            status: RecipeStatus::Current,
            digest: digest.clone(),
            spec: definition.spec,
            author: operator.to_owned(),
            created_at: Utc::now(),
        };
        let change = Change::new().with(|batch| {
            let batch = match &current {
                Some(current) => {
                    let mut superseded = current.value.clone();
                    superseded.status = RecipeStatus::Superseded;
                    batch.replace(current, &superseded)
                }
                None => batch,
            };
            batch.create(&id, &record)
        });
        let change = self.event(
            change,
            events::RECIPE_WRITTEN,
            Scope::default(),
            format!("{operator} wrote recipe {} version {version}", record.name),
            json!({ "recipe": record.name, "version": version, "digest": digest }),
        );
        self.apply(change).await?;
        Ok(record.into())
    }

    /// What a stored recipe will cause Compute to do. Read only.
    pub async fn resolve_recipe(
        &self,
        name: &str,
        version: Option<u64>,
        target: Option<&str>,
    ) -> Result<RecipeResolution, EnvironmentError> {
        let stored = self.stored_recipe(name, version).await?.value;
        let reference = RecipeRef {
            name: stored.name,
            version: stored.version,
            digest: stored.digest,
        };
        self.resolve_spec(Some(reference), &stored.spec, target)
            .await
    }

    /// What a spec will cause Compute to do: the recipe's problems, or the
    /// existing contracts it resolves to and whether placement can satisfy
    /// them now. Nothing is recorded, acquired, or started.
    pub async fn resolve_spec(
        &self,
        recipe: Option<RecipeRef>,
        spec: &RecipeSpec,
        target: Option<&str>,
    ) -> Result<RecipeResolution, EnvironmentError> {
        let resolved = match resolve(spec) {
            Ok(resolved) => resolved,
            Err(problems) => {
                return Ok(RecipeResolution {
                    recipe,
                    verdict: RecipeVerdict::Invalid,
                    problems,
                    resolved: None,
                    implied_capabilities: vec![],
                    lifecycle: vec![],
                    primitives: vec![],
                    placement: None,
                });
            }
        };
        if let Some(target) = target
            && self.pool.member(target).is_none()
        {
            return Err(EnvironmentError::Invalid(format!(
                "target {target} is not in the daemon's pool"
            )));
        }
        let computer = ComputerSpec {
            lifecycle: resolved.computer.lifecycle,
            requirements: resolved.computer.requirements.clone(),
            target: target.map(str::to_owned),
            ttl_seconds: resolved.computer.ttl_seconds,
            expires_at: None,
            generation: 1,
            destroy_requested_at: None,
        };
        // The environment the placement is evaluated for: transient, never
        // written. It carries what admission reads (the policy).
        let transient = compute_state::EnvironmentRecord {
            name: "recipe-resolution".into(),
            desired_state: compute_state::DesiredState::Running,
            config: Default::default(),
            policy: resolved
                .policy
                .clone()
                .map(compute_policy::Policy::canonical)
                .map(|policy| serde_json::to_value(policy).expect("policies serialize")),
            provider: None,
            created_at: Utc::now(),
            owner: None,
            computer: Some(computer.clone()),
            contents: None,
            configuration: None,
            recipe: None,
        };
        let (report, _) = self
            .evaluate_placement(&transient, &computer, target)
            .await?;
        let verdict = if report.outcome == PlacementOutcome::Placed {
            RecipeVerdict::Satisfiable
        } else {
            RecipeVerdict::Unsatisfied
        };
        let (lifecycle, primitives) = explain(spec);
        Ok(RecipeResolution {
            recipe,
            verdict,
            problems: vec![],
            implied_capabilities: implied_capabilities(spec.lifecycle),
            resolved: Some(resolved),
            lifecycle,
            primitives,
            placement: Some(report),
        })
    }

    /// Whether an environment's claim to have been made from a recipe is
    /// true: the version exists, its digest is the one named, and the
    /// computer and policy requested are what that version resolves to.
    /// The evidence recorded on an environment cannot describe a computer
    /// the recipe did not ask for.
    /// The recipe evidence a derived environment (fork, restore) or a
    /// replaced computer may keep: the reference itself while that recipe
    /// version still resolves to exactly the configuration now in force
    /// (only `target` may differ), otherwise none. Provenance is carried
    /// only while it is true; the creation event keeps the history.
    pub(crate) async fn surviving_recipe(
        &self,
        reference: Option<&RecipeRef>,
        computer: &crate::model::ComputerRequest,
        policy: Option<&compute_policy::Policy>,
    ) -> Option<RecipeRef> {
        let reference = reference?;
        self.verify_recipe_reference(reference, computer, policy)
            .await
            .is_ok()
            .then(|| reference.clone())
    }

    pub(crate) async fn verify_recipe_reference(
        &self,
        reference: &RecipeRef,
        computer: &crate::model::ComputerRequest,
        policy: Option<&compute_policy::Policy>,
    ) -> Result<(), EnvironmentError> {
        let stored = self
            .stored_recipe(&reference.name, Some(reference.version))
            .await?
            .value;
        if stored.digest != reference.digest {
            return Err(EnvironmentError::Conflict(format!(
                "recipe {} version {} has digest {}, not {}",
                reference.name, reference.version, stored.digest, reference.digest
            )));
        }
        let resolved = resolve(&stored.spec)
            .map_err(|problems| EnvironmentError::Invalid(problems.join("; ")))?;
        if !matches_resolution(&resolved, computer, policy) {
            return Err(EnvironmentError::Invalid(format!(
                "the computer requested is not what recipe {} version {} resolves to; \
                 only `target` may differ",
                reference.name, reference.version
            )));
        }
        Ok(())
    }
}
