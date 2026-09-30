//! Recipes are policy, not another execution system.
//!
//! These tests run against a real target (`compute serve` hosting workspace
//! sessions) and prove that a recipe reaches Compute only as the ordinary
//! environment request; that validation tells an invalid recipe from an
//! unsatisfiable one; that any recipe a user writes resolves through the same
//! mechanism; and that versions are durable and identifiable from what they
//! produced.

mod common;
#[path = "common/target.rs"]
mod target;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use compute_core::{
    ComputerLifecycle, ComputerRequirements, ComputerStatus, IsolationProfile, NetworkPolicy,
    RecipeSpec, recipe_digest,
};
use compute_environment::*;
use compute_state::StateStore;
use compute_state_file::FileState;
use compute_state_memory::MemoryState;
use target::*;

fn write(name: &str, spec: RecipeSpec) -> RecipeDefinition {
    RecipeDefinition {
        name: name.into(),
        spec,
        expected_version: None,
    }
}

fn small() -> ComputerRequirements {
    ComputerRequirements {
        cpu_count: Some(1),
        memory_bytes: Some(64 << 20),
        ..Default::default()
    }
}

fn ephemeral(ttl: u64) -> RecipeSpec {
    RecipeSpec {
        lifecycle: ComputerLifecycle::Ephemeral,
        ttl_seconds: Some(ttl),
        requirements: small(),
        ..Default::default()
    }
}

fn on_disk(path: &Path) -> Arc<dyn StateStore> {
    Arc::new(FileState::open(path).unwrap())
}

async fn running(daemon: &Arc<Daemon>, name: &str) -> ComputerView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(view) = daemon.computer(name).await
            && view.status == ComputerStatus::Running
        {
            return view;
        }
        assert!(tokio::time::Instant::now() < deadline, "{name} never ran");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The request `environment create` sends for a recipe: what it resolves to,
/// with the version's identity as evidence.
fn request(name: &str, resolution: &RecipeResolution) -> ComputerEnvironmentDefinition {
    let resolved = resolution.resolved.clone().expect("it resolved");
    ComputerEnvironmentDefinition {
        name: name.into(),
        desired_state: DesiredState::Running,
        env: Default::default(),
        policy: resolved.policy,
        computer: resolved.computer,
        contents: Default::default(),
        recipe: resolution.recipe.clone(),
    }
}

// ---- validation: three different answers ----------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn validation_tells_invalid_from_unsatisfied_from_satisfiable() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;

    // Valid, and a target can host it.
    daemon
        .write_recipe("alice", write("fits", ephemeral(600)))
        .await
        .unwrap();
    let fits = daemon.resolve_recipe("fits", None, None).await.unwrap();
    assert_eq!(fits.verdict, RecipeVerdict::Satisfiable);
    assert!(fits.problems.is_empty());
    assert!(fits.placement.as_ref().unwrap().selected.is_some());

    // Valid, and nothing satisfies it: the recipe is fine, the pool is not.
    let mut gpu = ephemeral(600);
    gpu.requirements.features = vec!["gpu".into()];
    daemon
        .write_recipe("alice", write("needs-gpu", gpu))
        .await
        .unwrap();
    let unsatisfied = daemon
        .resolve_recipe("needs-gpu", None, None)
        .await
        .unwrap();
    assert_eq!(unsatisfied.verdict, RecipeVerdict::Unsatisfied);
    assert!(
        unsatisfied.problems.is_empty(),
        "a valid recipe has no problems"
    );
    assert!(unsatisfied.resolved.is_some(), "it still resolves");
    let placement = unsatisfied.placement.unwrap();
    assert!(placement.selected.is_none());
    assert!(
        placement
            .providers
            .iter()
            .any(|provider| !provider.reasons.is_empty()),
        "placement says why no target fits"
    );

    // Invalid: refused when written, and reported with every problem when a
    // draft is resolved.
    let broken = RecipeSpec {
        ttl_seconds: Some(60), // persistent with a TTL: an unsupported combination
        requirements: ComputerRequirements {
            features: vec!["quantum".into()],
            ..Default::default()
        },
        policy: Some(serde_json::json!({ "nonsense": true })),
        ..Default::default()
    };
    let refusal = daemon
        .write_recipe("alice", write("broken", broken.clone()))
        .await
        .unwrap_err();
    assert_eq!(refusal.kind(), "invalid");
    let draft = daemon.resolve_spec(None, &broken, None).await.unwrap();
    assert_eq!(draft.verdict, RecipeVerdict::Invalid);
    assert!(draft.problems.len() >= 3, "{:?}", draft.problems);
    assert!(draft.resolved.is_none() && draft.placement.is_none());
    assert!(
        matches!(
            daemon.recipe("broken", None).await,
            Err(EnvironmentError::NotFound(_))
        ),
        "an invalid recipe is not stored"
    );
    daemon.shutdown().await;
}

// ---- resolution: existing primitives, nothing else ------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolution_is_the_existing_request_and_names_no_provider() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    let spec = RecipeSpec {
        description: Some("no network, sandboxed".into()),
        lifecycle: ComputerLifecycle::Ephemeral,
        ttl_seconds: Some(900),
        requirements: ComputerRequirements {
            network: NetworkPolicy::None,
            isolation: IsolationProfile::Sandboxed,
            ..small()
        },
        ..Default::default()
    };
    daemon
        .write_recipe("alice", write("locked-down", spec.clone()))
        .await
        .unwrap();
    let resolution = daemon
        .resolve_recipe("locked-down", None, None)
        .await
        .unwrap();
    let resolved = resolution.resolved.unwrap();

    // Lifecycle, persistence (TTL), isolation, and networking land in the
    // fields Compute already has, unchanged.
    let computer: ComputerRequest = resolved.computer;
    assert_eq!(computer.lifecycle, ComputerLifecycle::Ephemeral);
    assert_eq!(computer.ttl_seconds, Some(900));
    assert_eq!(computer.requirements, spec.requirements);
    assert_eq!(computer.requirements.isolation, IsolationProfile::Sandboxed);
    assert_eq!(computer.requirements.network, NetworkPolicy::None);
    // The recipe names no provider or target; placement chose among them.
    assert_eq!(computer.target, None);
    let placement = resolution.placement.unwrap();
    // Isolation reaches placement as the requirement placement evaluates
    // every target against; whether a host can enforce it is the host's
    // answer (`isolation_unsupported`), not the recipe's.
    assert_eq!(
        placement.requirements.isolation,
        IsolationProfile::Sandboxed
    );
    assert!(
        placement
            .providers
            .iter()
            .any(|provider| provider.provider_id == "target-a")
    );

    // A persistent recipe resolves to a persistent computer with no TTL, and
    // says what the lifecycle itself asks of the target and what it does not
    // promise.
    daemon
        .write_recipe(
            "alice",
            write(
                "keeper",
                RecipeSpec {
                    requirements: small(),
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    let keeper = daemon.resolve_recipe("keeper", None, None).await.unwrap();
    let persistent = keeper.resolved.unwrap().computer;
    assert_eq!(persistent.lifecycle, ComputerLifecycle::Persistent);
    assert_eq!(persistent.ttl_seconds, None);
    assert_eq!(keeper.implied_capabilities, ["claim"]);
    assert!(
        keeper
            .lifecycle
            .iter()
            .any(|line| line.contains("persistent_storage"))
    );

    // Placement can be constrained by the caller without touching the
    // recipe: a target the pool lacks is a caller error, not a recipe one.
    assert!(
        daemon
            .resolve_recipe("keeper", None, Some("target-a"))
            .await
            .is_ok()
    );
    assert_eq!(
        daemon
            .resolve_recipe("keeper", None, Some("nowhere"))
            .await
            .unwrap_err()
            .kind(),
        "invalid"
    );
    daemon.shutdown().await;
}

/// Resolving is read only: it records no environment, computer, or event
/// beyond the recipe's own write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolving_acquires_nothing() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .write_recipe("alice", write("plan", ephemeral(600)))
        .await
        .unwrap();
    let before = daemon.events(EventFilter::default()).await.unwrap().len();
    for _ in 0..3 {
        daemon.resolve_recipe("plan", None, None).await.unwrap();
    }
    assert!(daemon.environments().await.unwrap().is_empty());
    assert_eq!(
        daemon.events(EventFilter::default()).await.unwrap().len(),
        before
    );
    daemon.shutdown().await;
}

// ---- user-defined recipes: the same mechanism for every name ---------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn any_recipe_a_user_writes_resolves_through_the_same_mechanism() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    let mut gpu = ephemeral(4 * 3600);
    gpu.requirements.features = vec!["gpu".into()];
    let recipes = [
        ("customer-demo", ephemeral(4 * 3600)),
        ("nightly-data", ephemeral(6 * 3600)),
        (
            "review-environment",
            RecipeSpec {
                requirements: ComputerRequirements {
                    isolation: IsolationProfile::Process,
                    ..small()
                },
                lifecycle: ComputerLifecycle::Ephemeral,
                ..Default::default()
            },
        ),
        ("gpu-training", gpu),
        (
            "migration-window",
            RecipeSpec {
                requirements: small(),
                ..Default::default()
            },
        ),
    ];
    for (name, spec) in &recipes {
        daemon
            .write_recipe("alice", write(name, spec.clone()))
            .await
            .unwrap();
    }
    let listed = daemon.recipes().await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|recipe| recipe.name.as_str())
            .collect::<Vec<_>>(),
        [
            "customer-demo",
            "gpu-training",
            "migration-window",
            "nightly-data",
            "review-environment"
        ]
    );
    for (name, spec) in &recipes {
        let resolution = daemon.resolve_recipe(name, None, None).await.unwrap();
        let computer = resolution.resolved.expect(name).computer;
        assert_eq!(computer.requirements, spec.requirements, "{name}");
        assert_eq!(computer.lifecycle, spec.lifecycle, "{name}");
        let expected = if *name == "gpu-training" {
            RecipeVerdict::Unsatisfied
        } else {
            RecipeVerdict::Satisfiable
        };
        assert_eq!(resolution.verdict, expected, "{name}");
    }
    // A recipe cannot smuggle in anything Compute has no primitive for: a
    // provider, a target, contents, or source.
    for field in [
        "provider",
        "target",
        "contents",
        "repositories",
        "source",
        "command",
    ] {
        let parsed = serde_json::from_value::<RecipeSpec>(serde_json::json!({ field: "x" }));
        assert!(parsed.is_err(), "a recipe accepted `{field}`");
    }
    daemon.shutdown().await;
}

/// The shipped catalog uses ordinary specs: they validate and resolve through
/// the same pure resolver, and taking a copy cannot mutate the templates.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_starter_recipes_are_ordinary_documents() {
    let starters = compute_environment::recipe_starters();
    for starter in &starters {
        let problems = starter.spec.problems();
        assert!(problems.is_empty(), "{}: {problems:?}", starter.id);
        assert!(compute_environment::resolve(&starter.spec).is_ok());
    }
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
    let mut copy = compute_environment::recipe_starter("dev").unwrap();
    copy.spec.description = Some("user changed this copy".into());
    assert_ne!(
        copy.spec,
        compute_environment::recipe_starter("dev").unwrap().spec
    );
}

// ---- reaching the existing machinery ---------------------------------------

/// Recipe → the ordinary create → the computer → release: the same calls
/// every other caller makes, ending at the same records.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environment_is_made_from_a_recipe_by_the_ordinary_create() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    let written = daemon
        .write_recipe("alice", write("job", ephemeral(1200)))
        .await
        .unwrap();
    let resolution = daemon.resolve_recipe("job", None, None).await.unwrap();

    let view = daemon
        .create_computer_environment(request("job-1", &resolution), "alice")
        .await
        .unwrap();
    assert_eq!(
        view.recipe
            .as_ref()
            .map(|recipe| (recipe.name.as_str(), recipe.version)),
        Some(("job", 1))
    );
    let computer = running(&daemon, "job-1").await;
    // It is a computer like any other: the lifecycle the recipe asked for,
    // placed on the target, with the TTL the existing controller enforces.
    assert_eq!(computer.lifecycle, ComputerLifecycle::Ephemeral);
    assert_eq!(computer.target.as_deref(), Some("target-a"));
    assert_eq!(computer.requirements, small());
    assert!(computer.placement_id.is_some());

    // The event that made it names the version, never the spec.
    let events = daemon.events(EventFilter::default()).await.unwrap();
    let created = events
        .iter()
        .find(|event| event.kind == "environment.created")
        .expect("environment.created");
    assert_eq!(created.data["recipe"]["digest"], written.digest);
    assert!(created.data.get("spec").is_none());

    // Editing the recipe does not reach back: what produced the environment
    // is the version it recorded.
    let mut edited = ephemeral(1200);
    edited.requirements.cpu_count = Some(2);
    daemon
        .write_recipe(
            "alice",
            RecipeDefinition {
                name: "job".into(),
                spec: edited,
                expected_version: Some(1),
            },
        )
        .await
        .unwrap();
    let after = daemon.environment("job-1").await.unwrap();
    assert_eq!(after.recipe.as_ref().unwrap().version, 1);
    assert_eq!(after.recipe.unwrap().digest, written.digest);
    assert_eq!(
        daemon.recipe("job", Some(1)).await.unwrap().spec,
        ephemeral(1200)
    );

    // Release is the existing destroy: no recipe cleanup exists.
    daemon.destroy_computer("job-1", "alice").await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while daemon.computer("job-1").await.unwrap().status != ComputerStatus::Destroyed {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the computer was never destroyed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon.shutdown().await;
}

/// The recipe named on a request is evidence, and evidence cannot lie: a
/// request that is not what the version resolves to is refused, and only the
/// caller's placement constraint may differ.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recipe_reference_must_be_what_the_request_resolves_to() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    daemon
        .write_recipe("alice", write("job", ephemeral(1200)))
        .await
        .unwrap();
    let resolution = daemon.resolve_recipe("job", None, None).await.unwrap();

    let mut altered = request("lying", &resolution);
    altered.computer.requirements.cpu_count = Some(1);
    altered.computer.ttl_seconds = Some(99999);
    assert_eq!(
        daemon
            .create_computer_environment(altered, "alice")
            .await
            .unwrap_err()
            .kind(),
        "invalid"
    );

    let mut wrong_digest = request("stale", &resolution);
    wrong_digest.recipe.as_mut().unwrap().digest = "sha256:other".into();
    assert_eq!(
        daemon
            .create_computer_environment(wrong_digest, "alice")
            .await
            .unwrap_err()
            .kind(),
        "conflict"
    );

    let mut missing = request("ghost", &resolution);
    missing.recipe.as_mut().unwrap().version = 9;
    assert_eq!(
        daemon
            .create_computer_environment(missing, "alice")
            .await
            .unwrap_err()
            .kind(),
        "not_found"
    );

    let mut pinned = request("pinned", &resolution);
    pinned.computer.target = Some("target-a".into());
    daemon
        .create_computer_environment(pinned, "alice")
        .await
        .unwrap();
    daemon.shutdown().await;
}

/// An unsatisfiable recipe fails as a placement failure at create, before
/// anything is recorded: distinct from an invalid recipe, and from a
/// workload failing after it ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unsatisfied_recipe_creates_nothing() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    let mut gpu = ephemeral(600);
    gpu.requirements.features = vec!["gpu".into()];
    daemon
        .write_recipe("alice", write("needs-gpu", gpu))
        .await
        .unwrap();
    let resolution = daemon
        .resolve_recipe("needs-gpu", None, None)
        .await
        .unwrap();
    let error = daemon
        .create_computer_environment(request("nope", &resolution), "alice")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no target can host this computer"),
        "{error}"
    );
    assert!(daemon.environments().await.unwrap().is_empty());
    daemon.shutdown().await;
}

// ---- versions and durability -----------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn versions_are_immutable_and_edits_are_fenced() {
    let target = Target::start();
    let (daemon, _node) = start_daemon(Arc::new(MemoryState::new()), Some(pool(&target))).await;
    let v1 = daemon
        .write_recipe("alice", write("policy", ephemeral(600)))
        .await
        .unwrap();
    assert_eq!((v1.version, v1.status.as_str()), (1, "current"));
    assert_eq!(v1.digest, recipe_digest(&ephemeral(600)));

    // Creating what exists, or editing without the version read, is refused.
    assert_eq!(
        daemon
            .write_recipe("alice", write("policy", ephemeral(700)))
            .await
            .unwrap_err()
            .kind(),
        "conflict"
    );
    let edit = |expected, spec| RecipeDefinition {
        name: "policy".into(),
        spec,
        expected_version: Some(expected),
    };
    assert_eq!(
        daemon
            .write_recipe("bob", edit(7, ephemeral(700)))
            .await
            .unwrap_err()
            .kind(),
        "conflict"
    );
    // An edit that changes nothing is not a new version.
    assert_eq!(
        daemon
            .write_recipe("bob", edit(1, ephemeral(600)))
            .await
            .unwrap()
            .version,
        1
    );

    let v2 = daemon
        .write_recipe("bob", edit(1, ephemeral(700)))
        .await
        .unwrap();
    assert_eq!((v2.version, v2.author.as_str()), (2, "bob"));
    // The version an editor read is now stale.
    assert_eq!(
        daemon
            .write_recipe("alice", edit(1, ephemeral(800)))
            .await
            .unwrap_err()
            .kind(),
        "conflict"
    );
    // Version 1 is intact, superseded; the current one is 2; only it lists.
    let old = daemon.recipe("policy", Some(1)).await.unwrap();
    assert_eq!(
        (old.status.as_str(), old.spec),
        ("superseded", ephemeral(600))
    );
    assert_eq!(daemon.recipe("policy", None).await.unwrap().version, 2);
    let listed = daemon.recipes().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].version, 2);
    daemon.shutdown().await;
}

/// Recipes and the versions environments recorded survive a restart of the
/// controller: control state is the only authority.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recipes_and_their_evidence_survive_a_restart() {
    let target = Target::start();
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("control-state.json");
    let digest = {
        let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
        let v1 = daemon
            .write_recipe("alice", write("durable", ephemeral(1200)))
            .await
            .unwrap();
        daemon
            .write_recipe(
                "alice",
                RecipeDefinition {
                    name: "durable".into(),
                    spec: ephemeral(2400),
                    expected_version: Some(1),
                },
            )
            .await
            .unwrap();
        let resolution = daemon
            .resolve_recipe("durable", Some(1), None)
            .await
            .unwrap();
        daemon
            .create_computer_environment(request("from-v1", &resolution), "alice")
            .await
            .unwrap();
        daemon.shutdown().await;
        v1.digest
    };
    let (daemon, _node) = start_daemon(on_disk(&path), Some(pool(&target))).await;
    assert_eq!(daemon.recipe("durable", None).await.unwrap().version, 2);
    let v1 = daemon.recipe("durable", Some(1)).await.unwrap();
    assert_eq!(
        (v1.digest.as_str(), v1.spec),
        (digest.as_str(), ephemeral(1200))
    );
    let environment = daemon.environment("from-v1").await.unwrap();
    let recipe = environment
        .recipe
        .expect("the environment kept its evidence");
    assert_eq!(
        (recipe.name.as_str(), recipe.version, recipe.digest),
        ("durable", 1, digest)
    );
    // Resolving a past version yields what the environment was made from,
    // not what the recipe says now.
    let past = daemon
        .resolve_recipe("durable", Some(1), None)
        .await
        .unwrap();
    assert_eq!(past.resolved.unwrap().computer.ttl_seconds, Some(1200));
    daemon.shutdown().await;
}

// ---- the architectural invariant --------------------------------------------

/// Recipes express lifecycle policy but do not implement execution. All
/// Recipe execution must resolve into existing Compute Computer, Configured
/// Environment, workload, process, provider, persistence, isolation, and
/// lifecycle primitives.
///
/// Mechanically: the recipe modules cannot start processes, spawn tasks,
/// talk to a target or provider, or drive a computer; a recipe resolves to
/// the existing `ComputerRequest`; and the invariant is stated in the docs.
#[test]
fn recipes_express_policy_and_implement_no_execution() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let modules = [
        "crates/compute-core/src/recipes.rs",
        "crates/compute-environment/src/recipe.rs",
        "crates/compute-environment/src/daemon/recipes.rs",
        "crates/compute-cli/src/recipe_cmd.rs",
    ];
    // What execution looks like in this codebase. A recipe module that
    // mentions any of it is doing more than declaring policy.
    let execution = [
        "std::process",
        "tokio::process",
        "tokio::spawn",
        "std::thread",
        "RemoteProvider",
        "LocalProvider",
        "SessionProvider",
        "target_client",
        "computer_exec",
        "ComputerDriver",
        ".provision(",
        "destroy_computer(",
        "reconcile(",
        "supervisor",
    ];
    for module in modules {
        let source = std::fs::read_to_string(root.join(module)).unwrap();
        // The CLI exits with a status for `validate`: that is reporting, not
        // execution.
        let source = source.replace("std::process::exit", "exit");
        for word in execution {
            assert!(
                !source.contains(word),
                "{module} mentions `{word}`: a recipe must resolve into existing primitives, not execute"
            );
        }
    }

    // What a recipe resolves to is the existing request, by type.
    let _: fn(&ResolvedRecipe) -> &ComputerRequest = |resolved| &resolved.computer;
    let _: fn(&RecipeSpec) -> &ComputerRequirements = |spec| &spec.requirements;

    // No recipe subsystem: the only collection is the policy itself, and no
    // recipe type appears among the execution or lifecycle records.
    let collections = compute_state::Collection::ALL
        .iter()
        .map(|collection| collection.name())
        .filter(|name| name.to_lowercase().contains("recipe"))
        .collect::<Vec<_>>();
    assert_eq!(collections, ["Recipe"]);

    // The invariant is written where people read the architecture.
    for doc in ["docs/architecture.md", "docs/recipes.md"] {
        let text = std::fs::read_to_string(root.join(doc)).unwrap();
        assert!(
            text.contains("Recipes express lifecycle policy but do not implement execution."),
            "{doc} does not state the recipe invariant"
        );
    }
}
