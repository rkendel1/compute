//! Stack locks and the package/source inventory, against an in-memory world of
//! repositories and a registry. Each test changes one fact about that world
//! and checks that the lock reports it as exactly one kind of finding.

use std::collections::{BTreeMap, BTreeSet};

use compute_project::{
    Overrides, PublishedPackage, Stack, StackLock, StackResolver, TreeEntry, diff_locks,
    range_satisfies, resolve_stack, sha256_hex, upgrade_plan,
};

const LIB_OLD: &str = "1111111111111111111111111111111111111111";
const LIB_MAIN: &str = "2222222222222222222222222222222222222222";
const APP_MAIN: &str = "3333333333333333333333333333333333333333";

type Files = BTreeMap<String, Vec<u8>>;

#[derive(Clone, Default)]
struct World {
    /// repository → reference → commit
    refs: BTreeMap<String, BTreeMap<String, String>>,
    /// (repository, commit) → files
    commits: BTreeMap<(String, String), Files>,
    /// package → version → metadata
    registry: BTreeMap<String, BTreeMap<String, PublishedPackage>>,
}

impl World {
    fn files(&self, repository: &str, commit: &str) -> Result<&Files, String> {
        self.commits
            .get(&(repository.to_owned(), commit.to_owned()))
            .ok_or_else(|| format!("{repository} has no commit {commit}"))
    }

    fn app(&mut self) -> &mut Files {
        self.commits
            .get_mut(&("github:t/app".into(), APP_MAIN.into()))
            .unwrap()
    }

    fn set(&mut self, path: &str, contents: &str) {
        self.app().insert(path.into(), contents.as_bytes().to_vec());
    }
}

fn object(text: &str) -> String {
    sha256_hex(text.as_bytes())[7..47].to_owned()
}

impl StackResolver for World {
    fn resolve_reference(&self, repository: &str, reference: &str) -> Result<String, String> {
        if self
            .commits
            .contains_key(&(repository.to_owned(), reference.to_owned()))
        {
            return Ok(reference.to_owned());
        }
        self.refs
            .get(repository)
            .and_then(|refs| refs.get(reference))
            .cloned()
            .ok_or_else(|| format!("no {reference} in {repository}"))
    }
    fn tree(&self, repository: &str, commit: &str) -> Result<String, String> {
        let entries = self.list_tree(repository, commit)?;
        Ok(object(&format!("{entries:?}")))
    }
    fn read_file(
        &self,
        repository: &str,
        commit: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        Ok(self.files(repository, commit)?.get(path).cloned())
    }
    fn has_commit(&self, repository: &str, commit: &str) -> Result<bool, String> {
        Ok(self
            .commits
            .contains_key(&(repository.to_owned(), commit.to_owned())))
    }
    fn list_tree(&self, repository: &str, commit: &str) -> Result<Vec<TreeEntry>, String> {
        let files = self.files(repository, commit)?;
        let mut entries = vec![];
        let mut dirs = BTreeSet::new();
        for (path, bytes) in files {
            entries.push(TreeEntry {
                path: path.clone(),
                kind: "blob".into(),
                object: sha256_hex(bytes)[7..47].to_owned(),
            });
            let mut dir = path.as_str();
            while let Some((parent, _)) = dir.rsplit_once('/') {
                dirs.insert(parent.to_owned());
                dir = parent;
            }
        }
        for dir in dirs {
            // A tree's id is a function of exactly the files under it.
            let prefix = format!("{dir}/");
            let contents: Vec<(&str, String)> = files
                .iter()
                .filter_map(|(p, b)| p.strip_prefix(&prefix).map(|rel| (rel, sha256_hex(b))))
                .collect();
            entries.push(TreeEntry {
                path: dir,
                kind: "tree".into(),
                object: object(&format!("{contents:?}")),
            });
        }
        Ok(entries)
    }
    fn changed_paths(&self, repository: &str, from: &str, to: &str) -> Result<Vec<String>, String> {
        let (a, b) = (self.files(repository, from)?, self.files(repository, to)?);
        Ok(a.keys()
            .chain(b.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|p| a.get(*p) != b.get(*p))
            .cloned()
            .collect())
    }
    fn package_versions(&self, package: &str) -> Result<Vec<String>, String> {
        Ok(self
            .registry
            .get(package)
            .map(|v| v.keys().cloned().collect())
            .unwrap_or_default())
    }
    fn package(&self, package: &str, version: &str) -> Result<PublishedPackage, String> {
        self.registry
            .get(package)
            .and_then(|v| v.get(version))
            .cloned()
            .ok_or_else(|| format!("{package}@{version} is not published"))
    }
}

fn files(items: &[(&str, &str)]) -> Files {
    items
        .iter()
        .map(|(p, c)| ((*p).to_owned(), c.as_bytes().to_vec()))
        .collect()
}

const LIB_CARGO: &str = "[package]\nname = \"tlib\"\nversion = \"0.3.0\"\n";

fn lib(version: &str, source: &str) -> Files {
    files(&[
        (
            "packages/lib/package.json",
            &format!(r#"{{"name": "@t/lib", "version": "{version}"}}"#),
        ),
        ("crates/tlib/Cargo.toml", LIB_CARGO),
        ("crates/tlib/src/lib.rs", source),
        ("LICENSE", "MIT"),
    ])
}

fn npm_lock(entries: &[(&str, &str)]) -> String {
    let mut packages = serde_json::Map::new();
    packages.insert("".into(), serde_json::json!({"name": "app"}));
    for (key, version) in entries {
        packages.insert((*key).into(), serde_json::json!({"version": version, "resolved": format!("https://registry.npmjs.org/@t/lib/-/lib-{version}.tgz"), "integrity": format!("sha512-{version}")}));
    }
    serde_json::to_string_pretty(&serde_json::json!({"lockfileVersion": 3, "packages": packages}))
        .unwrap()
}

/// `lib` provides `@t/lib` (npm) and `tlib` (Cargo); `app` uses the published
/// package and vendors `tlib` from an older `lib` commit.
fn world() -> World {
    let mut world = World::default();
    world.refs.insert(
        "github:t/lib".into(),
        BTreeMap::from([("main".into(), LIB_MAIN.into())]),
    );
    world.refs.insert(
        "github:t/app".into(),
        BTreeMap::from([("main".into(), APP_MAIN.into())]),
    );
    world.commits.insert(
        ("github:t/lib".into(), LIB_OLD.into()),
        lib("1.1.0", "pub fn v() -> u32 { 1 }"),
    );
    world.commits.insert(
        ("github:t/lib".into(), LIB_MAIN.into()),
        lib("1.2.0", "pub fn v() -> u32 { 2 }"),
    );
    world.commits.insert(
        ("github:t/app".into(), APP_MAIN.into()),
        files(&[
            ("package.json", r#"{"name": "app", "dependencies": {"@t/lib": "^1.2.0"}}"#),
            ("package-lock.json", &npm_lock(&[("node_modules/@t/lib", "1.2.0")])),
            ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dependencies]\ntlib = { path = \"vendor/tlib\" }\n"),
            ("Cargo.lock", "version = 3\n[[package]]\nname = \"tlib\"\nversion = \"0.3.0\"\n"),
            ("vendor/tlib/Cargo.toml", LIB_CARGO),
            ("vendor/tlib/VENDORED.md", &format!("commit: {LIB_OLD}\n")),
            ("vendor/tlib/src/lib.rs", "pub fn v() -> u32 { 1 }"),
            ("vendor/tlib/LICENSE", "MIT"),
        ]),
    );
    for (version, head) in [("1.1.0", LIB_OLD), ("1.2.0", LIB_MAIN)] {
        world.registry.entry("@t/lib".into()).or_default().insert(
            version.into(),
            PublishedPackage {
                integrity: format!("sha512-{version}"),
                git_head: Some(head.into()),
                dependencies: BTreeMap::new(),
            },
        );
    }
    world
}

const MANIFEST: &str = r#"
schema = "compute.stack@1"
[stack]
name = "sync"
version = "1.0.0"
[[component]]
name = "lib"
kind = "package"
source = "npm:@t/lib"
version = "^1.0.0"

[[source]]
name = "lib"
repository = "github:t/lib"
track = "main"
provides = ["npm:@t/lib", "cargo:tlib"]

[[source]]
name = "app"
repository = "github:t/app"
track = "main"

[[relationship]]
name = "app-vendors-lib"
kind = "pin"
consumer = "app"
file = "vendor/tlib/VENDORED.md"
pattern = "commit: {commit}"
target = "lib"
policy = "fixed"
reason = "vendored deliberately; moved in its own reviewed change"

[[copy]]
name = "app-vendored-tlib"
package = "cargo:tlib"
consumer = "app"
path = "vendor/tlib"
origin = "app-vendors-lib"
map = ["src=crates/tlib/src", "LICENSE=LICENSE"]
local = ["Cargo.toml", "VENDORED.md"]
reason = "the manifest and the origin record are written for the copy"
"#;

fn stack(text: &str) -> Stack {
    Stack::parse(text).unwrap_or_else(|e| panic!("{}", e.message))
}

fn lock(text: &str, world: &World) -> StackLock {
    resolve_stack(&stack(text), &Overrides::default(), world)
        .unwrap_or_else(|e| panic!("{}", e.message))
}

/// `(kind/check, status)` for every finding, optionally of one consumer.
fn findings(lock: &StackLock) -> BTreeSet<(String, String)> {
    lock.inventory
        .findings
        .iter()
        .map(|f| (format!("{}/{}", f.kind, f.check), f.status.clone()))
        .collect()
}

fn has(lock: &StackLock, check: &str, status: &str) -> bool {
    findings(lock).contains(&(check.to_owned(), status.to_owned()))
}

#[test]
fn a_consistent_world_has_only_its_declared_divergence() {
    let lock = lock(MANIFEST, &world());
    assert_eq!(
        findings(&lock),
        BTreeSet::from([("source-drift/stale-copy".to_owned(), "accepted".to_owned())]),
        "{:#?}",
        lock.inventory.findings
    );
    let tlib = lock
        .inventory
        .packages
        .iter()
        .find(|p| p.package == "cargo:tlib")
        .unwrap();
    assert_eq!(tlib.canonical.path.as_deref(), Some("crates/tlib"));
    let copy = tlib.uses.iter().find(|u| u.kind == "copy").unwrap();
    assert_eq!(copy.matches.as_deref(), Some("canonical-origin"));
    assert_eq!(copy.origin, format!("copy:lib@{LIB_OLD}"));
    let npm = lock
        .inventory
        .packages
        .iter()
        .find(|p| p.package == "npm:@t/lib")
        .unwrap();
    assert_eq!(npm.stack_version.as_deref(), Some("1.2.0"));
    let resolved = npm.uses.iter().find(|u| u.kind == "resolved").unwrap();
    assert_eq!(
        (
            resolved.version.as_deref(),
            resolved.direct,
            resolved.digest.as_deref()
        ),
        (Some("1.2.0"), true, Some("sha512-1.2.0"))
    );
    assert!(lock.violated_findings().is_empty());
}

#[test]
fn resolution_is_deterministic_and_the_lock_is_canonical() {
    let world = world();
    let (a, b) = (lock(MANIFEST, &world), lock(MANIFEST, &world));
    assert_eq!(a.canonical_bytes(), b.canonical_bytes());
    assert_eq!(StackLock::parse(&a.canonical_bytes()).unwrap(), a);
    let mut edited = a.canonical_bytes();
    edited.extend_from_slice(b"\n");
    assert!(
        StackLock::parse(&edited).is_err(),
        "a hand-edited lock is refused"
    );
}

#[test]
fn version_drift_in_a_consumer_is_reported_with_its_compatibility() {
    let mut world = world();
    world.set(
        "package.json",
        r#"{"name": "app", "dependencies": {"@t/lib": "1.1.0"}}"#,
    );
    world.set(
        "package-lock.json",
        &npm_lock(&[("node_modules/@t/lib", "1.1.0")]),
    );
    let lock = lock(MANIFEST, &world);
    assert!(
        has(&lock, "version-drift/consumer-version", "open"),
        "{:#?}",
        lock.inventory.findings
    );
    assert!(has(
        &lock,
        "compatibility-drift/excludes-stack-version",
        "open"
    ));
    assert!(
        lock.violated_findings().is_empty(),
        "drift is not a violation"
    );
}

#[test]
fn a_lockfile_that_contradicts_its_manifest_is_a_violation() {
    let mut world = world();
    world.set(
        "package-lock.json",
        &npm_lock(&[("node_modules/@t/lib", "1.1.0")]),
    );
    let lock = lock(MANIFEST, &world);
    assert!(
        has(&lock, "resolution-drift/lock-mismatch", "violated"),
        "{:#?}",
        lock.inventory.findings
    );
    let mut world = self::world();
    world.set("package-lock.json", &npm_lock(&[]));
    let lock = self::lock(MANIFEST, &world);
    assert!(has(&lock, "resolution-drift/missing-from-lock", "violated"));
}

#[test]
fn duplicate_resolutions_are_reported() {
    let mut world = world();
    world.set(
        "package-lock.json",
        &npm_lock(&[
            ("node_modules/@t/lib", "1.2.0"),
            ("node_modules/other/node_modules/@t/lib", "1.1.0"),
        ]),
    );
    let lock = lock(MANIFEST, &world);
    assert!(
        has(&lock, "resolution-drift/duplicate", "open"),
        "{:#?}",
        lock.inventory.findings
    );
    let npm = lock
        .inventory
        .packages
        .iter()
        .find(|p| p.package == "npm:@t/lib")
        .unwrap();
    assert!(
        npm.uses
            .iter()
            .any(|u| u.version.as_deref() == Some("1.1.0") && !u.direct),
        "the nested copy is transitive"
    );
}

#[test]
fn a_stale_vendored_copy_is_drift_unless_its_pin_is_deliberately_fixed() {
    let lock = lock(
        &MANIFEST.replace("policy = \"fixed\"", "policy = \"locked\""),
        &world(),
    );
    assert!(
        has(&lock, "source-drift/stale-copy", "open"),
        "{:#?}",
        lock.inventory.findings
    );
    // Re-vendored from the locked commit: in sync.
    let mut world = world();
    world.set("vendor/tlib/VENDORED.md", &format!("commit: {LIB_MAIN}\n"));
    world.set("vendor/tlib/src/lib.rs", "pub fn v() -> u32 { 2 }");
    let lock = self::lock(MANIFEST, &world);
    assert!(findings(&lock).is_empty(), "{:#?}", lock.inventory.findings);
    let copy = lock
        .inventory
        .packages
        .iter()
        .flat_map(|p| &p.uses)
        .find(|u| u.kind == "copy")
        .unwrap();
    assert_eq!(copy.matches.as_deref(), Some("canonical-locked"));
}

#[test]
fn an_unexplained_modification_of_vendored_source_blocks_promotion() {
    let mut world = world();
    world.set(
        "vendor/tlib/src/lib.rs",
        "pub fn v() -> u32 { 1 } // patched",
    );
    let lock = lock(MANIFEST, &world);
    assert!(
        has(&lock, "source-drift/modified-copy", "violated"),
        "{:#?}",
        lock.inventory.findings
    );
    assert_eq!(lock.violated_findings().len(), 1);

    let mut world = self::world();
    world.set("vendor/tlib/src/extra.rs", "");
    world.set("vendor/tlib/NOTES.md", "local notes");
    let lock = self::lock(MANIFEST, &world);
    assert!(
        has(&lock, "source-drift/modified-copy", "violated"),
        "a file added under a mapped directory changes it"
    );
    assert!(
        has(&lock, "source-drift/unexplained-file", "violated"),
        "{:#?}",
        lock.inventory.findings
    );
}

#[test]
fn an_undeclared_copy_is_a_provenance_gap() {
    let mut world = world();
    world.set(
        "forks/lib/package.json",
        r#"{"name": "@t/lib", "version": "1.2.0-fork"}"#,
    );
    let lock = lock(MANIFEST, &world);
    assert!(
        has(&lock, "provenance-gap/undeclared-copy", "open"),
        "{:#?}",
        lock.inventory.findings
    );
    assert!(has(&lock, "source-drift/undeclared-copy-differs", "open"));
}

#[test]
fn a_declared_divergence_accepts_only_what_it_names_and_must_say_why() {
    let mut world = world();
    world.set(
        "forks/lib/package.json",
        r#"{"name": "@t/lib", "version": "1.2.0-fork"}"#,
    );
    let accepted = format!(
        "{MANIFEST}\n[[divergence]]\nname = \"app-forks-lib\"\npackage = \"npm:@t/*\"\nconsumer = \"app\"\npath = \"forks\"\nreason = \"the fork carries an unreleased fix\"\n"
    );
    let lock = lock(&accepted, &world);
    let forks: Vec<_> = lock
        .inventory
        .findings
        .iter()
        .filter(|f| f.path.starts_with("forks"))
        .collect();
    assert!(!forks.is_empty());
    for finding in forks {
        assert_eq!(finding.status, "accepted");
        assert_eq!(
            finding.accepted_by.as_deref(),
            Some("app-forks-lib: the fork carries an unreleased fix")
        );
    }
    let error = Stack::parse(&accepted.replace(
        "reason = \"the fork carries an unreleased fix\"",
        "reason = \"\"",
    ))
    .unwrap_err();
    assert!(error.message.contains("must say why"), "{}", error.message);
    let error =
        Stack::parse(&accepted.replace("path = \"forks\"", "check = \"everything\"")).unwrap_err();
    assert!(error.message.contains("check"), "{}", error.message);
}

#[test]
fn workspace_members_are_governed_by_their_workspace_lockfile() {
    let mut world = world();
    world.set("pnpm-workspace.yaml", "packages:\n  - \"packages/*\"\n");
    world.set(
        "packages/ui/package.json",
        r#"{"name": "ui", "dependencies": {"@t/lib": "1.2.0"}}"#,
    );
    let lock = lock(MANIFEST, &world);
    assert!(
        has(&lock, "provenance-gap/unlocked", "open"),
        "a pnpm workspace with no pnpm-lock.yaml: {:#?}",
        lock.inventory.findings
    );
    world.set(
        "pnpm-lock.yaml",
        "lockfileVersion: '9.0'\n\nimporters:\n\n  packages/ui:\n    dependencies:\n      '@t/lib':\n        specifier: 1.2.0\n        version: 1.2.0\n\npackages:\n\n  '@t/lib@1.2.0':\n    resolution: {integrity: sha512-1.2.0}\n",
    );
    let lock = self::lock(MANIFEST, &world);
    assert!(
        !has(&lock, "provenance-gap/unlocked", "open"),
        "{:#?}",
        lock.inventory.findings
    );
    let ui = lock
        .inventory
        .packages
        .iter()
        .flat_map(|p| &p.uses)
        .find(|u| u.path == "pnpm-lock.yaml#importers/packages/ui")
        .unwrap();
    assert_eq!(
        (ui.version.as_deref(), ui.digest.as_deref()),
        (Some("1.2.0"), Some("sha512-1.2.0"))
    );
}

#[test]
fn a_new_violation_shows_in_the_diff() {
    let stack = stack(&format!(
        "{MANIFEST}\n[[acceptance]]\nname = \"app-tests\"\ncovers = [\"app\"]\nmode = \"deterministic\"\n"
    ));
    let world_before = world();
    let before = resolve_stack(&stack, &Overrides::default(), &world_before).unwrap();
    let mut world_after = world();
    world_after.set("vendor/tlib/src/lib.rs", "patched");
    let after = resolve_stack(&stack, &Overrides::default(), &world_after).unwrap();
    let diff = diff_locks(&stack, &before, &after, Some(&world_after));
    assert!(
        diff.findings
            .iter()
            .any(|f| f.check == "modified-copy" && f.to.as_deref() == Some("violated")),
        "{:#?}",
        diff.findings
    );
}

#[test]
fn an_upgrade_plan_lists_every_consumer_and_what_is_outstanding() {
    let lock = lock(MANIFEST, &world());
    let stack = stack(MANIFEST);
    // `^1.2.0` admits 1.3.0: only the lockfile moves.
    let plan = upgrade_plan(&lock.inventory, &stack, "npm:@t/lib", "1.3.0").unwrap();
    assert_eq!((plan.at_target, plan.outstanding), (0, 1));
    let app = &plan.consumers[0];
    assert_eq!(app.status, "outstanding");
    assert_eq!(app.actions.len(), 1, "{:?}", app.actions);
    assert!(
        app.actions[0].contains("package-lock.json") && app.actions[0].contains("re-resolve"),
        "{:?}",
        app.actions
    );
    // A major version needs the declaration too.
    let plan = upgrade_plan(&lock.inventory, &stack, "npm:@t/lib", "2.0.0").unwrap();
    assert!(
        plan.consumers[0]
            .actions
            .iter()
            .any(|a| a.starts_with("package.json:") && a.contains("admit 2.0.0")),
        "{plan:#?}"
    );
    let plan = upgrade_plan(&lock.inventory, &stack, "npm:@t/lib", "1.2.0").unwrap();
    assert_eq!((plan.at_target, plan.outstanding), (1, 0), "{plan:#?}");
    // A commit moves the copy; its Cargo.lock path entry follows it and is not an action.
    let plan = upgrade_plan(&lock.inventory, &stack, "cargo:tlib", LIB_MAIN).unwrap();
    let app = &plan.consumers[0];
    // Held deliberately behind the fixed pin: accepted, not outstanding.
    assert_eq!(
        (app.status.as_str(), app.actions.len()),
        ("accepted", 1),
        "{plan:#?}"
    );
    assert_eq!((plan.accepted, plan.outstanding), (1, 0));
    let locked = self::stack(&MANIFEST.replace("policy = \"fixed\"", "policy = \"locked\""));
    let relocked = resolve_stack(&locked, &Overrides::default(), &world()).unwrap();
    let moved = upgrade_plan(&relocked.inventory, &locked, "cargo:tlib", LIB_MAIN).unwrap();
    assert_eq!(
        moved.consumers[0].status, "manual",
        "a copy with local files is moved by a person: {moved:#?}"
    );
    assert!(
        app.actions
            .iter()
            .any(|a| a.contains("re-copy") && a.contains("keep its local files")),
        "{:?}",
        app.actions
    );
}

#[test]
fn copies_must_name_a_real_origin() {
    let error =
        Stack::parse(&MANIFEST.replace("origin = \"app-vendors-lib\"", "origin = \"no-such-pin\""))
            .unwrap_err();
    assert!(error.message.contains("is not a pin"), "{}", error.message);
    let error = Stack::parse(&MANIFEST.replace(
        "reason = \"the manifest and the origin record are written for the copy\"\n",
        "",
    ))
    .unwrap_err();
    assert!(error.message.contains("must say why"), "{}", error.message);
}

#[test]
fn npm_ranges_are_understood() {
    for (range, version, expected) in [
        ("^1.2.0", "1.3.0", true),
        ("^0.11.0", "0.12.0", false),
        (">=1.0.0 <2.0.0", "1.9.9", true),
        ("1.x", "1.4.0", true),
        ("1.0.0 || ^2.0.0", "2.1.0", true),
        ("*", "0.0.1", true),
        ("~1.2.0", "1.3.0", false),
    ] {
        assert_eq!(
            range_satisfies(range, version),
            Some(expected),
            "{range} {version}"
        );
    }
    assert_eq!(range_satisfies("workspace:*", "1.0.0"), None);
}
