//! The package and source inventory of a locked stack.
//!
//! A stack lock pins each source at a commit. That says nothing about whether
//! the sources agree with each other: a consumer can resolve another FeltDB
//! than the stack runs, vendor an old copy of it, patch that copy, or resolve
//! two versions at once. The inventory reads every package manifest and
//! lockfile (npm, pnpm, Yarn, Cargo), every tracked `node_modules` entry, and
//! every copy of a provided package, *at the locked commits*, and records for
//! each package the stack's sources provide:
//!
//! - its canonical location (source, directory, version, tree);
//! - every use: declared specifiers, resolved versions with their integrity,
//!   installed and copied source with its tree, compared with the canonical;
//! - findings, each of one kind: version drift, source drift, resolution drift,
//!   compatibility drift, a provenance gap, and whether a declared divergence
//!   accepts it (with its reason).
//!
//! It is derived from git objects only (no checkout, no package manager), so it
//! is as deterministic as the lock. It sees what is committed: an artifact a
//! build downloads at run time, or a dependency resolved outside a lockfile, is
//! not in it.

use std::collections::{BTreeMap, BTreeSet};

use compute_core::version_satisfies;
use serde::{Deserialize, Serialize};

use crate::error::{FailureKind, ProjectError};
use crate::stack::{CopyOrigin, Relationship, SourceCopy, Stack, StackSource};
use crate::stack_lock::{LockedSource, StackResolver, extract_pin, short};

/// One entry of a commit's tree (`git ls-tree -r -t`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: String,
    /// `blob`, `tree`, or `commit` (a submodule).
    pub kind: String,
    pub object: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub packages: Vec<InventoryPackage>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryPackage {
    /// `npm:<package>` or `cargo:<crate>`.
    pub package: String,
    pub canonical: Canonical,
    /// The published version the stack locks, when it is a stack component.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_version: Option<String>,
    /// Every use, in every source, at its locked commit.
    pub uses: Vec<PackageUse>,
    /// Consumers the canonical source expects (`expectations`) that are not
    /// sources of this stack: nothing about them was inspected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unlocated: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Canonical {
    pub source: String,
    pub commit: String,
    /// The package's directory; absent when the source has no manifest naming it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageUse {
    pub consumer: String,
    /// The manifest, lockfile entry, or copy directory.
    pub path: String,
    /// `declared`, `resolved`, `installed` (a tracked `node_modules` entry), or `copy`.
    pub kind: String,
    /// `production`, `development`, `peer`, `optional`, or `build`.
    pub scope: String,
    /// `package`, `test`, or `example`, from where the manifest lives.
    pub context: String,
    pub direct: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `registry`, `workspace`, `link`, `path:<path>`, `git:<url>#<rev>`, or `copy`.
    pub origin: String,
    /// Registry integrity or checksum, or a copy's git tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// For a copy: `canonical-locked`, `canonical-origin`, `modified`, or `differs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    /// One of `stack::FINDING_KINDS`.
    pub kind: String,
    /// The specific check (`duplicate`, `stale-copy`, `modified-copy`, …).
    pub check: String,
    pub package: String,
    pub consumer: String,
    pub path: String,
    /// `open` (drift to resolve), `accepted` (a declared divergence), or
    /// `violated` (blocks promotion).
    pub status: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_by: Option<String>,
}

impl Finding {
    pub fn key(&self) -> (String, String, String, String, String) {
        (
            self.kind.clone(),
            self.check.clone(),
            self.package.clone(),
            self.consumer.clone(),
            self.path.clone(),
        )
    }
}

fn error(message: impl Into<String>) -> ProjectError {
    ProjectError::new(FailureKind::StackInvalid, message)
}

/// An npm-style range: `||` alternatives of space-separated comparators, with
/// `*`, `x`, and `latest` admitting anything. `None` when it is not a range.
pub fn range_satisfies(range: &str, version: &str) -> Option<bool> {
    let mut any = false;
    for alternative in range.split("||") {
        let mut all = true;
        let comparators: Vec<&str> = alternative.split_whitespace().collect();
        let mut index = 0;
        while index < comparators.len() {
            let mut comparator = comparators[index].to_owned();
            // `>= 1.2.3` written with a space.
            if ["<", "<=", ">", ">=", "=", "^", "~"].contains(&comparator.as_str()) {
                index += 1;
                comparator.push_str(comparators.get(index)?);
            }
            index += 1;
            let comparator = comparator.trim_start_matches('v');
            if ["*", "x", "X", "latest"].contains(&comparator) {
                continue;
            }
            let normalized = match comparator.split('.').collect::<Vec<_>>().as_slice() {
                [major, "x" | "X" | "*"] | [major, "x" | "X" | "*", _] => format!("^{major}"),
                [major, minor, "x" | "X" | "*"] => format!("~{major}.{minor}"),
                _ => comparator.to_owned(),
            };
            all &= version_satisfies(&normalized, version)?;
        }
        any |= all;
    }
    Some(any)
}

/// A Cargo requirement: a bare version means `^`.
fn cargo_satisfies(requirement: &str, version: &str) -> Option<bool> {
    let mut all = true;
    for part in requirement.split(',') {
        let part = part.trim();
        let part = if part.starts_with(|c: char| c.is_ascii_digit()) {
            format!("^{part}")
        } else {
            part.to_owned()
        };
        all &= range_satisfies(&part, version)?;
    }
    Some(all)
}

fn context(path: &str) -> &'static str {
    let parts: Vec<&str> = path.split('/').collect();
    let dirs = &parts[..parts.len().saturating_sub(1)];
    if dirs.iter().any(|p| {
        [
            "test",
            "tests",
            "__tests__",
            "fixture",
            "fixtures",
            "testdata",
            "e2e",
        ]
        .contains(p)
    }) {
        "test"
    } else if dirs.iter().any(|p| {
        [
            "examples",
            "example",
            "experiments",
            "benchmarks",
            "docs",
            "samples",
            "demo",
        ]
        .contains(p)
    }) {
        "example"
    } else {
        "package"
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// `dir/../x` → `x`; `None` when the path leaves the repository.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = vec![];
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn in_node_modules(path: &str) -> bool {
    path.split('/').any(|part| part == "node_modules")
}

/// The package a `node_modules/…` key names, if it is a package directory.
fn node_modules_package(key: &str) -> Option<(String, bool)> {
    let index = key.rfind("node_modules/")?;
    let name = &key[index + "node_modules/".len()..];
    let parts: Vec<&str> = name.split('/').collect();
    let valid = match parts.as_slice() {
        [scope, _] => scope.starts_with('@'),
        [one] => !one.starts_with('@'),
        _ => false,
    };
    // Direct: the only `node_modules` in the key is the importer's own.
    let direct = key[..index].split('/').all(|p| p != "node_modules");
    valid.then(|| (name.to_owned(), direct))
}

fn json_str(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

const NPM_SECTIONS: &[(&str, &str)] = &[
    ("dependencies", "production"),
    ("devDependencies", "development"),
    ("peerDependencies", "peer"),
    ("optionalDependencies", "optional"),
];

fn npm_origin(specifier: &str) -> String {
    if specifier.starts_with("workspace:") {
        "workspace".into()
    } else if let Some(path) = specifier
        .strip_prefix("file:")
        .or_else(|| specifier.strip_prefix("link:"))
    {
        format!("path:{path}")
    } else if specifier.starts_with("git")
        || specifier.starts_with("github:")
        || specifier.contains("://")
        || (specifier.contains('/') && !specifier.starts_with('@') && !specifier.contains(' '))
    {
        format!("git:{specifier}")
    } else {
        "registry".into()
    }
}

/// The repository a git URL or shorthand names, as a `github:` identity.
fn git_identity(url: &str) -> Option<String> {
    let url = url.trim_start_matches("git+");
    let rest = url
        .strip_prefix("github:")
        .or_else(|| url.split_once("github.com/").map(|(_, r)| r))
        .or_else(|| url.split_once("github.com:").map(|(_, r)| r))?;
    let rest = rest.split(['#', '?']).next()?;
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?.trim_end_matches(".git");
    Some(format!("github:{owner}/{repo}").to_ascii_lowercase())
}

fn git_rev(url: &str) -> Option<String> {
    let rev = url
        .split_once('#')
        .map(|(_, r)| r)
        .or_else(|| url.split_once("rev=").map(|(_, r)| r))?;
    let rev = rev.split(['&', '#']).next()?;
    (!rev.is_empty()).then(|| rev.to_owned())
}

/// The few parts of a pnpm lockfile the inventory needs.
#[derive(Default)]
struct PnpmLock {
    /// importer → (package, specifier, version, section).
    importers: Vec<(String, String, String, String, String)>,
    /// package → (version, integrity) from `packages:`.
    packages: Vec<(String, String, Option<String>)>,
}

fn unquote(text: &str) -> &str {
    text.trim().trim_matches('\'').trim_matches('"')
}

/// `@scope/name@1.2.3(peer@1)`, `/@scope/name@1.2.3`, or `/@scope/name/1.2.3` → name, version.
fn pnpm_key(key: &str) -> Option<(String, String)> {
    let key = key.trim_start_matches('/');
    let key = key.split('(').next()?;
    let split = key[1..].find('@').map(|i| i + 1);
    match split {
        Some(at) => Some((key[..at].to_owned(), key[at + 1..].to_owned())),
        None => {
            let (name, version) = key.rsplit_once('/')?;
            Some((name.to_owned(), version.to_owned()))
        }
    }
}

fn parse_pnpm(text: &str) -> PnpmLock {
    let mut lock = PnpmLock::default();
    let mut section = "";
    let (mut importer, mut deps, mut name) = (String::new(), String::new(), String::new());
    let mut specifier = String::new();
    let mut package: Option<(String, String)> = None;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let content = line.trim();
        if indent == 0 {
            section = content.trim_end_matches(':');
            continue;
        }
        match (section, indent) {
            ("importers", 2) => importer = unquote(content.trim_end_matches(':')).to_owned(),
            ("importers", 4) => deps = content.trim_end_matches(':').to_owned(),
            ("importers", 6) => name = unquote(content.trim_end_matches(':')).to_owned(),
            ("importers", 8) => {
                if let Some(value) = content.strip_prefix("specifier:") {
                    specifier = unquote(value).to_owned();
                } else if let Some(value) = content.strip_prefix("version:") {
                    let version = unquote(value).split('(').next().unwrap_or("").to_owned();
                    lock.importers.push((
                        importer.clone(),
                        name.clone(),
                        specifier.clone(),
                        version,
                        deps.clone(),
                    ));
                }
            }
            ("packages", 2) => {
                package = pnpm_key(unquote(content.trim_end_matches(':')));
                if let Some((name, version)) = &package {
                    lock.packages.push((name.clone(), version.clone(), None));
                }
            }
            ("packages", 4) if package.is_some() => {
                if let Some(rest) = content.strip_prefix("resolution:")
                    && let Some(integrity) = rest.split_once("integrity:").map(|(_, i)| {
                        unquote(i.trim_end_matches('}').split(',').next().unwrap_or(""))
                    })
                    && let Some(last) = lock.packages.last_mut()
                {
                    last.2 = Some(integrity.to_owned());
                }
            }
            _ => {}
        }
    }
    lock
}

/// Yarn (classic and berry) entries: (package, specifiers, version, integrity).
fn parse_yarn(text: &str) -> Vec<(String, Vec<String>, String, Option<String>)> {
    let mut entries = vec![];
    let mut current: Option<(String, Vec<String>, String, Option<String>)> = None;
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            let header = line.trim_end_matches(':');
            let mut name = String::new();
            let mut specifiers = vec![];
            for item in header.split(", ") {
                let item = unquote(item);
                let at = item[1..].find('@').map(|i| i + 1);
                if let Some(at) = at {
                    name = item[..at].to_owned();
                    specifiers.push(item[at + 1..].trim_start_matches("npm:").to_owned());
                }
            }
            if !name.is_empty() && name != "__metadata" {
                current = Some((name, specifiers, String::new(), None));
            }
            continue;
        }
        if let Some(entry) = current.as_mut() {
            let content = line.trim();
            if let Some(version) = content.strip_prefix("version") {
                entry.2 = unquote(version.trim_start_matches(':')).to_owned();
            } else if let Some(integrity) = content
                .strip_prefix("integrity")
                .or_else(|| content.strip_prefix("checksum"))
            {
                entry.3 = Some(unquote(integrity.trim_start_matches(':')).to_owned());
            }
        }
    }
    entries.extend(current);
    entries
}

/// Every source's files, read once.
struct Snapshot<'a> {
    source: &'a StackSource,
    locked: &'a LockedSource,
    blobs: BTreeMap<String, String>,
    trees: BTreeMap<String, String>,
    gitlinks: BTreeMap<String, String>,
}

impl Snapshot<'_> {
    fn read(
        &self,
        resolver: &dyn StackResolver,
        path: &str,
    ) -> Result<Option<Vec<u8>>, ProjectError> {
        resolver
            .read_file(&self.locked.repository, &self.locked.commit, path)
            .map_err(|e| error(format!("source `{}`: {path}: {e}", self.source.name)))
    }

    fn files_named(&self, names: &[&str]) -> Vec<String> {
        self.blobs
            .keys()
            .filter(|path| names.contains(&path.rsplit('/').next().unwrap_or(path)))
            .cloned()
            .collect()
    }

    fn files_under(&self, dir: &str) -> Vec<&String> {
        let prefix = format!("{dir}/");
        self.blobs
            .keys()
            .filter(|p| p.starts_with(&prefix))
            .collect()
    }
}

fn tree_lookup(
    entries: &[TreeEntry],
) -> (
    BTreeMap<String, String>,
    BTreeMap<String, String>,
    BTreeMap<String, String>,
) {
    let (mut blobs, mut trees, mut links) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    for entry in entries {
        let map = match entry.kind.as_str() {
            "blob" => &mut blobs,
            "tree" => &mut trees,
            "commit" => &mut links,
            _ => continue,
        };
        map.insert(entry.path.clone(), entry.object.clone());
    }
    (blobs, trees, links)
}

/// The object at `path` in a tree listing (a blob or a tree).
fn object_at(entries: &[TreeEntry], path: &str) -> Option<String> {
    entries
        .iter()
        .find(|e| e.path == path)
        .map(|e| e.object.clone())
}

struct Builder<'a> {
    stack: &'a Stack,
    tracked: BTreeSet<String>,
    uses: BTreeMap<String, Vec<PackageUse>>,
    findings: Vec<Finding>,
}

impl Builder<'_> {
    fn add(&mut self, package: &str, used: PackageUse) {
        self.uses.entry(package.to_owned()).or_default().push(used);
    }

    fn find(
        &mut self,
        kind: &str,
        check: &str,
        package: &str,
        consumer: &str,
        path: &str,
        status: &str,
        detail: String,
    ) {
        let accepted = (status != "holds")
            .then(|| {
                self.stack
                    .divergences
                    .iter()
                    .find(|d| d.accepts(kind, package, consumer, path))
            })
            .flatten();
        self.findings.push(Finding {
            kind: kind.into(),
            check: check.into(),
            package: package.into(),
            consumer: consumer.into(),
            path: path.into(),
            status: if accepted.is_some() {
                "accepted"
            } else {
                status
            }
            .into(),
            detail,
            accepted_by: accepted.map(|d| format!("{}: {}", d.name, d.reason)),
        });
    }
}

/// The inventory of `sources` (already locked) for `stack`.
pub fn inventory(
    stack: &Stack,
    sources: &[LockedSource],
    stack_versions: &BTreeMap<String, String>,
    no_source_claim: &BTreeSet<String>,
    resolver: &dyn StackResolver,
) -> Result<Inventory, ProjectError> {
    let mut snapshots = vec![];
    let mut listings = BTreeMap::new();
    for locked in sources {
        let source = stack
            .source(&locked.name)
            .expect("locked sources are declared");
        let entries: Vec<TreeEntry> = resolver
            .list_tree(&locked.repository, &locked.commit)
            .map_err(|e| {
                error(format!(
                    "source `{}`: listing {}: {e}",
                    locked.name,
                    short(&locked.commit)
                ))
            })?
            .into_iter()
            .filter(|entry| source.reads(&entry.path))
            .collect();
        let (blobs, trees, gitlinks) = tree_lookup(&entries);
        listings.insert(locked.name.clone(), entries);
        snapshots.push(Snapshot {
            source,
            locked,
            blobs,
            trees,
            gitlinks,
        });
    }
    let mut builder = Builder {
        stack,
        tracked: stack
            .sources
            .iter()
            .flat_map(|s| s.provides.iter().cloned())
            .collect(),
        uses: BTreeMap::new(),
        findings: vec![],
    };

    // Canonical locations, and every manifest each source holds.
    let mut canonical: BTreeMap<String, Canonical> = BTreeMap::new();
    let mut manifests: BTreeMap<(String, String), serde_json::Value> = BTreeMap::new();
    let mut crates: BTreeMap<(String, String), toml::Table> = BTreeMap::new();
    for snapshot in &snapshots {
        for path in snapshot.files_named(&["package.json"]) {
            if let Some(bytes) = snapshot.read(resolver, &path)?
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            {
                manifests.insert((snapshot.source.name.clone(), path), value);
            }
        }
        for path in snapshot.files_named(&["Cargo.toml"]) {
            if let Some(bytes) = snapshot.read(resolver, &path)?
                && let Ok(table) = String::from_utf8_lossy(&bytes).parse::<toml::Table>()
            {
                crates.insert((snapshot.source.name.clone(), path), table);
            }
        }
    }
    let mut workspaces: Workspaces = BTreeMap::new();
    for ((source, path), value) in &manifests {
        let globs = workspace_globs(value);
        if !globs.is_empty() && !in_node_modules(path) {
            workspaces.insert(
                (source.clone(), parent(path).to_owned()),
                ("npm".into(), globs),
            );
        }
    }
    for snapshot in &snapshots {
        for path in snapshot.files_named(&["pnpm-workspace.yaml"]) {
            if let Some(bytes) = snapshot.read(resolver, &path)? {
                let globs = pnpm_workspace_globs(&String::from_utf8_lossy(&bytes));
                workspaces.insert(
                    (snapshot.source.name.clone(), parent(&path).to_owned()),
                    ("pnpm".into(), globs),
                );
            }
        }
    }
    let crate_version = |source: &str, path: &str, table: &toml::Table| -> Option<String> {
        let package = table.get("package")?.as_table()?;
        match package.get("version") {
            Some(toml::Value::String(v)) => Some(v.clone()),
            Some(toml::Value::Table(t))
                if t.get("workspace").and_then(|w| w.as_bool()) == Some(true) =>
            {
                // The nearest ancestor workspace manifest.
                let mut dir = parent(path).to_owned();
                loop {
                    dir = parent(&dir).to_owned();
                    let candidate = join(&dir, "Cargo.toml");
                    if let Some(root) = crates.get(&(source.to_owned(), candidate)) {
                        if let Some(v) = root
                            .get("workspace")
                            .and_then(|w| w.get("package"))
                            .and_then(|p| p.get("version"))
                            .and_then(|v| v.as_str())
                        {
                            return Some(v.to_owned());
                        }
                    }
                    if dir.is_empty() {
                        return None;
                    }
                }
            }
            _ => None,
        }
    };
    let crate_name = |table: &toml::Table| {
        table
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .map(str::to_owned)
    };
    for package in &builder.tracked {
        let owner = stack
            .canonical(package)
            .expect("tracked packages are provided");
        let snapshot = snapshots
            .iter()
            .find(|s| s.source.name == owner.name)
            .expect("locked");
        let (ecosystem, name) = package.split_once(':').expect("validated");
        let mut found: Vec<(String, Option<String>)> = match ecosystem {
            "npm" => manifests
                .iter()
                .filter(|((source, path), value)| {
                    *source == owner.name
                        && !in_node_modules(path)
                        && json_str(value, "name").as_deref() == Some(name)
                })
                .map(|((_, path), value)| (parent(path).to_owned(), json_str(value, "version")))
                .collect(),
            _ => crates
                .iter()
                .filter(|((source, _), table)| {
                    *source == owner.name && crate_name(table).as_deref() == Some(name)
                })
                .map(|((source, path), table)| {
                    (parent(path).to_owned(), crate_version(source, path, table))
                })
                .collect(),
        };
        // The package's own directory, not a test fixture or example that shares its name.
        found.sort_by_key(|(dir, _)| {
            (
                context(&join(dir, "x")) != "package",
                dir.matches('/').count(),
                dir.clone(),
            )
        });
        let location = found.into_iter().next();
        canonical.insert(
            package.clone(),
            Canonical {
                source: owner.name.clone(),
                commit: snapshot.locked.commit.clone(),
                tree: location.as_ref().and_then(|(dir, _)| {
                    if dir.is_empty() {
                        Some(snapshot.locked.tree.clone())
                    } else {
                        snapshot.trees.get(dir).cloned()
                    }
                }),
                version: location.as_ref().and_then(|(_, v)| v.clone()),
                path: location.map(|(dir, _)| dir),
            },
        );
    }

    // Declared copies first: they say what a directory is.
    let mut declared_copy_dirs: BTreeSet<(String, String)> = BTreeSet::new();
    for copy in &stack.copies {
        declared_copy_dirs.insert((copy.consumer.clone(), copy.path.clone()));
        compare_copy(
            &mut builder,
            copy,
            &snapshots,
            &canonical,
            &listings,
            resolver,
        )?;
    }

    for snapshot in &snapshots {
        let consumer = snapshot.source.name.as_str();
        let is_canonical =
            |package: &str| canonical.get(package).is_some_and(|c| c.source == consumer);

        // npm manifests: declarations, undeclared copies, installed packages.
        for ((source, path), value) in manifests.range((consumer.to_owned(), String::new())..) {
            if source != consumer {
                break;
            }
            let dir = parent(path);
            if in_node_modules(path) {
                let key = dir.to_owned();
                if let Some((name, direct)) = node_modules_package(&key) {
                    let package = format!("npm:{name}");
                    if builder.tracked.contains(&package) {
                        builder.add(
                            &package,
                            PackageUse {
                                consumer: consumer.into(),
                                path: path.clone(),
                                kind: "installed".into(),
                                scope: "production".into(),
                                context: context(path).into(),
                                direct,
                                specifier: None,
                                version: json_str(value, "version"),
                                origin: "copy".into(),
                                digest: snapshot.trees.get(dir).cloned(),
                                matches: None,
                            },
                        );
                        builder.find(
                            "provenance-gap",
                            "committed-install",
                            &package,
                            consumer,
                            path,
                            "open",
                            format!(
                                "{consumer} commits an installed {name} {} under node_modules: the build uses it, not what its lockfile resolves",
                                json_str(value, "version").unwrap_or_default()
                            ),
                        );
                    }
                }
                continue;
            }
            if let Some(name) = json_str(value, "name") {
                let package = format!("npm:{name}");
                if builder.tracked.contains(&package)
                    && !is_canonical(&package)
                    && !declared_copy_dirs.contains(&(consumer.to_owned(), dir.to_owned()))
                {
                    undeclared_copy(
                        &mut builder,
                        &package,
                        consumer,
                        dir,
                        json_str(value, "version"),
                        snapshot,
                        &canonical,
                    );
                }
            }
            for (section, scope) in NPM_SECTIONS {
                let Some(deps) = value.get(section).and_then(|d| d.as_object()) else {
                    continue;
                };
                for (dependency, specifier) in deps {
                    let Some(specifier) = specifier.as_str() else {
                        continue;
                    };
                    // `"alias": "npm:@feltdb/core@^0.12"`.
                    let (name, specifier) = match specifier.strip_prefix("npm:") {
                        Some(aliased) => match aliased[1..].find('@') {
                            Some(at) => {
                                (aliased[..at + 1].to_owned(), aliased[at + 2..].to_owned())
                            }
                            None => (aliased.to_owned(), "*".to_owned()),
                        },
                        None => (dependency.clone(), specifier.to_owned()),
                    };
                    let package = format!("npm:{name}");
                    if !builder.tracked.contains(&package) {
                        continue;
                    }
                    let origin = npm_origin(&specifier);
                    builder.add(
                        &package,
                        PackageUse {
                            consumer: consumer.into(),
                            path: path.clone(),
                            kind: "declared".into(),
                            scope: (*scope).into(),
                            context: context(path).into(),
                            direct: true,
                            specifier: Some(specifier.clone()),
                            version: None,
                            origin: origin.clone(),
                            digest: None,
                            matches: None,
                        },
                    );
                    dependency_provenance(
                        &mut builder,
                        &package,
                        consumer,
                        path,
                        &origin,
                        snapshot,
                    );
                }
            }
        }

        // npm lockfiles.
        for path in snapshot.files_named(&["package-lock.json", "npm-shrinkwrap.json"]) {
            if in_node_modules(&path) {
                continue;
            }
            if let Some((root, _)) = workspace_root(snapshot, parent(&path), &workspaces) {
                builder.find("resolution-drift", "shadowed-lockfile", "", consumer, &path, "open",
                    format!("{path} belongs to a member of the workspace at {}; the workspace's lockfile governs it, so this one is never used and can disagree", if root.is_empty() { "the root" } else { &root }));
            }
            let Some(bytes) = snapshot.read(resolver, &path)? else {
                continue;
            };
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                builder.find(
                    "resolution-drift",
                    "unreadable-lockfile",
                    "",
                    consumer,
                    &path,
                    "violated",
                    format!("{path} is not valid JSON"),
                );
                continue;
            };
            let mut entries: Vec<(String, String, serde_json::Value)> = vec![];
            if let Some(packages) = value.get("packages").and_then(|p| p.as_object()) {
                for (key, entry) in packages {
                    if let Some((name, _)) = node_modules_package(key) {
                        let mut entry = entry.clone();
                        // A workspace link: its version is the linked package's.
                        if entry.get("link").and_then(|l| l.as_bool()) == Some(true)
                            && let Some(target) = json_str(&entry, "resolved")
                            && let Some(version) =
                                packages.get(&target).and_then(|t| json_str(t, "version"))
                        {
                            entry["version"] = serde_json::Value::String(version);
                        }
                        entries.push((key.clone(), name, entry));
                    }
                }
            } else if let Some(deps) = value.get("dependencies").and_then(|d| d.as_object()) {
                // lockfileVersion 1: a nested `dependencies` tree.
                let mut stack_ = vec![(String::new(), deps.clone())];
                while let Some((prefix, deps)) = stack_.pop() {
                    for (name, entry) in deps {
                        let key = format!("{prefix}node_modules/{name}");
                        if let Some(nested) = entry.get("dependencies").and_then(|d| d.as_object())
                        {
                            stack_.push((format!("{key}/"), nested.clone()));
                        }
                        entries.push((key, name.clone(), entry.clone()));
                    }
                }
            }
            for (key, name, entry) in entries {
                let package = format!("npm:{name}");
                if !builder.tracked.contains(&package) {
                    continue;
                }
                let direct = node_modules_package(&key).is_some_and(|(_, d)| d);
                let resolved = json_str(&entry, "resolved").unwrap_or_default();
                let link = entry.get("link").and_then(|l| l.as_bool()) == Some(true);
                let origin = if link {
                    format!("link:{}", json_str(&entry, "resolved").unwrap_or_default())
                } else if resolved.starts_with("https://registry.") || resolved.is_empty() {
                    "registry".into()
                } else if resolved.starts_with("git") || resolved.contains("github") {
                    format!("git:{resolved}")
                } else {
                    format!("path:{resolved}")
                };
                let flag = |f: &str| entry.get(f).and_then(|v| v.as_bool()) == Some(true);
                let scope = if flag("dev") {
                    "development"
                } else if flag("peer") {
                    "peer"
                } else if flag("optional") || flag("devOptional") {
                    "optional"
                } else {
                    "production"
                };
                let version = json_str(&entry, "version");
                let integrity = json_str(&entry, "integrity");
                if origin == "registry" && integrity.is_none() && !link {
                    builder.find(
                        "provenance-gap",
                        "no-integrity",
                        &package,
                        consumer,
                        &format!("{path}#{key}"),
                        "open",
                        format!(
                            "{path} resolves {name} {} without an integrity",
                            version.clone().unwrap_or_default()
                        ),
                    );
                }
                builder.add(
                    &package,
                    PackageUse {
                        consumer: consumer.into(),
                        path: format!("{path}#{key}"),
                        kind: "resolved".into(),
                        scope: scope.into(),
                        context: context(&path).into(),
                        direct,
                        specifier: None,
                        version,
                        origin,
                        digest: integrity,
                        matches: None,
                    },
                );
            }
        }

        // pnpm lockfiles.
        for path in snapshot.files_named(&["pnpm-lock.yaml"]) {
            if in_node_modules(&path) {
                continue;
            }
            let Some(bytes) = snapshot.read(resolver, &path)? else {
                continue;
            };
            let lock = parse_pnpm(&String::from_utf8_lossy(&bytes));
            let mut direct_versions: BTreeSet<(String, String)> = BTreeSet::new();
            for (importer, name, specifier, version, section) in &lock.importers {
                let package = format!("npm:{name}");
                if !builder.tracked.contains(&package) {
                    continue;
                }
                direct_versions.insert((name.clone(), version.clone()));
                let scope = NPM_SECTIONS
                    .iter()
                    .find(|(s, _)| s == section)
                    .map_or("production", |(_, scope)| scope);
                let (version, origin) = match version.strip_prefix("link:") {
                    // A workspace link: its version is the linked package's own.
                    Some(target) => {
                        let importer_dir =
                            join(parent(&path), if importer == "." { "" } else { importer });
                        let linked = normalize(&join(&importer_dir, target))
                            .and_then(|dir| {
                                manifests.get(&(consumer.to_owned(), join(&dir, "package.json")))
                            })
                            .and_then(|m| json_str(m, "version"));
                        (linked, format!("link:{target}"))
                    }
                    None => (Some(version.clone()), npm_origin(specifier)),
                };
                builder.add(
                    &package,
                    PackageUse {
                        consumer: consumer.into(),
                        path: format!("{path}#importers/{importer}"),
                        kind: "resolved".into(),
                        scope: scope.into(),
                        context: context(&path).into(),
                        direct: true,
                        specifier: Some(specifier.clone()),
                        digest: version.as_ref().and_then(|v| {
                            lock.packages
                                .iter()
                                .find(|(n, pv, _)| n == name && pv == v)
                                .and_then(|(_, _, i)| i.clone())
                        }),
                        version,
                        origin,
                        matches: None,
                    },
                );
            }
            for (name, version, integrity) in &lock.packages {
                let package = format!("npm:{name}");
                if !builder.tracked.contains(&package)
                    || direct_versions.contains(&(name.clone(), version.clone()))
                {
                    continue;
                }
                builder.add(
                    &package,
                    PackageUse {
                        consumer: consumer.into(),
                        path: format!("{path}#packages/{name}@{version}"),
                        kind: "resolved".into(),
                        scope: "production".into(),
                        context: context(&path).into(),
                        direct: false,
                        specifier: None,
                        version: Some(version.clone()),
                        origin: "registry".into(),
                        digest: integrity.clone(),
                        matches: None,
                    },
                );
            }
        }

        // Yarn lockfiles.
        for path in snapshot.files_named(&["yarn.lock"]) {
            if in_node_modules(&path) {
                continue;
            }
            let Some(bytes) = snapshot.read(resolver, &path)? else {
                continue;
            };
            for (name, specifiers, version, integrity) in
                parse_yarn(&String::from_utf8_lossy(&bytes))
            {
                let package = format!("npm:{name}");
                if !builder.tracked.contains(&package) {
                    continue;
                }
                builder.add(
                    &package,
                    PackageUse {
                        consumer: consumer.into(),
                        path: format!("{path}#{name}@{version}"),
                        kind: "resolved".into(),
                        scope: "production".into(),
                        context: context(&path).into(),
                        direct: false,
                        specifier: Some(specifiers.join(", ")),
                        version: Some(version),
                        origin: "registry".into(),
                        digest: integrity,
                        matches: None,
                    },
                );
            }
        }

        // Cargo manifests: declarations and undeclared copies.
        for ((source, path), table) in crates.range((consumer.to_owned(), String::new())..) {
            if source != consumer {
                break;
            }
            let dir = parent(path);
            if let Some(name) = crate_name(table) {
                let package = format!("cargo:{name}");
                if builder.tracked.contains(&package)
                    && !is_canonical(&package)
                    && !declared_copy_dirs.contains(&(consumer.to_owned(), dir.to_owned()))
                {
                    undeclared_copy(
                        &mut builder,
                        &package,
                        consumer,
                        dir,
                        crate_version(source, path, table),
                        snapshot,
                        &canonical,
                    );
                }
            }
            let mut sections: Vec<(&str, &toml::Table)> = vec![];
            for (key, scope) in [
                ("dependencies", "production"),
                ("dev-dependencies", "development"),
                ("build-dependencies", "build"),
            ] {
                if let Some(t) = table.get(key).and_then(|t| t.as_table()) {
                    sections.push((scope, t));
                }
                if let Some(targets) = table.get("target").and_then(|t| t.as_table()) {
                    for target in targets.values() {
                        if let Some(t) = target.get(key).and_then(|t| t.as_table()) {
                            sections.push((scope, t));
                        }
                    }
                }
            }
            if let Some(t) = table
                .get("workspace")
                .and_then(|w| w.get("dependencies"))
                .and_then(|t| t.as_table())
            {
                sections.push(("production", t));
            }
            for (scope, deps) in sections {
                for (key, spec) in deps {
                    let (name, specifier, origin) = match spec {
                        toml::Value::String(version) => {
                            (key.clone(), Some(version.clone()), "registry".to_owned())
                        }
                        toml::Value::Table(t) => {
                            let name = t
                                .get("package")
                                .and_then(|p| p.as_str())
                                .unwrap_or(key)
                                .to_owned();
                            let version =
                                t.get("version").and_then(|v| v.as_str()).map(str::to_owned);
                            let origin = if let Some(p) = t.get("path").and_then(|p| p.as_str()) {
                                format!("path:{p}")
                            } else if let Some(g) = t.get("git").and_then(|g| g.as_str()) {
                                let rev = ["rev", "tag", "branch"]
                                    .iter()
                                    .find_map(|k| t.get(*k).and_then(|v| v.as_str()))
                                    .unwrap_or("");
                                format!("git:{g}#{rev}")
                            } else if t.get("workspace").and_then(|w| w.as_bool()) == Some(true) {
                                "workspace".to_owned()
                            } else {
                                "registry".to_owned()
                            };
                            (name, version, origin)
                        }
                        _ => continue,
                    };
                    let package = format!("cargo:{name}");
                    if !builder.tracked.contains(&package) {
                        continue;
                    }
                    builder.add(
                        &package,
                        PackageUse {
                            consumer: consumer.into(),
                            path: path.clone(),
                            kind: "declared".into(),
                            scope: scope.into(),
                            context: context(path).into(),
                            direct: true,
                            specifier,
                            version: None,
                            origin: origin.clone(),
                            digest: None,
                            matches: None,
                        },
                    );
                    dependency_provenance(
                        &mut builder,
                        &package,
                        consumer,
                        path,
                        &origin,
                        snapshot,
                    );
                }
            }
        }

        // Cargo lockfiles.
        for path in snapshot.files_named(&["Cargo.lock"]) {
            let Some(bytes) = snapshot.read(resolver, &path)? else {
                continue;
            };
            let Ok(table) = String::from_utf8_lossy(&bytes).parse::<toml::Table>() else {
                builder.find(
                    "resolution-drift",
                    "unreadable-lockfile",
                    "",
                    consumer,
                    &path,
                    "violated",
                    format!("{path} is not valid TOML"),
                );
                continue;
            };
            for entry in table
                .get("package")
                .and_then(|p| p.as_array())
                .into_iter()
                .flatten()
            {
                let Some(name) = entry.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                let package = format!("cargo:{name}");
                if !builder.tracked.contains(&package) {
                    continue;
                }
                let version = entry
                    .get("version")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                let origin = match entry.get("source").and_then(|s| s.as_str()) {
                    None => "path".to_owned(),
                    Some(s) if s.starts_with("registry+") => "registry".to_owned(),
                    Some(s) => format!("git:{}", s.trim_start_matches("git+")),
                };
                builder.add(
                    &package,
                    PackageUse {
                        consumer: consumer.into(),
                        path: format!("{path}#{name}@{}", version.clone().unwrap_or_default()),
                        kind: "resolved".into(),
                        scope: "production".into(),
                        context: context(&path).into(),
                        direct: false,
                        specifier: None,
                        version,
                        origin,
                        digest: entry
                            .get("checksum")
                            .and_then(|c| c.as_str())
                            .map(str::to_owned),
                        matches: None,
                    },
                );
            }
        }

        // Submodules: a pinned commit of another repository.
        for (path, commit) in &snapshot.gitlinks {
            let url = snapshot
                .read(resolver, ".gitmodules")?
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .and_then(|text| {
                    let mut current = None;
                    for line in text.lines() {
                        let line = line.trim();
                        if let Some(p) = line.strip_prefix("path") {
                            current = Some(p.trim_start_matches([' ', '=']).trim().to_owned());
                        }
                        if let Some(u) = line.strip_prefix("url")
                            && current.as_deref() == Some(path.as_str())
                        {
                            return Some(u.trim_start_matches([' ', '=']).trim().to_owned());
                        }
                    }
                    None
                })
                .unwrap_or_default();
            let target = git_identity(&url).and_then(|id| {
                stack
                    .sources
                    .iter()
                    .find(|s| s.repository.to_ascii_lowercase() == id)
            });
            match target {
                None => builder.find("provenance-gap", "undeclared-submodule", "", consumer, path, "open",
                    format!("{consumer} embeds {url} at {} as a submodule; it is not a source of this stack", short(commit))),
                Some(target) => {
                    let locked = sources.iter().find(|s| s.name == target.name).expect("locked");
                    if locked.commit != *commit {
                        builder.find("source-drift", "submodule-pin", "", consumer, path, "open",
                            format!("{consumer} embeds {} at {}; the stack locks {}", target.name, short(commit), short(&locked.commit)));
                    }
                }
            }
        }
    }

    // Expectations a canonical source states for its consumers.
    let mut expected: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut unlocated: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for snapshot in &snapshots {
        let Some(path) = &snapshot.source.expectations else {
            continue;
        };
        let Some(bytes) = snapshot.read(resolver, path)? else {
            builder.find(
                "provenance-gap",
                "missing-expectations",
                "",
                &snapshot.source.name,
                path,
                "open",
                format!(
                    "{} declares expectations in {path}, which it does not have",
                    snapshot.source.name
                ),
            );
            continue;
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| error(format!("{}:{path}: {e}", snapshot.source.name)))?;
        if json_str(&value, "schema").as_deref() != Some("feltdb.compatibility-manifest.v1") {
            return Err(error(format!(
                "{}:{path} is not a feltdb.compatibility-manifest.v1",
                snapshot.source.name
            )));
        }
        for (consumer, entry) in value
            .get("consumers")
            .and_then(|c| c.as_object())
            .into_iter()
            .flatten()
        {
            let (Some(package), Some(version)) =
                (json_str(entry, "package"), json_str(entry, "expected"))
            else {
                continue;
            };
            let package = format!("npm:{package}");
            if stack.source(consumer).is_some() {
                expected.insert((package, consumer.clone()), version);
            } else {
                unlocated.entry(package).or_default().push(consumer.clone());
            }
        }
    }

    // Findings over the collected uses.
    let mut packages = vec![];
    for package in builder.tracked.clone() {
        let mut uses = builder.uses.remove(&package).unwrap_or_default();
        uses.sort_by(|a, b| (&a.consumer, &a.path, &a.kind).cmp(&(&b.consumer, &b.path, &b.kind)));
        uses.dedup();
        let location = canonical[&package].clone();
        let stack_version = package
            .strip_prefix("npm:")
            .and_then(|p| stack_versions.get(p))
            .cloned();
        let reference = stack_version.clone().or_else(|| location.version.clone());
        let name = package.split_once(':').map_or(package.as_str(), |(_, n)| n);

        // Resolution drift: a lockfile resolving more than one version.
        let mut per_lockfile: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
        for used in uses.iter().filter(|u| u.kind == "resolved") {
            if let Some(version) = &used.version {
                let file = used.path.split('#').next().unwrap_or(&used.path).to_owned();
                per_lockfile
                    .entry((used.consumer.clone(), file))
                    .or_default()
                    .insert(version.clone());
            }
        }
        for ((consumer, file), versions) in &per_lockfile {
            if versions.len() > 1 {
                builder.find(
                    "resolution-drift",
                    "duplicate",
                    &package,
                    consumer,
                    file,
                    "open",
                    format!(
                        "{file} resolves {name} at {} versions: {}",
                        versions.len(),
                        versions.iter().cloned().collect::<Vec<_>>().join(", ")
                    ),
                );
            }
        }

        // Resolution drift: a declaration its lockfile does not satisfy.
        for declared in uses
            .iter()
            .filter(|u| u.kind == "declared" && u.origin == "registry")
        {
            let Some(specifier) = &declared.specifier else {
                continue;
            };
            let snapshot = snapshots
                .iter()
                .find(|s| s.source.name == declared.consumer)
                .expect("locked");
            let lock = covering_lock(
                snapshot,
                &declared.path,
                package.starts_with("cargo:"),
                &workspaces,
            );
            let lock = match lock {
                Coverage::Locked(lock) => lock,
                Coverage::Unlocked(root) => {
                    if declared.context == "package" {
                        let why = match root {
                            Some(root) => format!(
                                "it is a member of the workspace at {}, which commits no lockfile of its manager",
                                if root.is_empty() { "the root" } else { &root }
                            ),
                            None => "no lockfile covers it".to_owned(),
                        };
                        builder.find("provenance-gap", "unlocked", &package, &declared.consumer, &declared.path, "open",
                            format!("{} declares {name} {specifier}; {why}: what installs is not recorded", declared.path));
                    }
                    continue;
                }
            };
            let candidates: Vec<&PackageUse> = uses
                .iter()
                .filter(|u| {
                    u.kind == "resolved"
                        && u.consumer == declared.consumer
                        && u.path.split('#').next() == Some(lock.as_str())
                })
                .collect();
            let importer = parent(&declared.path)
                .strip_prefix(parent(&lock))
                .map(|rest| rest.trim_start_matches('/').to_owned())
                .unwrap_or_default();
            let resolved = candidates
                .iter()
                .filter(|u| {
                    let key = u.path.split_once('#').map_or("", |(_, k)| k);
                    if package.starts_with("cargo:") || lock.ends_with("yarn.lock") {
                        true
                    } else if lock.ends_with("pnpm-lock.yaml") {
                        key == format!(
                            "importers/{}",
                            if importer.is_empty() { "." } else { &importer }
                        )
                    } else {
                        // The importer's own entry, or the hoisted one above it.
                        key == join(&importer, &format!("node_modules/{name}"))
                            || key == format!("node_modules/{name}")
                    }
                })
                .filter_map(|u| u.version.clone())
                .collect::<BTreeSet<_>>();
            let satisfies = |v: &str| {
                if package.starts_with("cargo:") {
                    cargo_satisfies(specifier, v)
                } else {
                    range_satisfies(specifier, v)
                }
            };
            // A lockfile out of step with its own manifest breaks a clean
            // install (`npm ci` refuses it); in a fixture or example it is drift.
            let broken = if declared.context == "package" {
                "violated"
            } else {
                "open"
            };
            if resolved.is_empty() {
                builder.find(
                    "resolution-drift",
                    "missing-from-lock",
                    &package,
                    &declared.consumer,
                    &declared.path,
                    broken,
                    format!(
                        "{} declares {name} {specifier}; {lock} resolves no {name} for it",
                        declared.path
                    ),
                );
            } else if resolved.iter().all(|v| satisfies(v) == Some(false)) {
                builder.find(
                    "resolution-drift",
                    "lock-mismatch",
                    &package,
                    &declared.consumer,
                    &declared.path,
                    broken,
                    format!(
                        "{} declares {name} {specifier}; {lock} resolves {}",
                        declared.path,
                        resolved.into_iter().collect::<Vec<_>>().join(", ")
                    ),
                );
            }
        }

        // Version and compatibility drift, per consumer, against the stack.
        if let Some(reference) = &reference {
            // version → whether any use of it is direct.
            let mut by_consumer: BTreeMap<(String, String), BTreeMap<String, bool>> =
                BTreeMap::new();
            for used in uses
                .iter()
                .filter(|u| matches!(u.kind.as_str(), "resolved" | "installed" | "copy"))
            {
                // The canonical source linking its own workspace package is not a consumer.
                if used.consumer == location.source
                    && (used.kind != "resolved" || used.origin.starts_with("link"))
                {
                    continue;
                }
                if let Some(version) = &used.version {
                    let direct = by_consumer
                        .entry((used.consumer.clone(), used.context.clone()))
                        .or_default()
                        .entry(version.clone())
                        .or_default();
                    *direct |= used.direct;
                }
            }
            for ((consumer, ctx), versions) in &by_consumer {
                let other: Vec<String> = versions
                    .iter()
                    .filter(|(v, _)| *v != reference)
                    .map(|(v, direct)| {
                        if *direct {
                            v.clone()
                        } else {
                            format!("{v} (transitive)")
                        }
                    })
                    .collect();
                if other.is_empty() {
                    continue;
                }
                let against = match (&stack_version, &location.version) {
                    (Some(stack), Some(source)) if stack != source => {
                        format!("the stack runs {stack}; {} is at {source}", location.source)
                    }
                    (Some(stack), _) => format!("the stack runs {stack}"),
                    (None, Some(source)) => format!("{} is at {source}", location.source),
                    (None, None) => String::new(),
                };
                builder.find(
                    "version-drift",
                    if ctx == "package" {
                        "consumer-version"
                    } else {
                        "fixture-version"
                    },
                    &package,
                    consumer,
                    ctx,
                    "open",
                    format!(
                        "{consumer} ({ctx}) uses {name} {}; {against}",
                        other.join(", ")
                    ),
                );
            }
            for declared in uses.iter().filter(|u| {
                u.kind == "declared"
                    && u.origin == "registry"
                    && u.context == "package"
                    && matches!(u.scope.as_str(), "production" | "peer")
            }) {
                let Some(specifier) = &declared.specifier else {
                    continue;
                };
                if declared.consumer == location.source {
                    continue;
                }
                let satisfies = if package.starts_with("cargo:") {
                    cargo_satisfies(specifier, reference)
                } else {
                    range_satisfies(specifier, reference)
                };
                if satisfies == Some(false) {
                    builder.find("compatibility-drift", "excludes-stack-version", &package, &declared.consumer, &declared.path, "open",
                        format!("{} requires {name} {specifier}, which does not admit {reference}, the version this stack locks or the canonical source is at", declared.path));
                }
            }
        }
        for ((expected_package, consumer), version) in &expected {
            if *expected_package != package {
                continue;
            }
            let observed: BTreeSet<String> = uses
                .iter()
                .filter(|u| {
                    &u.consumer == consumer
                        && u.context == "package"
                        && u.direct
                        && matches!(u.kind.as_str(), "resolved" | "installed")
                })
                .filter_map(|u| u.version.clone())
                .collect();
            if observed.iter().any(|v| v != version) || observed.is_empty() {
                builder.find(
                    "version-drift",
                    "expected-version",
                    &package,
                    consumer,
                    "",
                    "open",
                    format!(
                        "{} expects {consumer} to run {name} {version}; {consumer} resolves {}",
                        location.source,
                        if observed.is_empty() {
                            "none".to_owned()
                        } else {
                            observed.into_iter().collect::<Vec<_>>().join(", ")
                        }
                    ),
                );
            }
        }
        if package
            .strip_prefix("npm:")
            .is_some_and(|p| no_source_claim.contains(p))
        {
            builder.find(
                "provenance-gap",
                "no-source-claim",
                &package,
                &location.source,
                "",
                "open",
                format!(
                    "the published {name} {} records no source commit: it cannot be tied to {}",
                    stack_version.clone().unwrap_or_default(),
                    location.source
                ),
            );
        }
        let mut unlocated_here = unlocated.remove(&package).unwrap_or_default();
        unlocated_here.sort();
        packages.push(InventoryPackage {
            package,
            canonical: location,
            stack_version,
            uses,
            unlocated: unlocated_here,
        });
    }
    let mut findings = builder.findings;
    findings.sort();
    findings.dedup();
    Ok(Inventory { packages, findings })
}

enum Coverage {
    Locked(String),
    /// No lockfile covers it; `Some(root)` when it is a member of a workspace that commits none.
    Unlocked(Option<String>),
}

/// Workspace roots → (manager, member globs), from `package.json#workspaces` and `pnpm-workspace.yaml`.
type Workspaces = BTreeMap<(String, String), (String, Vec<String>)>;

fn workspace_globs(value: &serde_json::Value) -> Vec<String> {
    let list = match value.get("workspaces") {
        Some(serde_json::Value::Array(items)) => items.clone(),
        Some(serde_json::Value::Object(map)) => map
            .get("packages")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default(),
        _ => vec![],
    };
    list.into_iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

fn pnpm_workspace_globs(text: &str) -> Vec<String> {
    let mut globs = vec![];
    let mut inside = false;
    for line in text.lines() {
        if !line.starts_with(' ') && !line.starts_with('-') {
            inside = line.trim_end() == "packages:";
            continue;
        }
        if inside && let Some(item) = line.trim().strip_prefix('-') {
            let item = unquote(item);
            if !item.starts_with('!') {
                globs.push(item.to_owned());
            }
        }
    }
    globs
}

/// Whether `dir` (relative to the workspace root) matches a member glob: `a/b`, `a/*`, `a/**`.
fn member(globs: &[String], dir: &str) -> bool {
    globs.iter().any(|glob| {
        let glob = glob.trim_start_matches("./").trim_end_matches('/');
        if let Some(prefix) = glob.strip_suffix("/**") {
            dir.starts_with(&format!("{prefix}/"))
        } else if let Some(prefix) = glob.strip_suffix("/*") {
            dir.strip_prefix(&format!("{prefix}/"))
                .is_some_and(|rest| !rest.contains('/'))
        } else {
            dir == glob
        }
    })
}

const NPM_LOCKS: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
];

/// The workspace root above `dir` that lists it as a member, if any.
fn workspace_root<'w>(
    snapshot: &Snapshot,
    dir: &str,
    workspaces: &'w Workspaces,
) -> Option<(String, &'w str)> {
    let mut root = dir.to_owned();
    while !root.is_empty() {
        root = parent(&root).to_owned();
        if let Some((manager, globs)) =
            workspaces.get(&(snapshot.source.name.clone(), root.clone()))
        {
            let relative = if root.is_empty() {
                dir.to_owned()
            } else {
                dir.strip_prefix(&format!("{root}/"))
                    .unwrap_or("")
                    .to_owned()
            };
            if member(globs, &relative) {
                return Some((root, manager.as_str()));
            }
        }
    }
    None
}

/// The lockfile that governs a manifest, as its package manager would pick it:
/// a workspace member's is its workspace root's; otherwise its own directory's.
/// Cargo: the nearest `Cargo.lock` above it.
fn covering_lock(
    snapshot: &Snapshot,
    manifest: &str,
    cargo: bool,
    workspaces: &Workspaces,
) -> Coverage {
    let own = parent(manifest).to_owned();
    if cargo {
        let mut dir = own;
        loop {
            let candidate = join(&dir, "Cargo.lock");
            if snapshot.blobs.contains_key(&candidate) {
                return Coverage::Locked(candidate);
            }
            if dir.is_empty() {
                return Coverage::Unlocked(None);
            }
            dir = parent(&dir).to_owned();
        }
    }
    if let Some((root, manager)) = workspace_root(snapshot, &own, workspaces) {
        let names: &[&str] = if manager == "pnpm" {
            &["pnpm-lock.yaml"]
        } else {
            NPM_LOCKS
        };
        return match names
            .iter()
            .map(|n| join(&root, n))
            .find(|c| snapshot.blobs.contains_key(c))
        {
            Some(found) => Coverage::Locked(found),
            None => Coverage::Unlocked(Some(root)),
        };
    }
    match NPM_LOCKS
        .iter()
        .map(|n| join(&own, n))
        .find(|c| snapshot.blobs.contains_key(c))
    {
        Some(found) => Coverage::Locked(found),
        None => Coverage::Unlocked(None),
    }
}

fn dependency_provenance(
    builder: &mut Builder,
    package: &str,
    consumer: &str,
    path: &str,
    origin: &str,
    _snapshot: &Snapshot,
) {
    if let Some(target) = origin.strip_prefix("path:") {
        if normalize(&join(parent(path), target)).is_none() {
            builder.find("provenance-gap", "external-path", package, consumer, path, "open",
                format!("{path} takes {package} from `{target}`, outside {consumer}: the build depends on a sibling checkout no lock records"));
        }
    } else if let Some(url) = origin.strip_prefix("git:") {
        let identity = git_identity(url);
        let source = identity.as_ref().and_then(|id| {
            builder
                .stack
                .sources
                .iter()
                .find(|s| s.repository.to_ascii_lowercase() == *id)
        });
        match (source, git_rev(url)) {
            (None, _) => builder.find(
                "provenance-gap",
                "undeclared-git",
                package,
                consumer,
                path,
                "open",
                format!("{path} takes {package} from {url}, which is not a source of this stack"),
            ),
            (Some(_), None) => builder.find(
                "provenance-gap",
                "floating-git",
                package,
                consumer,
                path,
                "open",
                format!(
                    "{path} takes {package} from {url} with no commit: each resolution can differ"
                ),
            ),
            (Some(source), Some(rev)) => {
                builder.find(
                    "source-drift",
                    "git-pin",
                    package,
                    consumer,
                    path,
                    "open",
                    format!(
                        "{path} takes {package} from {} at {rev}, outside the stack's lock of {}",
                        source.name, source.name
                    ),
                );
            }
        }
    }
}

fn undeclared_copy(
    builder: &mut Builder,
    package: &str,
    consumer: &str,
    dir: &str,
    version: Option<String>,
    snapshot: &Snapshot,
    canonical: &BTreeMap<String, Canonical>,
) {
    let tree = if dir.is_empty() {
        Some(snapshot.locked.tree.clone())
    } else {
        snapshot.trees.get(dir).cloned()
    };
    let location = &canonical[package];
    let identical = tree.is_some() && tree == location.tree;
    builder.add(
        package,
        PackageUse {
            consumer: consumer.into(),
            path: dir.into(),
            kind: "copy".into(),
            scope: "production".into(),
            context: context(&join(dir, "x")).into(),
            direct: true,
            specifier: None,
            version: version.clone(),
            origin: "copy".into(),
            digest: tree,
            matches: Some(
                if identical {
                    "canonical-locked"
                } else {
                    "differs"
                }
                .into(),
            ),
        },
    );
    let place = if dir.is_empty() {
        "its root".to_owned()
    } else {
        dir.to_owned()
    };
    builder.find("provenance-gap", "undeclared-copy", package, consumer, dir, "open",
        format!("{consumer} holds a package named {package} at {place} ({}) with no declared origin in {}",
            version.unwrap_or_else(|| "no version".into()), location.source));
    if !identical {
        builder.find(
            "source-drift",
            "undeclared-copy-differs",
            package,
            consumer,
            dir,
            "open",
            format!(
                "{consumer}:{place} differs from {}@{}:{}",
                location.source,
                short(&location.commit),
                location.path.clone().unwrap_or_default()
            ),
        );
    }
}

fn compare_copy(
    builder: &mut Builder,
    copy: &SourceCopy,
    snapshots: &[Snapshot],
    canonical: &BTreeMap<String, Canonical>,
    listings: &BTreeMap<String, Vec<TreeEntry>>,
    resolver: &dyn StackResolver,
) -> Result<(), ProjectError> {
    let consumer = snapshots
        .iter()
        .find(|s| s.source.name == copy.consumer)
        .expect("validated");
    let location = &canonical[&copy.package];
    let owner = snapshots
        .iter()
        .find(|s| s.source.name == location.source)
        .expect("validated");
    let Some(tree) = consumer.trees.get(&copy.path).cloned() else {
        builder.find(
            "source-drift",
            "missing-copy",
            &copy.package,
            &copy.consumer,
            &copy.path,
            "violated",
            format!(
                "copy `{}` declares {}:{}, which does not exist",
                copy.name, copy.consumer, copy.path
            ),
        );
        return Ok(());
    };
    // Where it says it came from.
    let (origin, origin_reason) = match &copy.origin {
        CopyOrigin::Commit(commit) => (Some(commit.clone()), None),
        CopyOrigin::Pin(pin) => {
            let relationship = builder.stack.relationships.iter().find(|r| r.name() == pin);
            match relationship {
                Some(Relationship::Pin {
                    file,
                    pattern,
                    reason,
                    must_be_locked,
                    ..
                }) => {
                    let text = consumer
                        .read(resolver, file)?
                        .map(|b| String::from_utf8_lossy(&b).into_owned());
                    let commit = text.as_deref().and_then(|t| extract_pin(t, pattern));
                    (commit, (!must_be_locked).then(|| reason.clone()).flatten())
                }
                _ => (None, None),
            }
        }
        CopyOrigin::Undeclared => (None, None),
    };
    let origin_entries = match &origin {
        Some(commit) if *commit == owner.locked.commit => {
            Some(listings[&owner.source.name].clone())
        }
        Some(commit) => Some(
            resolver
                .list_tree(&owner.locked.repository, commit)
                .map_err(|e| {
                    error(format!(
                        "copy `{}`: {} at {}: {e}",
                        copy.name,
                        owner.source.name,
                        short(commit)
                    ))
                })?,
        ),
        None => None,
    };
    let locked_entries = &listings[&owner.source.name];
    let version = consumer
        .read(resolver, &join(&copy.path, "Cargo.toml"))?
        .and_then(|b| String::from_utf8_lossy(&b).parse::<toml::Table>().ok())
        .and_then(|t| {
            t.get("package")
                .and_then(|p| p.get("version"))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .or_else(|| {
            consumer
                .read(resolver, &join(&copy.path, "package.json"))
                .ok()
                .flatten()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .and_then(|v| json_str(&v, "version"))
        });

    let mut modified = vec![];
    let mut stale = vec![];
    for (here, there) in &copy.map {
        let ours = consumer
            .trees
            .get(&join(&copy.path, here))
            .or_else(|| consumer.blobs.get(&join(&copy.path, here)))
            .cloned();
        let at_origin = origin_entries.as_ref().and_then(|e| object_at(e, there));
        let at_locked = object_at(locked_entries, there);
        if origin_entries.is_some() && ours != at_origin {
            modified.push(here.clone());
        }
        if ours != at_locked {
            stale.push(here.clone());
        }
    }
    // Every file is mapped or declared local.
    let covered = |path: &str| {
        let relative = &path[copy.path.len() + 1..];
        copy.map
            .iter()
            .map(|(h, _)| h)
            .chain(copy.local.iter())
            .any(|prefix| {
                let prefix = prefix.trim_end_matches('/');
                relative == prefix || relative.starts_with(&format!("{prefix}/"))
            })
    };
    let unexplained: Vec<String> = consumer
        .files_under(&copy.path)
        .into_iter()
        .filter(|path| !covered(path))
        .cloned()
        .collect();

    let matches = if origin.is_none() {
        "differs"
    } else if !modified.is_empty() {
        "modified"
    } else if stale.is_empty() {
        "canonical-locked"
    } else {
        "canonical-origin"
    };
    builder.add(
        &copy.package,
        PackageUse {
            consumer: copy.consumer.clone(),
            path: copy.path.clone(),
            kind: "copy".into(),
            scope: "production".into(),
            context: context(&join(&copy.path, "x")).into(),
            direct: true,
            specifier: None,
            version,
            origin: origin.as_ref().map_or("copy".to_owned(), |c| {
                format!("copy:{}@{c}", location.source)
            }),
            digest: Some(tree),
            matches: Some(matches.into()),
        },
    );
    let at = |commit: &str| format!("{}@{}", location.source, short(commit));
    match &origin {
        None => builder.find(
            "provenance-gap",
            "copy-origin-unknown",
            &copy.package,
            &copy.consumer,
            &copy.path,
            "open",
            format!(
                "copy `{}` records no origin commit it can be checked against",
                copy.name
            ),
        ),
        Some(commit) if !modified.is_empty() => builder.find(
            "source-drift",
            "modified-copy",
            &copy.package,
            &copy.consumer,
            &copy.path,
            "violated",
            format!(
                "copy `{}` claims {} but {} differ from it: an unexplained modification",
                copy.name,
                at(commit),
                modified.join(", ")
            ),
        ),
        Some(commit) if !stale.is_empty() => {
            let detail = format!(
                "copy `{}` is {} exactly; the stack locks {}, where {} differ",
                copy.name,
                at(commit),
                short(&owner.locked.commit),
                stale.join(", ")
            );
            match origin_reason {
                Some(reason) => {
                    builder.findings.push(Finding {
                        kind: "source-drift".into(),
                        check: "stale-copy".into(),
                        package: copy.package.clone(),
                        consumer: copy.consumer.clone(),
                        path: copy.path.clone(),
                        status: "accepted".into(),
                        detail,
                        accepted_by: Some(format!(
                            "{}: {reason}",
                            match &copy.origin {
                                CopyOrigin::Pin(p) => p.clone(),
                                _ => copy.name.clone(),
                            }
                        )),
                    });
                }
                None => builder.find(
                    "source-drift",
                    "stale-copy",
                    &copy.package,
                    &copy.consumer,
                    &copy.path,
                    "open",
                    detail,
                ),
            }
        }
        Some(_) => {}
    }
    if !unexplained.is_empty() {
        builder.find(
            "source-drift",
            "unexplained-file",
            &copy.package,
            &copy.consumer,
            &copy.path,
            "violated",
            format!(
                "copy `{}` holds files neither mapped to {} nor declared local: {}",
                copy.name,
                location.source,
                unexplained.join(", ")
            ),
        );
    }
    Ok(())
}

/// What an upgrade of `package` to `target` leaves to do, consumer by consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpgradePlan {
    pub package: String,
    pub target: String,
    pub canonical: Canonical,
    pub consumers: Vec<ConsumerPlan>,
    pub at_target: usize,
    pub outstanding: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConsumerPlan {
    pub consumer: String,
    /// `at-target`, `outstanding`, `accepted` (a declared divergence holds it), or `manual` (a copy with local files).
    pub status: String,
    pub actions: Vec<String>,
}

/// The consumers of `package` and what moving each to `target` (a version, or
/// for a copy a canonical commit) would change. It changes nothing.
pub fn upgrade_plan(
    inventory: &Inventory,
    stack: &Stack,
    package: &str,
    target: &str,
) -> Option<UpgradePlan> {
    let entry = inventory.packages.iter().find(|p| p.package == package)?;
    let name = package.split_once(':').map_or(package, |(_, n)| n);
    let mut consumers: BTreeMap<String, Vec<(bool, String, bool)>> = BTreeMap::new();
    for used in &entry.uses {
        if used.consumer == entry.canonical.source && used.kind != "resolved" {
            continue;
        }
        let at_target = match used.kind.as_str() {
            "copy" => {
                used.version.as_deref() == Some(target)
                    || used.matches.as_deref() == Some("canonical-locked")
                        && entry.canonical.commit.starts_with(target)
            }
            "declared" => match &used.specifier {
                Some(spec) if used.origin == "registry" => {
                    let ok = if package.starts_with("cargo:") {
                        cargo_satisfies(spec, target)
                    } else {
                        range_satisfies(spec, target)
                    };
                    ok == Some(true)
                }
                _ => true,
            },
            _ => used.version.as_deref().is_none_or(|v| v == target),
        };
        let accepted = inventory.findings.iter().any(|f| {
            f.status == "accepted"
                && f.package == package
                && f.consumer == used.consumer
                && used.path.starts_with(f.path.as_str())
                && !f.path.is_empty()
        });
        let action = match used.kind.as_str() {
            "declared" => format!(
                "{}: {} {name} {} → admit {target}",
                used.path,
                used.scope,
                used.specifier.clone().unwrap_or_default()
            ),
            "resolved" => format!(
                "{}: re-resolve {name} {} → {target} with the consumer's own package manager",
                used.path,
                used.version.clone().unwrap_or_default()
            ),
            "installed" => format!(
                "{}: committed install {} → reinstall {target}",
                used.path,
                used.version.clone().unwrap_or_default()
            ),
            _ => {
                let copy = stack
                    .copies
                    .iter()
                    .find(|c| c.consumer == used.consumer && c.path == used.path);
                match copy {
                    Some(c) if !c.local.is_empty() => format!(
                        "{}: re-copy {} from {}@{target}; keep its local files ({}) and review them by hand",
                        used.path,
                        c.map
                            .iter()
                            .map(|(h, t)| format!("{h}←{t}"))
                            .collect::<Vec<_>>()
                            .join(", "),
                        entry.canonical.source,
                        c.local.join(", ")
                    ),
                    Some(c) => format!(
                        "{}: re-copy {} from {}@{target}",
                        used.path,
                        c.map
                            .iter()
                            .map(|(h, t)| format!("{h}←{t}"))
                            .collect::<Vec<_>>()
                            .join(", "),
                        entry.canonical.source
                    ),
                    None => format!(
                        "{}: an undeclared copy ({}) — declare its origin before upgrading it",
                        used.path,
                        used.version.clone().unwrap_or_default()
                    ),
                }
            }
        };
        let manual = used.kind == "copy";
        consumers.entry(used.consumer.clone()).or_default().push((
            at_target,
            action,
            accepted || manual,
        ));
    }
    let mut plans = vec![];
    for (consumer, items) in consumers {
        let pending: Vec<&(bool, String, bool)> = items.iter().filter(|(ok, _, _)| !ok).collect();
        let status = if pending.is_empty() {
            "at-target"
        } else if pending.iter().all(|(_, _, held)| *held)
            && pending
                .iter()
                .any(|(_, a, _)| a.contains("re-copy") || a.contains("undeclared copy"))
        {
            "manual"
        } else if inventory
            .findings
            .iter()
            .any(|f| f.package == package && f.consumer == consumer && f.status == "accepted")
            && pending.iter().all(|(_, _, held)| *held)
        {
            "accepted"
        } else {
            "outstanding"
        };
        plans.push(ConsumerPlan {
            consumer,
            status: status.into(),
            actions: pending.into_iter().map(|(_, a, _)| a.clone()).collect(),
        });
    }
    let at_target = plans.iter().filter(|p| p.status == "at-target").count();
    Some(UpgradePlan {
        package: package.into(),
        target: target.into(),
        canonical: entry.canonical.clone(),
        outstanding: plans.len() - at_target,
        at_target,
        consumers: plans,
    })
}
