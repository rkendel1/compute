//! The stack lock: one exact, resolved revision of a stack.
//!
//! A stack (`stack.toml`) holds policy: components at version constraints,
//! the sources they are assembled from, compatibility relationships, and the
//! acceptance suites the stack requires. A lock records what that policy
//! resolved to at one moment: each source's exact commit and tree, the digests
//! of its own lockfiles and toolchain files, each package's exact published
//! version and registry integrity, and whether every relationship held.
//!
//! The lock is evidence of what was resolved, not proof that the stack works:
//! acceptance results are recorded separately, against the lock's digest.
//! It holds no timestamps, paths, or machine facts, so resolving the same
//! inputs twice produces the same bytes and the same digest.
//!
//! A published package's claimed source commit (npm `gitHead`) is recorded as
//! a claim. The lock says whether that commit exists in the declared source;
//! it never says the artifact was built from it.

use std::collections::{BTreeMap, BTreeSet};

use compute_core::{ComponentSource, version_satisfies};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{FailureKind, ProjectError};
use crate::stack::{Relationship, Stack, StackSource, pin_pattern};
use crate::stack_inventory::{Finding, Inventory, TreeEntry, inventory};

pub const LOCK_FORMAT: &str = "compute.stack-lock@1";
pub const LOCK_FILE: &str = "stack.lock";

/// Files whose digest a source records when it holds them at its root: the
/// lockfiles and toolchain declarations its own package managers own.
pub const RECORDED_FILES: &[&str] = &[
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    "rust-toolchain",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "go.mod",
    "go.sum",
    ".nvmrc",
    ".node-version",
];

/// What a registry says about one published version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedPackage {
    pub integrity: String,
    /// The commit the publisher recorded (npm `gitHead`), if any.
    pub git_head: Option<String>,
    /// Its declared runtime dependencies.
    pub dependencies: BTreeMap<String, String>,
}

/// Everything resolution needs from the outside world.
pub trait StackResolver {
    /// A branch, tag, or full commit of `repository` → the commit it names.
    fn resolve_reference(&self, repository: &str, reference: &str) -> Result<String, String>;
    /// The commit's tree.
    fn tree(&self, repository: &str, commit: &str) -> Result<String, String>;
    /// A file at a commit; `None` when the commit has no such file.
    fn read_file(
        &self,
        repository: &str,
        commit: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, String>;
    /// Whether `commit` exists in `repository`.
    fn has_commit(&self, repository: &str, commit: &str) -> Result<bool, String>;
    /// Every blob, tree, and submodule of a commit, recursively.
    fn list_tree(&self, repository: &str, commit: &str) -> Result<Vec<TreeEntry>, String>;
    /// Paths that differ between two commits.
    fn changed_paths(&self, repository: &str, from: &str, to: &str) -> Result<Vec<String>, String>;
    /// Every published version of a package.
    fn package_versions(&self, package: &str) -> Result<Vec<String>, String>;
    fn package(&self, package: &str, version: &str) -> Result<PublishedPackage, String>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedStack {
    pub name: String,
    pub version: String,
    /// The runtime fingerprint receipts carry.
    pub fingerprint: String,
    /// The canonical assembly digest (`Stack::assembly_digest`).
    pub assembly: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedSource {
    pub name: String,
    pub repository: String,
    /// `track:<branch>` or `ref:<reference>`, as the manifest (or a candidate override) asked.
    pub requested: String,
    pub commit: String,
    pub tree: String,
    /// Root lockfiles and toolchain files → `sha256:<hex>`.
    pub files: BTreeMap<String, String>,
    /// Toolchain requirements the source itself declares.
    pub toolchains: BTreeMap<String, String>,
    /// `record` values: `<path>#<pointer>` → the value at that commit.
    pub recorded: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPackage {
    pub component: String,
    pub package: String,
    pub constraint: String,
    pub version: String,
    pub integrity: String,
    /// The source commit the publisher claims, and what the lock could check of it.
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// `no-claim`, `claim-unchecked` (no declared source provides it),
    /// `claimed-commit-in-source`, or `claimed-commit-not-in-source`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationshipResult {
    pub name: String,
    pub kind: String,
    /// `holds`, `accepted` (a declared, reasoned exception), `drift`
    /// (resolvable by a release or pin update), or `violated`.
    pub status: String,
    pub detail: String,
}

/// One resolved stack revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackLock {
    pub format: String,
    pub stack: LockedStack,
    pub sources: Vec<LockedSource>,
    pub packages: Vec<LockedPackage>,
    pub relationships: Vec<RelationshipResult>,
    /// What every source holds of every provided package, and where they disagree.
    #[serde(default)]
    pub inventory: Inventory,
}

fn lock_error(message: impl Into<String>) -> ProjectError {
    ProjectError::new(FailureKind::StackInvalid, message)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

impl StackLock {
    /// The canonical bytes: pretty JSON with sorted keys and a final newline.
    /// Writing and digesting use these bytes only.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let value = serde_json::to_value(self).expect("a lock serializes");
        let mut bytes = serde_json::to_vec_pretty(&sort(value)).expect("a lock serializes");
        bytes.push(b'\n');
        bytes
    }

    pub fn digest(&self) -> String {
        sha256_hex(&self.canonical_bytes())
    }

    /// A lock file's contents; refused unless they are exactly the canonical form.
    pub fn parse(bytes: &[u8]) -> Result<Self, ProjectError> {
        let lock: StackLock = serde_json::from_slice(bytes)
            .map_err(|error| lock_error(format!("stack lock: {error}")))?;
        if lock.format != LOCK_FORMAT {
            return Err(lock_error(format!(
                "unsupported stack lock format `{}`",
                lock.format
            )));
        }
        if lock.canonical_bytes() != bytes {
            return Err(lock_error(
                "the stack lock is not in canonical form: it was edited by hand or by another tool; regenerate it",
            ));
        }
        Ok(lock)
    }

    pub fn source(&self, name: &str) -> Option<&LockedSource> {
        self.sources.iter().find(|source| source.name == name)
    }

    pub fn package(&self, package: &str) -> Option<&LockedPackage> {
        self.packages
            .iter()
            .find(|locked| locked.package == package)
    }

    /// Relationships that block promotion: violations. Drift and accepted
    /// exceptions are reported, not blocking.
    pub fn violations(&self) -> Vec<&RelationshipResult> {
        self.relationships
            .iter()
            .filter(|result| result.status == "violated")
            .collect()
    }

    /// Inventory findings that block promotion.
    pub fn violated_findings(&self) -> Vec<&Finding> {
        self.inventory
            .findings
            .iter()
            .filter(|finding| finding.status == "violated")
            .collect()
    }
}

fn sort(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: BTreeMap<_, _> = map.into_iter().map(|(k, v)| (k, sort(v))).collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sort).collect())
        }
        other => other,
    }
}

/// What a candidate asks for instead of the manifest's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// Source name → a branch, tag, or commit.
    pub sources: BTreeMap<String, String>,
    /// Package → an exact version (it must still satisfy the constraint).
    pub packages: BTreeMap<String, String>,
}

fn parse_semver(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// The highest release (no prerelease) satisfying `constraint`.
pub fn highest_satisfying(constraint: &str, versions: &[String]) -> Option<String> {
    versions
        .iter()
        .filter(|version| parse_semver(version).is_some())
        .filter(|version| version_satisfies(constraint, version) == Some(true))
        .max_by_key(|version| parse_semver(version))
        .cloned()
}

fn json_pointer(bytes: &[u8], pointer: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    match value.pointer(pointer)? {
        serde_json::Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// The toolchain requirements a source states in the files it holds.
fn toolchains(files: &BTreeMap<String, Vec<u8>>) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    for name in ["rust-toolchain.toml", "rust-toolchain"] {
        if let Some(bytes) = files.get(name) {
            let text = String::from_utf8_lossy(bytes);
            let channel = text
                .lines()
                .find_map(|line| {
                    let line = line.trim();
                    line.strip_prefix("channel")
                        .and_then(|rest| rest.trim().strip_prefix('='))
                        .map(|value| value.trim().trim_matches('"').to_owned())
                })
                .unwrap_or_else(|| text.trim().to_owned());
            found.insert("rust".to_owned(), channel);
        }
    }
    if let Some(bytes) = files.get("package.json") {
        if let Some(node) = json_pointer(bytes, "/engines/node") {
            found.insert("node".to_owned(), node);
        }
        if let Some(manager) = json_pointer(bytes, "/packageManager") {
            found.insert("package-manager".to_owned(), manager);
        }
    }
    for name in [".nvmrc", ".node-version"] {
        if let Some(bytes) = files.get(name) {
            found
                .entry("node".to_owned())
                .or_insert_with(|| String::from_utf8_lossy(bytes).trim().to_owned());
        }
    }
    if let Some(bytes) = files.get("go.mod") {
        let text = String::from_utf8_lossy(bytes);
        if let Some(go) = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("go ").map(str::to_owned))
        {
            found.insert("go".to_owned(), go.trim().to_owned());
        }
    }
    found
}

fn resolve_source(
    source: &StackSource,
    override_reference: Option<&str>,
    resolver: &dyn StackResolver,
) -> Result<LockedSource, ProjectError> {
    let (requested, reference) = match (override_reference, &source.track, &source.reference) {
        (Some(reference), _, _) => (format!("candidate:{reference}"), reference.to_owned()),
        (None, Some(branch), _) => (format!("track:{branch}"), branch.clone()),
        (None, None, Some(reference)) => (format!("ref:{reference}"), reference.clone()),
        (None, None, None) => unreachable!("a validated source has track or ref"),
    };
    let failed = |what: &str, error: String| {
        lock_error(format!("source `{}`: {what}: {error}", source.name))
            .found("repository", source.repository.clone())
    };
    let commit = resolver
        .resolve_reference(&source.repository, &reference)
        .map_err(|error| failed(&format!("`{reference}` did not resolve"), error))?;
    let tree = resolver
        .tree(&source.repository, &commit)
        .map_err(|error| failed("the tree could not be read", error))?;
    let mut contents = BTreeMap::new();
    for name in RECORDED_FILES {
        if let Some(bytes) = resolver
            .read_file(&source.repository, &commit, name)
            .map_err(|error| failed(&format!("{name} could not be read"), error))?
        {
            contents.insert((*name).to_owned(), bytes);
        }
    }
    let mut recorded = BTreeMap::new();
    for record in &source.record {
        let (path, pointer) = record.split_once('#').expect("validated");
        let value = resolver
            .read_file(&source.repository, &commit, path)
            .map_err(|error| failed(&format!("{path} could not be read"), error))?
            .and_then(|bytes| json_pointer(&bytes, pointer))
            .ok_or_else(|| {
                lock_error(format!(
                    "source `{}` has no `{record}` at {commit}",
                    source.name
                ))
            })?;
        recorded.insert(record.clone(), value);
    }
    Ok(LockedSource {
        name: source.name.clone(),
        repository: source.repository.clone(),
        requested,
        commit,
        tree,
        files: contents
            .iter()
            .map(|(name, bytes)| (name.clone(), sha256_hex(bytes)))
            .collect(),
        toolchains: toolchains(&contents),
        recorded,
        optional: source.optional,
    })
}

/// Resolve `stack` (with a candidate's `overrides`) into a lock.
pub fn resolve(
    stack: &Stack,
    overrides: &Overrides,
    resolver: &dyn StackResolver,
) -> Result<StackLock, ProjectError> {
    for name in overrides.sources.keys() {
        if stack.source(name).is_none() {
            return Err(lock_error(format!(
                "`{name}` is not a source of this stack"
            )));
        }
    }
    for package in overrides.packages.keys() {
        if stack.package(package).is_none() {
            return Err(lock_error(format!(
                "`{package}` is not a package of this stack"
            )));
        }
    }
    let sources = stack
        .sources
        .iter()
        .map(|source| {
            resolve_source(
                source,
                overrides.sources.get(&source.name).map(String::as_str),
                resolver,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut packages = vec![];
    let mut published = BTreeMap::new();
    for component in &stack.components {
        let ComponentSource::Package {
            package,
            constraint,
            ..
        } = &component.source;
        let version = match overrides.packages.get(package) {
            Some(version) => {
                if version_satisfies(constraint, version) != Some(true) {
                    return Err(lock_error(format!(
                        "candidate version {version} of {package} is outside the stack's constraint `{constraint}`: change the manifest to widen it"
                    )));
                }
                version.clone()
            }
            None => {
                let versions = resolver
                    .package_versions(package)
                    .map_err(|error| lock_error(format!("{package}: {error}")))?;
                highest_satisfying(constraint, &versions).ok_or_else(|| {
                    lock_error(format!(
                        "no published version of {package} satisfies `{constraint}`"
                    ))
                })?
            }
        };
        let meta = resolver
            .package(package, &version)
            .map_err(|error| lock_error(format!("{package}@{version}: {error}")))?;
        let providing = stack.canonical(&format!("npm:{package}"));
        let provenance = match (&meta.git_head, providing) {
            (None, _) => Provenance {
                status: "no-claim".into(),
                claimed_commit: None,
                source: providing.map(|s| s.name.clone()),
            },
            (Some(head), None) => Provenance {
                status: "claim-unchecked".into(),
                claimed_commit: Some(head.clone()),
                source: None,
            },
            (Some(head), Some(source)) => {
                let present = resolver
                    .has_commit(&source.repository, head)
                    .map_err(|error| lock_error(format!("{package}: {error}")))?;
                Provenance {
                    status: if present {
                        "claimed-commit-in-source"
                    } else {
                        "claimed-commit-not-in-source"
                    }
                    .into(),
                    claimed_commit: Some(head.clone()),
                    source: Some(source.name.clone()),
                }
            }
        };
        packages.push(LockedPackage {
            component: component.name.clone(),
            package: package.clone(),
            constraint: constraint.clone(),
            version,
            integrity: meta.integrity.clone(),
            provenance,
        });
        published.insert(package.clone(), meta);
    }
    packages.sort_by(|a, b| a.package.cmp(&b.package));

    let relationships = evaluate(stack, &sources, &packages, &published, resolver)?;
    let versions = packages
        .iter()
        .map(|p| (p.package.clone(), p.version.clone()))
        .collect();
    let unclaimed = packages
        .iter()
        .filter(|p| p.provenance.status == "no-claim")
        .map(|p| p.package.clone())
        .collect();
    let inventory = inventory(stack, &sources, &versions, &unclaimed, resolver)?;
    Ok(StackLock {
        format: LOCK_FORMAT.into(),
        stack: LockedStack {
            name: stack.name.clone(),
            version: stack.version.clone(),
            fingerprint: stack.identity().fingerprint,
            assembly: stack.assembly_digest(),
        },
        sources,
        packages,
        relationships,
        inventory,
    })
}

fn evaluate(
    stack: &Stack,
    sources: &[LockedSource],
    packages: &[LockedPackage],
    published: &BTreeMap<String, PublishedPackage>,
    resolver: &dyn StackResolver,
) -> Result<Vec<RelationshipResult>, ProjectError> {
    let locked_version = |package: &str| {
        packages
            .iter()
            .find(|p| p.package == package)
            .map(|p| p.version.clone())
    };
    let source = |name: &str| sources.iter().find(|s| s.name == name).expect("validated");
    let mut results = vec![];
    let mut declared_pairs = BTreeSet::new();
    for relationship in &stack.relationships {
        let result = match relationship {
            Relationship::PackageDependency {
                name,
                package,
                dependency,
                nested_allowed,
                reason,
            } => {
                declared_pairs.insert((package.clone(), dependency.clone()));
                let wanted = published[package].dependencies.get(dependency).cloned();
                let stack_version = locked_version(dependency).expect("validated");
                let (status, detail) = match wanted {
                    None => (
                        "holds",
                        format!("{package} does not depend on {dependency}"),
                    ),
                    Some(spec) if version_satisfies(&spec, &stack_version) == Some(true) => (
                        "holds",
                        format!(
                            "{package} wants {dependency} {spec}; the stack has {stack_version}"
                        ),
                    ),
                    Some(spec) if *nested_allowed => (
                        "accepted",
                        format!(
                            "{package} wants {dependency} {spec}; the stack has {stack_version}; a nested copy is accepted: {}",
                            reason.clone().unwrap_or_default()
                        ),
                    ),
                    Some(spec) => (
                        "violated",
                        format!(
                            "{package} wants {dependency} {spec}; the stack has {stack_version}"
                        ),
                    ),
                };
                RelationshipResult {
                    name: name.clone(),
                    kind: "package-dependency".into(),
                    status: status.into(),
                    detail,
                }
            }
            Relationship::PublishedFrom {
                name,
                package,
                source: source_name,
                path,
            } => {
                let locked = source(source_name);
                let stack_version = locked_version(package).expect("validated");
                let source_version = resolver
                    .read_file(&locked.repository, &locked.commit, path)
                    .map_err(|error| lock_error(format!("relationship `{name}`: {error}")))?
                    .and_then(|bytes| json_pointer(&bytes, "/version"));
                let claimed = published[package].git_head.clone();
                let (status, detail) = match source_version {
                    None => (
                        "violated",
                        format!(
                            "{source_name}@{} has no version at {path}",
                            short(&locked.commit)
                        ),
                    ),
                    Some(version) if version != stack_version => (
                        "drift",
                        format!(
                            "{source_name}@{} declares {package} {version}; the stack uses the published {stack_version}: a release or a constraint change is pending",
                            short(&locked.commit)
                        ),
                    ),
                    Some(version) => match claimed {
                        Some(head) if head == locked.commit => (
                            "holds",
                            format!("{package} {version} claims {source_name}@{}", short(&head)),
                        ),
                        Some(head) => (
                            "drift",
                            format!(
                                "{source_name}@{} declares {package} {version}, the published version, but the published artifact claims {}: the source changed without a version bump",
                                short(&locked.commit),
                                short(&head)
                            ),
                        ),
                        None => (
                            "drift",
                            format!(
                                "{source_name}@{} declares {package} {version}, the published version, whose artifact records no source commit: they cannot be shown to match",
                                short(&locked.commit)
                            ),
                        ),
                    },
                };
                RelationshipResult {
                    name: name.clone(),
                    kind: "published-from".into(),
                    status: status.into(),
                    detail,
                }
            }
            Relationship::Pin {
                name,
                consumer,
                file,
                pattern,
                target,
                must_be_locked,
                reason,
            } => {
                let consumer_lock = source(consumer);
                let target_lock = source(target);
                let text = resolver
                    .read_file(&consumer_lock.repository, &consumer_lock.commit, file)
                    .map_err(|error| lock_error(format!("relationship `{name}`: {error}")))?
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
                let pinned = text.as_deref().and_then(|text| extract_pin(text, pattern));
                let (status, detail) = match pinned {
                    None => (
                        "violated",
                        format!(
                            "{consumer}@{}:{file} does not contain the pin pattern",
                            short(&consumer_lock.commit)
                        ),
                    ),
                    Some(commit) if commit == target_lock.commit => (
                        "holds",
                        format!("{consumer} pins {target} at the locked {}", short(&commit)),
                    ),
                    Some(commit) => {
                        let exists = resolver
                            .has_commit(&target_lock.repository, &commit)
                            .map_err(|error| {
                                lock_error(format!("relationship `{name}`: {error}"))
                            })?;
                        match (exists, must_be_locked) {
                            (false, _) => (
                                "violated",
                                format!(
                                    "{consumer} pins {target} at {}, which {target} does not have",
                                    short(&commit)
                                ),
                            ),
                            (true, true) => (
                                "drift",
                                format!(
                                    "{consumer} pins {target} at {}; the stack locks {}: update the pin or lock {target} there",
                                    short(&commit),
                                    short(&target_lock.commit)
                                ),
                            ),
                            (true, false) => (
                                "accepted",
                                format!(
                                    "{consumer} holds {target} at {} (locked {}): {}",
                                    short(&commit),
                                    short(&target_lock.commit),
                                    reason.clone().unwrap_or_default()
                                ),
                            ),
                        }
                    }
                };
                RelationshipResult {
                    name: name.clone(),
                    kind: "pin".into(),
                    status: status.into(),
                    detail,
                }
            }
            Relationship::Consumes {
                name,
                consumer,
                provider,
                contract,
            } => RelationshipResult {
                name: name.clone(),
                kind: "consumes".into(),
                status: "holds".into(),
                detail: format!("{consumer} consumes {provider}: {contract}"),
            },
        };
        results.push(result);
    }
    // Every published dependency of one stack package on another is checked,
    // declared or not: an undeclared mismatch is a violation.
    for (package, meta) in published {
        for (dependency, spec) in &meta.dependencies {
            if declared_pairs.contains(&(package.clone(), dependency.clone())) {
                continue;
            }
            let Some(stack_version) = locked_version(dependency) else {
                continue;
            };
            if version_satisfies(spec, &stack_version) != Some(true) {
                results.push(RelationshipResult {
                    name: format!("undeclared:{package}->{dependency}"),
                    kind: "package-dependency".into(),
                    status: "violated".into(),
                    detail: format!(
                        "{package} wants {dependency} {spec}; the stack has {stack_version}; no relationship declares whether a nested copy is acceptable"
                    ),
                });
            }
        }
    }
    Ok(results)
}

pub fn extract_pin(text: &str, pattern: &str) -> Option<String> {
    let (before, after) = pin_pattern(pattern)?;
    let start = text.find(before)? + before.len();
    let rest = &text[start..];
    let end = if after.is_empty() {
        rest.find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(rest.len())
    } else {
        rest.find(after)?
    };
    let commit = &rest[..end];
    (commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| commit.to_ascii_lowercase())
}

pub fn short(commit: &str) -> &str {
    &commit[..commit.len().min(7)]
}

/// How two locks differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LockDiff {
    pub from: String,
    pub to: String,
    pub manifest_changed: bool,
    pub sources: Vec<SourceChange>,
    pub packages: Vec<PackageChange>,
    pub relationships: Vec<RelationshipChange>,
    /// Inventory findings that appeared, disappeared, or changed status.
    pub findings: Vec<FindingChange>,
    /// Sources changed, and every source that consumes one of them.
    pub affected: Vec<String>,
    /// Acceptance suites that cover an affected source or a changed package.
    pub suites: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceChange {
    pub name: String,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Files whose recorded digest or value changed.
    pub lockfiles: Vec<String>,
    pub toolchains: Vec<String>,
    pub recorded: Vec<String>,
    /// Changed paths under the source's declared contract prefixes.
    pub contracts: Vec<String>,
    pub changed_paths: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageChange {
    pub package: String,
    pub from: Option<String>,
    pub to: Option<String>,
    /// The same version with a different integrity: the registry changed an artifact.
    pub integrity_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RelationshipChange {
    pub name: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FindingChange {
    pub kind: String,
    pub check: String,
    pub package: String,
    pub consumer: String,
    pub path: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub detail: String,
}

impl LockDiff {
    pub fn is_empty(&self) -> bool {
        !self.manifest_changed
            && self.sources.is_empty()
            && self.packages.is_empty()
            && self.relationships.is_empty()
            && self.findings.is_empty()
    }
}

fn changed_keys(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> Vec<String> {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter(|key| a.get(*key) != b.get(*key))
        .cloned()
        .collect()
}

/// Explain `to` against `from`. With a resolver, changed source paths are
/// classified against the stack's contract prefixes.
pub fn diff(
    stack: &Stack,
    from: &StackLock,
    to: &StackLock,
    resolver: Option<&dyn StackResolver>,
) -> LockDiff {
    let mut sources = vec![];
    let names: BTreeSet<&str> = from
        .sources
        .iter()
        .chain(&to.sources)
        .map(|s| s.name.as_str())
        .collect();
    for name in names {
        let (a, b) = (from.source(name), to.source(name));
        if a == b {
            continue;
        }
        let empty = BTreeMap::new();
        let (af, bf) = (
            a.map_or(&empty, |s| &s.files),
            b.map_or(&empty, |s| &s.files),
        );
        let (at, bt) = (
            a.map_or(&empty, |s| &s.toolchains),
            b.map_or(&empty, |s| &s.toolchains),
        );
        let (ar, br) = (
            a.map_or(&empty, |s| &s.recorded),
            b.map_or(&empty, |s| &s.recorded),
        );
        let mut contracts = vec![];
        let mut changed_paths = 0;
        if let (Some(a), Some(b), Some(resolver)) = (a, b, resolver)
            && a.commit != b.commit
            && let Ok(paths) = resolver.changed_paths(&b.repository, &a.commit, &b.commit)
        {
            let prefixes = stack
                .source(name)
                .map(|s| s.contracts.clone())
                .unwrap_or_default();
            let paths: Vec<String> = match stack.source(name) {
                Some(declared) => paths.into_iter().filter(|p| declared.reads(p)).collect(),
                None => paths,
            };
            changed_paths = paths.len();
            contracts = paths
                .into_iter()
                .filter(|path| {
                    prefixes
                        .iter()
                        .any(|prefix| path.starts_with(prefix.as_str()))
                })
                .collect();
        }
        sources.push(SourceChange {
            name: name.to_owned(),
            from: a.map(|s| s.commit.clone()),
            to: b.map(|s| s.commit.clone()),
            lockfiles: changed_keys(af, bf),
            toolchains: changed_keys(at, bt),
            recorded: changed_keys(ar, br),
            contracts,
            changed_paths,
        });
    }
    let mut packages = vec![];
    let names: BTreeSet<&str> = from
        .packages
        .iter()
        .chain(&to.packages)
        .map(|p| p.package.as_str())
        .collect();
    for name in names {
        let (a, b) = (from.package(name), to.package(name));
        if a == b {
            continue;
        }
        packages.push(PackageChange {
            package: name.to_owned(),
            from: a.map(|p| p.version.clone()),
            to: b.map(|p| p.version.clone()),
            integrity_changed: matches!((a, b), (Some(a), Some(b)) if a.version == b.version && a.integrity != b.integrity),
        });
    }
    let mut relationships = vec![];
    let find =
        |lock: &StackLock, name: &str| lock.relationships.iter().find(|r| r.name == name).cloned();
    let names: BTreeSet<String> = from
        .relationships
        .iter()
        .chain(&to.relationships)
        .map(|r| r.name.clone())
        .collect();
    for name in names {
        let (a, b) = (find(from, &name), find(to, &name));
        if a.as_ref().map(|r| (&r.status, &r.detail)) == b.as_ref().map(|r| (&r.status, &r.detail))
        {
            continue;
        }
        relationships.push(RelationshipChange {
            detail: b
                .as_ref()
                .or(a.as_ref())
                .map(|r| r.detail.clone())
                .unwrap_or_default(),
            from: a.map(|r| r.status),
            to: b.map(|r| r.status),
            name,
        });
    }

    let mut findings = vec![];
    let index = |lock: &StackLock| {
        lock.inventory
            .findings
            .iter()
            .map(|f| (f.key(), f.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    let (before, after) = (index(from), index(to));
    let keys: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    for key in keys {
        let (a, b) = (before.get(&key), after.get(&key));
        if a.map(|f| (&f.status, &f.detail)) == b.map(|f| (&f.status, &f.detail)) {
            continue;
        }
        let (kind, check, package, consumer, path) = key;
        findings.push(FindingChange {
            kind,
            check,
            package,
            consumer,
            path,
            from: a.map(|f| f.status.clone()),
            to: b.map(|f| f.status.clone()),
            detail: b.or(a).map(|f| f.detail.clone()).unwrap_or_default(),
        });
    }

    // Impact: the changed sources (ignoring changes only under their ignored
    // paths), then everything that consumes them.
    let mut affected: BTreeSet<String> = sources
        .iter()
        .filter(|s| {
            s.from.is_none()
                || s.to.is_none()
                || s.changed_paths > 0
                || resolver.is_none()
                || !s.lockfiles.is_empty()
                || !s.recorded.is_empty()
        })
        .map(|s| s.name.clone())
        .collect();
    loop {
        let before = affected.len();
        for relationship in &stack.relationships {
            if let Relationship::Consumes {
                consumer, provider, ..
            } = relationship
                && affected.contains(provider)
            {
                affected.insert(consumer.clone());
            }
        }
        if affected.len() == before {
            break;
        }
    }
    let changed_components: BTreeSet<String> = packages
        .iter()
        .filter_map(|change| {
            stack
                .package(&change.package)
                .map(|(component, _)| component.name.clone())
        })
        .collect();
    let manifest_changed = from.stack != to.stack;
    let suites = stack
        .acceptance
        .iter()
        .filter(|suite| {
            manifest_changed
                || suite.covers.iter().any(|covered| {
                    affected.contains(covered) || changed_components.contains(covered)
                })
        })
        .map(|suite| suite.name.clone())
        .collect();
    LockDiff {
        from: from.digest(),
        to: to.digest(),
        manifest_changed,
        sources,
        packages,
        relationships,
        findings,
        affected: affected.into_iter().collect(),
        suites,
    }
}
