//! Stacks: declarative, versioned desired environments.
//!
//! A stack lists components (packages at version constraints), the runtime
//! they need, and the *names* of credentials they reference. It contains no
//! commands and no secrets. A stack does not install anything: its
//! requirements join the project's (`Stack::apply`), flow through the same
//! planning, placement, and materialization as any other requirement, and
//! its components are resolved against the dependency capsule the target
//! will materialize.
//!
//! No stack is special: `randy` is `stacks/randy/stack.toml`, data that
//! Compute reads exactly as it reads any other.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use compute_core::{
    ComponentBinding, ComponentDeclaration, ComponentSource, DependencyCapsule, DependencyGroup,
    DependencyNeed, ProjectRequirements, ResolvedComponent, RuntimeKind, RuntimeNeed,
    STACK_VERSION, StackBinding, StackIdentity, is_version, version_satisfies,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{FailureKind, ProjectError};

/// The manifest file a stack directory holds.
pub const STACK_FILE: &str = "stack.toml";
/// Where extra stacks are looked up, besides `./stacks` (`:`-separated
/// directories that contain `<name>/stack.toml`).
pub const STACKS_ENV: &str = "COMPUTE_STACKS";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    stack: Header,
    #[serde(default)]
    requirements: Option<Requirements>,
    #[serde(default, rename = "component")]
    components: Vec<ManifestComponent>,
    #[serde(default, rename = "source")]
    sources: Vec<ManifestSource>,
    #[serde(default, rename = "relationship")]
    relationships: Vec<ManifestRelationship>,
    #[serde(default, rename = "acceptance")]
    acceptance: Vec<ManifestAcceptance>,
    #[serde(default, rename = "copy")]
    copies: Vec<ManifestCopy>,
    #[serde(default, rename = "divergence")]
    divergences: Vec<ManifestDivergence>,
}

/// Where a part of the stack is built from: a canonical repository, followed
/// at a branch (`track`) or held at an exact `ref`. Sources describe how the
/// stack is *assembled*; they are never materialized on a Computer.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestSource {
    name: String,
    /// `github:<owner>/<repository>` (or `file:<absolute path>` for a local mirror).
    repository: String,
    #[serde(default)]
    track: Option<String>,
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    /// `required` (the default) or `optional`.
    #[serde(default)]
    role: Option<String>,
    /// Packages whose canonical source is this repository (`npm:<package>`,
    /// `cargo:<crate>`).
    #[serde(default)]
    provides: Vec<String>,
    /// Versions to record from the source: `<path>#<json pointer>` into a JSON file.
    #[serde(default)]
    record: Vec<String>,
    /// Path prefixes that hold contracts, schemas, or protocols consumers depend on.
    #[serde(default)]
    contracts: Vec<String>,
    /// Path prefixes the inventory and impact analysis do not read (for
    /// example the stack's own lock history inside the harness repository).
    #[serde(default)]
    ignore: Vec<String>,
    /// A file in this source stating which version each consumer of a package
    /// it provides is expected to run (`feltdb.compatibility-manifest.v1`).
    #[serde(default)]
    expectations: Option<String>,
}

/// A copy of a provided package's source kept inside another source
/// (vendored, copied, or embedded), and how it maps onto the canonical one.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestCopy {
    name: String,
    /// `npm:<package>` or `cargo:<crate>`, provided by a declared source.
    package: String,
    consumer: String,
    /// The copy's directory in the consumer.
    path: String,
    /// A `pin` relationship that records the canonical commit the copy was taken from.
    #[serde(default)]
    origin: Option<String>,
    /// Or that commit, written here.
    #[serde(default)]
    commit: Option<String>,
    /// `<path in the copy>=<path in the canonical source>`: these must match the
    /// canonical source at the origin commit exactly.
    #[serde(default)]
    map: Vec<String>,
    /// Paths in the copy written for it, not taken from the canonical source.
    #[serde(default)]
    local: Vec<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// A difference between a consumer and its canonical package that is
/// intended, with the reason a reviewer can check.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestDivergence {
    name: String,
    /// `npm:<package>` / `cargo:<crate>`, or a prefix ending in `*` (`npm:@appport/*`).
    package: String,
    #[serde(default)]
    consumer: Option<String>,
    /// The finding kind accepted; every kind when absent.
    #[serde(default)]
    check: Option<String>,
    /// Only findings at or under this path in the consumer.
    #[serde(default)]
    path: Option<String>,
    reason: String,
}

/// A compatibility constraint, checked when the stack is locked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestRelationship {
    name: String,
    kind: String,
    #[serde(default)]
    package: Option<String>,
    #[serde(default)]
    dependency: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    consumer: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    contract: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    policy: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// An acceptance suite the stack requires, by name. Its implementation lives
/// with the tooling (`compatibility/stack/<name>`), never in the manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestAcceptance {
    name: String,
    /// Source or component names whose change makes this suite relevant.
    covers: Vec<String>,
    /// `deterministic`, `live`, `replay`, `mock`, or `model`.
    mode: String,
    #[serde(default = "mandatory_default")]
    mandatory: bool,
    #[serde(default)]
    description: String,
}

fn mandatory_default() -> bool {
    true
}

/// A validated source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StackSource {
    pub name: String,
    pub repository: String,
    /// The branch a candidate follows, or `None` for a fixed `reference`.
    pub track: Option<String>,
    pub reference: Option<String>,
    pub optional: bool,
    pub provides: Vec<String>,
    pub record: Vec<String>,
    pub contracts: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expectations: Option<String>,
}

impl StackSource {
    /// Whether the inventory and impact analysis read `path`.
    pub fn reads(&self, path: &str) -> bool {
        !self
            .ignore
            .iter()
            .any(|prefix| path == prefix.trim_end_matches('/') || path.starts_with(prefix.as_str()))
    }
}

/// Where a copy says it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CopyOrigin {
    /// The commit a `pin` relationship extracts.
    Pin(String),
    Commit(String),
    /// Not declared: a provenance gap.
    Undeclared,
}

/// A validated copy declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceCopy {
    pub name: String,
    pub package: String,
    pub consumer: String,
    pub path: String,
    pub origin: CopyOrigin,
    pub map: Vec<(String, String)>,
    pub local: Vec<String>,
    pub reason: Option<String>,
}

/// The kinds of inventory finding a divergence may accept.
pub const FINDING_KINDS: &[&str] = &[
    "version-drift",
    "source-drift",
    "resolution-drift",
    "compatibility-drift",
    "provenance-gap",
];

/// A validated, reasoned divergence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Divergence {
    pub name: String,
    pub package: String,
    pub consumer: Option<String>,
    pub check: Option<String>,
    pub path: Option<String>,
    pub reason: String,
}

impl Divergence {
    pub fn accepts(&self, kind: &str, package: &str, consumer: &str, path: &str) -> bool {
        let package_ok = match self.package.strip_suffix('*') {
            Some(prefix) => package.starts_with(prefix),
            None => package == self.package,
        };
        package_ok
            && self.consumer.as_deref().is_none_or(|c| c == consumer)
            && self.check.as_deref().is_none_or(|c| c == kind)
            && self.path.as_deref().is_none_or(|prefix| {
                let prefix = prefix.trim_end_matches('/');
                path == prefix || path.starts_with(&format!("{prefix}/"))
            })
    }
}

/// A validated compatibility constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Relationship {
    /// A stack package's published dependency on another stack package must
    /// be the stack's version (`same`), or a separate copy is accepted (`nested`).
    PackageDependency {
        name: String,
        package: String,
        dependency: String,
        nested_allowed: bool,
        reason: Option<String>,
    },
    /// A stack package is published from a source; its version there is compared
    /// with the published one.
    PublishedFrom {
        name: String,
        package: String,
        source: String,
        path: String,
    },
    /// `consumer` pins `target` at a commit written in `file` (the first capture
    /// of `pattern`). `locked`: it must be the target's locked commit; `fixed`:
    /// it must exist in the target.
    Pin {
        name: String,
        consumer: String,
        file: String,
        pattern: String,
        target: String,
        must_be_locked: bool,
        reason: Option<String>,
    },
    /// `consumer` depends on `provider` through `contract`: a change to the
    /// provider affects the consumer.
    Consumes {
        name: String,
        consumer: String,
        provider: String,
        contract: String,
    },
}

impl Relationship {
    pub fn name(&self) -> &str {
        match self {
            Self::PackageDependency { name, .. }
            | Self::PublishedFrom { name, .. }
            | Self::Pin { name, .. }
            | Self::Consumes { name, .. } => name,
        }
    }
}

/// A validated acceptance suite declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Acceptance {
    pub name: String,
    pub covers: Vec<String>,
    pub mode: String,
    pub mandatory: bool,
    pub description: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    name: String,
    version: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Requirements {
    runtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestComponent {
    name: String,
    /// The artifact type: `package`. Applications are not stack components.
    kind: String,
    /// `<ecosystem>:<package>`, e.g. `npm:@appport/core`.
    #[serde(default)]
    source: Option<String>,
    /// The version constraint.
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    credentials: Vec<String>,
    /// `<os>-<architecture>` platforms the component is available on; empty
    /// means all.
    #[serde(default)]
    platforms: Vec<String>,
    /// Declared not realizable by Compute today, and why.
    #[serde(default)]
    unsupported: Option<String>,
}

/// One validated component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Component {
    pub name: String,
    pub source: ComponentSource,
    pub credentials: Vec<String>,
    pub platforms: Vec<String>,
    pub unsupported: Option<String>,
}

/// A validated stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    pub name: String,
    pub version: String,
    pub description: String,
    pub runtime: Option<(RuntimeKind, Option<String>)>,
    pub components: Vec<Component>,
    /// How the stack is assembled: not part of the fingerprint, which names
    /// what a Computer is configured with. The stack lock covers these.
    pub sources: Vec<StackSource>,
    pub relationships: Vec<Relationship>,
    pub acceptance: Vec<Acceptance>,
    pub copies: Vec<SourceCopy>,
    pub divergences: Vec<Divergence>,
    fingerprint: String,
}

fn invalid(message: impl Into<String>) -> ProjectError {
    ProjectError::new(FailureKind::StackInvalid, message)
}

fn valid_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 63
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !text.starts_with('-')
}

/// A relative path inside a repository: no absolute path, no `..`.
fn safe_path(text: &str) -> bool {
    let text = text.strip_suffix('/').unwrap_or(text);
    !text.is_empty()
        && !text.starts_with('/')
        && !text.split('/').any(|part| part == ".." || part.is_empty())
}

/// `npm:<package>` or `cargo:<crate>`.
pub fn package_identity(text: &str) -> bool {
    match text.split_once(':') {
        Some(("npm", name)) => !name.is_empty() && !name.contains(char::is_whitespace),
        Some(("cargo", name)) => {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        }
        _ => false,
    }
}

/// A pin pattern is literal text around one `{commit}` placeholder.
pub fn pin_pattern(pattern: &str) -> Option<(&str, &str)> {
    let (before, after) = pattern.split_once("{commit}")?;
    (!after.contains("{commit}") && !(before.is_empty() && after.is_empty()))
        .then_some((before, after))
}

fn valid_credential(text: &str) -> bool {
    text.len() <= 64
        && text.starts_with(|c: char| c.is_ascii_uppercase())
        && text
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_platform(text: &str) -> bool {
    text.split_once('-').is_some_and(|(os, arch)| {
        !os.is_empty()
            && !arch.is_empty()
            && [os, arch]
                .iter()
                .all(|part| part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
    })
}

impl Stack {
    pub fn parse(text: &str) -> Result<Self, ProjectError> {
        let manifest: Manifest =
            toml::from_str(text).map_err(|error| invalid(format!("stack.toml: {error}")))?;
        Self::from_manifest(manifest)
    }

    fn from_manifest(manifest: Manifest) -> Result<Self, ProjectError> {
        if manifest.schema != STACK_VERSION {
            return Err(
                invalid(format!("unsupported stack schema `{}`", manifest.schema))
                    .require("schema", STACK_VERSION)
                    .found("schema", manifest.schema),
            );
        }
        let header = manifest.stack;
        if !valid_name(&header.name) {
            return Err(invalid(format!(
                "stack name `{}` must be lowercase letters, digits, and `-`",
                header.name
            )));
        }
        if !is_version(&header.version) {
            return Err(invalid(format!(
                "stack version `{}` is not major.minor.patch",
                header.version
            )));
        }
        let runtime = match manifest.requirements {
            None => None,
            Some(requirements) => {
                let kind = requirements
                    .runtime
                    .parse::<RuntimeKind>()
                    .map_err(|_| invalid(format!("unknown runtime `{}`", requirements.runtime)))?;
                if let Some(version) = &requirements.version
                    && version_satisfies(version, "1.0.0").is_none()
                {
                    return Err(invalid(format!(
                        "runtime version constraint `{version}` is outside the supported grammar"
                    )));
                }
                Some((kind, requirements.version))
            }
        };
        if manifest.components.is_empty() {
            return Err(invalid("a stack declares at least one component"));
        }
        let mut seen = BTreeSet::new();
        let mut components = vec![];
        for component in manifest.components {
            if !valid_name(&component.name) {
                return Err(invalid(format!(
                    "component name `{}` must be lowercase letters, digits, and `-`",
                    component.name
                )));
            }
            if !seen.insert(component.name.clone()) {
                return Err(invalid(format!(
                    "component `{}` is declared twice",
                    component.name
                )));
            }
            let source = Self::component_source(&component)?;
            for credential in &component.credentials {
                if !valid_credential(credential) {
                    return Err(invalid(format!(
                        "component `{}` credential `{credential}` must be an UPPER_SNAKE name; stacks reference credentials by name and never hold values",
                        component.name
                    )));
                }
            }
            for platform in &component.platforms {
                if !valid_platform(platform) {
                    return Err(invalid(format!(
                        "component `{}` platform `{platform}` is not `<os>-<architecture>`",
                        component.name
                    )));
                }
            }
            let mut credentials = component.credentials;
            credentials.sort();
            credentials.dedup();
            let mut platforms = component.platforms;
            platforms.sort();
            platforms.dedup();
            components.push(Component {
                name: component.name,
                source,
                credentials,
                platforms,
                unsupported: component.unsupported,
            });
        }
        components.sort_by(|a, b| a.name.cmp(&b.name));

        // The fingerprint covers declarative contents only.
        #[derive(Serialize)]
        struct Canonical<'a> {
            schema: &'a str,
            name: &'a str,
            version: &'a str,
            runtime: &'a Option<(RuntimeKind, Option<String>)>,
            components: &'a [Component],
        }
        let bytes = serde_json::to_vec(&Canonical {
            schema: STACK_VERSION,
            name: &header.name,
            version: &header.version,
            runtime: &runtime,
            components: &components,
        })
        .map_err(|error| invalid(error.to_string()))?;
        let sources = Self::sources(manifest.sources)?;
        let relationships = Self::relationships(manifest.relationships, &components, &sources)?;
        let acceptance = Self::acceptance(manifest.acceptance, &components, &sources)?;
        let copies = Self::copies(manifest.copies, &sources, &relationships)?;
        let divergences = Self::divergences(manifest.divergences, &sources)?;
        Ok(Self {
            name: header.name,
            version: header.version,
            description: header.description,
            runtime,
            components,
            sources,
            relationships,
            acceptance,
            copies,
            divergences,
            fingerprint: format!("sha256:{:x}", Sha256::digest(bytes)),
        })
    }

    fn sources(declared: Vec<ManifestSource>) -> Result<Vec<StackSource>, ProjectError> {
        let mut seen = BTreeSet::new();
        let mut sources = vec![];
        for source in declared {
            let name = source.name;
            if !valid_name(&name) {
                return Err(invalid(format!(
                    "source name `{name}` must be lowercase letters, digits, and `-`"
                )));
            }
            if !seen.insert(name.clone()) {
                return Err(invalid(format!("source `{name}` is declared twice")));
            }
            let repository_ok = match source.repository.split_once(':') {
                Some(("github", path)) => path.split_once('/').is_some_and(|(owner, repo)| {
                    !owner.is_empty()
                        && !repo.is_empty()
                        && !repo.contains('/')
                        && [owner, repo].iter().all(|part| {
                            part.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                        })
                }),
                Some(("file", path)) => path.starts_with('/'),
                _ => false,
            };
            if !repository_ok {
                return Err(invalid(format!(
                    "source `{name}` repository `{}` is not `github:<owner>/<repository>` or `file:<absolute path>`",
                    source.repository
                )));
            }
            match (&source.track, &source.reference) {
                (Some(_), None) | (None, Some(_)) => {}
                _ => {
                    return Err(invalid(format!(
                        "source `{name}` declares exactly one of `track` (a branch) and `ref` (an exact commit or tag)"
                    )));
                }
            }
            let optional = match source.role.as_deref() {
                None | Some("required") => false,
                Some("optional") => true,
                Some(other) => {
                    return Err(invalid(format!(
                        "source `{name}` role `{other}` is not `required` or `optional`"
                    )));
                }
            };
            for provided in &source.provides {
                if !package_identity(provided) {
                    return Err(invalid(format!(
                        "source `{name}` provides `{provided}`, which is not `npm:<package>` or `cargo:<crate>`"
                    )));
                }
            }
            for prefix in &source.ignore {
                if !safe_path(prefix) {
                    return Err(invalid(format!(
                        "source `{name}` ignore path `{prefix}` is not a relative path"
                    )));
                }
            }
            if let Some(path) = &source.expectations
                && !safe_path(path)
            {
                return Err(invalid(format!(
                    "source `{name}` expectations `{path}` is not a relative path"
                )));
            }
            for record in &source.record {
                if !record
                    .split_once('#')
                    .is_some_and(|(path, pointer)| safe_path(path) && pointer.starts_with('/'))
                {
                    return Err(invalid(format!(
                        "source `{name}` record `{record}` is not `<relative path>#<json pointer>`"
                    )));
                }
            }
            for prefix in &source.contracts {
                if !safe_path(prefix) {
                    return Err(invalid(format!(
                        "source `{name}` contract path `{prefix}` is not a relative path"
                    )));
                }
            }
            let mut provides = source.provides;
            provides.sort();
            provides.dedup();
            sources.push(StackSource {
                name,
                repository: source.repository,
                track: source.track,
                reference: source.reference,
                optional,
                provides,
                record: source.record,
                contracts: source.contracts,
                ignore: source.ignore,
                expectations: source.expectations,
            });
        }
        sources.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(sources)
    }

    fn relationships(
        declared: Vec<ManifestRelationship>,
        components: &[Component],
        sources: &[StackSource],
    ) -> Result<Vec<Relationship>, ProjectError> {
        let packages: BTreeSet<&str> = components
            .iter()
            .map(|component| {
                let ComponentSource::Package { package, .. } = &component.source;
                package.as_str()
            })
            .collect();
        let source_names: BTreeSet<&str> = sources.iter().map(|s| s.name.as_str()).collect();
        let mut seen = BTreeSet::new();
        let mut relationships = vec![];
        for relationship in declared {
            let name = relationship.name.clone();
            if !valid_name(&name) {
                return Err(invalid(format!(
                    "relationship name `{name}` must be lowercase letters, digits, and `-`"
                )));
            }
            if !seen.insert(name.clone()) {
                return Err(invalid(format!("relationship `{name}` is declared twice")));
            }
            let field = |value: &Option<String>, key: &str| {
                value
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| invalid(format!("relationship `{name}` needs `{key}`")))
            };
            let package_in_stack = |package: &str| {
                if packages.contains(package) {
                    Ok(())
                } else {
                    Err(invalid(format!(
                        "relationship `{name}` names `{package}`, which is not a component package"
                    )))
                }
            };
            let source_in_stack = |source: &str| {
                if source_names.contains(source) {
                    Ok(())
                } else {
                    Err(invalid(format!(
                        "relationship `{name}` names `{source}`, which is not a declared source"
                    )))
                }
            };
            let parsed = match relationship.kind.as_str() {
                "package-dependency" => {
                    let package = field(&relationship.package, "package")?;
                    let dependency = field(&relationship.dependency, "dependency")?;
                    package_in_stack(&package)?;
                    package_in_stack(&dependency)?;
                    let nested_allowed = match relationship.policy.as_deref() {
                        None | Some("same") => false,
                        Some("nested") => true,
                        Some(other) => {
                            return Err(invalid(format!(
                                "relationship `{name}` policy `{other}` is not `same` or `nested`"
                            )));
                        }
                    };
                    if nested_allowed && relationship.reason.is_none() {
                        return Err(invalid(format!(
                            "relationship `{name}` accepts a nested copy and must say why (`reason`)"
                        )));
                    }
                    Relationship::PackageDependency {
                        name: name.clone(),
                        package,
                        dependency,
                        nested_allowed,
                        reason: relationship.reason,
                    }
                }
                "published-from" => {
                    let package = field(&relationship.package, "package")?;
                    let source = field(&relationship.source, "source")?;
                    let path = field(&relationship.path, "path")?;
                    package_in_stack(&package)?;
                    source_in_stack(&source)?;
                    if !safe_path(&path) {
                        return Err(invalid(format!(
                            "relationship `{name}` path `{path}` is not a relative path"
                        )));
                    }
                    Relationship::PublishedFrom {
                        name: name.clone(),
                        package,
                        source,
                        path,
                    }
                }
                "pin" => {
                    let consumer = field(&relationship.consumer, "consumer")?;
                    let target = field(&relationship.target, "target")?;
                    let file = field(&relationship.file, "file")?;
                    let pattern = field(&relationship.pattern, "pattern")?;
                    source_in_stack(&consumer)?;
                    source_in_stack(&target)?;
                    if !safe_path(&file) {
                        return Err(invalid(format!(
                            "relationship `{name}` file `{file}` is not a relative path"
                        )));
                    }
                    if pin_pattern(&pattern).is_none() {
                        return Err(invalid(format!(
                            "relationship `{name}` pattern must contain exactly one `{{commit}}` placeholder"
                        )));
                    }
                    let must_be_locked = match relationship.policy.as_deref() {
                        None | Some("locked") => true,
                        Some("fixed") => false,
                        Some(other) => {
                            return Err(invalid(format!(
                                "relationship `{name}` policy `{other}` is not `locked` or `fixed`"
                            )));
                        }
                    };
                    if !must_be_locked && relationship.reason.is_none() {
                        return Err(invalid(format!(
                            "relationship `{name}` holds a fixed pin and must say why (`reason`)"
                        )));
                    }
                    Relationship::Pin {
                        name: name.clone(),
                        consumer,
                        file,
                        pattern,
                        target,
                        must_be_locked,
                        reason: relationship.reason,
                    }
                }
                "consumes" => {
                    let consumer = field(&relationship.consumer, "consumer")?;
                    let provider = field(&relationship.provider, "provider")?;
                    let contract = field(&relationship.contract, "contract")?;
                    source_in_stack(&consumer)?;
                    source_in_stack(&provider)?;
                    Relationship::Consumes {
                        name: name.clone(),
                        consumer,
                        provider,
                        contract,
                    }
                }
                other => {
                    return Err(invalid(format!(
                        "relationship `{name}` has an unknown kind `{other}` (package-dependency, published-from, pin, consumes)"
                    )));
                }
            };
            relationships.push(parsed);
        }
        relationships.sort_by(|a, b| a.name().cmp(b.name()));
        Ok(relationships)
    }

    fn acceptance(
        declared: Vec<ManifestAcceptance>,
        components: &[Component],
        sources: &[StackSource],
    ) -> Result<Vec<Acceptance>, ProjectError> {
        let known: BTreeSet<&str> = components
            .iter()
            .map(|c| c.name.as_str())
            .chain(sources.iter().map(|s| s.name.as_str()))
            .collect();
        let mut seen = BTreeSet::new();
        let mut suites = vec![];
        for suite in declared {
            let name = suite.name;
            if !valid_name(&name) {
                return Err(invalid(format!(
                    "acceptance suite name `{name}` must be lowercase letters, digits, and `-`"
                )));
            }
            if !seen.insert(name.clone()) {
                return Err(invalid(format!(
                    "acceptance suite `{name}` is declared twice"
                )));
            }
            if !["deterministic", "live", "replay", "mock", "model"].contains(&suite.mode.as_str())
            {
                return Err(invalid(format!(
                    "acceptance suite `{name}` mode `{}` is not deterministic, live, replay, mock, or model",
                    suite.mode
                )));
            }
            if suite.mode == "model" && suite.mandatory {
                return Err(invalid(format!(
                    "acceptance suite `{name}` depends on a model and cannot be mandatory"
                )));
            }
            if suite.covers.is_empty() {
                return Err(invalid(format!("acceptance suite `{name}` covers nothing")));
            }
            for covered in &suite.covers {
                if !known.contains(covered.as_str()) {
                    return Err(invalid(format!(
                        "acceptance suite `{name}` covers `{covered}`, which is neither a component nor a source"
                    )));
                }
            }
            let mut covers = suite.covers;
            covers.sort();
            covers.dedup();
            suites.push(Acceptance {
                name,
                covers,
                mode: suite.mode,
                mandatory: suite.mandatory,
                description: suite.description,
            });
        }
        // Declaration order is execution order; it is kept.
        Ok(suites)
    }

    fn copies(
        declared: Vec<ManifestCopy>,
        sources: &[StackSource],
        relationships: &[Relationship],
    ) -> Result<Vec<SourceCopy>, ProjectError> {
        let mut seen = BTreeSet::new();
        let mut copies = vec![];
        for copy in declared {
            let name = copy.name;
            if !valid_name(&name) || !seen.insert(name.clone()) {
                return Err(invalid(format!(
                    "copy `{name}` needs a unique name of lowercase letters, digits, and `-`"
                )));
            }
            let canonical = sources
                .iter()
                .find(|s| s.provides.contains(&copy.package))
                .ok_or_else(|| {
                    invalid(format!(
                        "copy `{name}` is of `{}`, which no source provides",
                        copy.package
                    ))
                })?;
            if !sources.iter().any(|s| s.name == copy.consumer) || copy.consumer == canonical.name {
                return Err(invalid(format!(
                    "copy `{name}` consumer `{}` must be a declared source other than `{}`",
                    copy.consumer, canonical.name
                )));
            }
            if !safe_path(&copy.path) {
                return Err(invalid(format!(
                    "copy `{name}` path is not a relative path"
                )));
            }
            let origin = match (copy.origin, copy.commit) {
                (Some(pin), None) => {
                    let ok = relationships.iter().any(|r| {
                        matches!(r, Relationship::Pin { name, consumer, target, .. }
                            if *name == pin && *consumer == copy.consumer && *target == canonical.name)
                    });
                    if !ok {
                        return Err(invalid(format!(
                            "copy `{name}` origin `{pin}` is not a pin of `{}` on `{}`",
                            copy.consumer, canonical.name
                        )));
                    }
                    CopyOrigin::Pin(pin)
                }
                (None, Some(commit)) => {
                    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return Err(invalid(format!(
                            "copy `{name}` commit is not a full commit"
                        )));
                    }
                    CopyOrigin::Commit(commit.to_ascii_lowercase())
                }
                (None, None) => CopyOrigin::Undeclared,
                (Some(_), Some(_)) => {
                    return Err(invalid(format!(
                        "copy `{name}` declares its origin once: `origin` (a pin) or `commit`"
                    )));
                }
            };
            let mut map = vec![];
            for entry in &copy.map {
                let Some((here, there)) = entry.split_once('=') else {
                    return Err(invalid(format!(
                        "copy `{name}` map `{entry}` is not `<copy path>=<canonical path>`"
                    )));
                };
                if !safe_path(here) || !safe_path(there) {
                    return Err(invalid(format!(
                        "copy `{name}` map `{entry}` has an unsafe path"
                    )));
                }
                map.push((here.to_owned(), there.to_owned()));
            }
            if copy.local.iter().any(|path| !safe_path(path)) {
                return Err(invalid(format!(
                    "copy `{name}` local paths must be relative"
                )));
            }
            if !copy.local.is_empty() && copy.reason.is_none() {
                return Err(invalid(format!(
                    "copy `{name}` keeps files of its own and must say why (`reason`)"
                )));
            }
            copies.push(SourceCopy {
                name,
                package: copy.package,
                consumer: copy.consumer,
                path: copy.path.trim_end_matches('/').to_owned(),
                origin,
                map,
                local: copy.local,
                reason: copy.reason,
            });
        }
        copies.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(copies)
    }

    fn divergences(
        declared: Vec<ManifestDivergence>,
        sources: &[StackSource],
    ) -> Result<Vec<Divergence>, ProjectError> {
        let mut seen = BTreeSet::new();
        let mut divergences = vec![];
        for divergence in declared {
            let name = divergence.name;
            if !valid_name(&name) || !seen.insert(name.clone()) {
                return Err(invalid(format!(
                    "divergence `{name}` needs a unique name of lowercase letters, digits, and `-`"
                )));
            }
            let pattern = divergence
                .package
                .strip_suffix('*')
                .unwrap_or(&divergence.package);
            if !(package_identity(&divergence.package)
                || pattern.ends_with('/')
                || pattern.ends_with(':'))
            {
                return Err(invalid(format!(
                    "divergence `{name}` package `{}` is not a package identity or a prefix ending in `*`",
                    divergence.package
                )));
            }
            if let Some(consumer) = &divergence.consumer
                && !sources.iter().any(|s| &s.name == consumer)
            {
                return Err(invalid(format!(
                    "divergence `{name}` consumer `{consumer}` is not a declared source"
                )));
            }
            if let Some(check) = &divergence.check
                && !FINDING_KINDS.contains(&check.as_str())
            {
                return Err(invalid(format!(
                    "divergence `{name}` check `{check}` is not one of {}",
                    FINDING_KINDS.join(", ")
                )));
            }
            if divergence.path.as_deref().is_some_and(|p| !safe_path(p)) {
                return Err(invalid(format!(
                    "divergence `{name}` path is not a relative path"
                )));
            }
            if divergence.reason.trim().is_empty() {
                return Err(invalid(format!(
                    "divergence `{name}` must say why (`reason`)"
                )));
            }
            divergences.push(Divergence {
                name,
                package: divergence.package,
                consumer: divergence.consumer,
                check: divergence.check,
                path: divergence.path,
                reason: divergence.reason,
            });
        }
        divergences.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(divergences)
    }

    /// The source that provides `package` (`npm:…` / `cargo:…`).
    pub fn canonical(&self, package: &str) -> Option<&StackSource> {
        self.sources
            .iter()
            .find(|s| s.provides.iter().any(|p| p == package))
    }

    /// `sha256` over the canonical assembly: the fingerprint plus sources,
    /// relationships, and acceptance suites. A stack lock records it, so a
    /// manifest change is visible as drift.
    pub fn assembly_digest(&self) -> String {
        #[derive(Serialize)]
        struct Canonical<'a> {
            fingerprint: &'a str,
            sources: &'a [StackSource],
            relationships: &'a [Relationship],
            acceptance: &'a [Acceptance],
            #[serde(skip_serializing_if = "<[_]>::is_empty")]
            copies: &'a [SourceCopy],
            #[serde(skip_serializing_if = "<[_]>::is_empty")]
            divergences: &'a [Divergence],
        }
        let bytes = serde_json::to_vec(&Canonical {
            fingerprint: &self.fingerprint,
            sources: &self.sources,
            relationships: &self.relationships,
            acceptance: &self.acceptance,
            copies: &self.copies,
            divergences: &self.divergences,
        })
        .expect("a stack serializes");
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    pub fn source(&self, name: &str) -> Option<&StackSource> {
        self.sources.iter().find(|source| source.name == name)
    }

    /// The component package named `package`, with its constraint.
    pub fn package(&self, package: &str) -> Option<(&Component, &str)> {
        self.components.iter().find_map(|component| {
            let ComponentSource::Package {
                package: declared,
                constraint,
                ..
            } = &component.source;
            (declared == package).then_some((component, constraint.as_str()))
        })
    }

    fn component_source(component: &ManifestComponent) -> Result<ComponentSource, ProjectError> {
        let name = &component.name;
        match component.kind.as_str() {
            "package" => {
                let source = component
                    .source
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| invalid(format!("package component `{name}` needs `source`")))?;
                let (ecosystem, package) = source.split_once(':').ok_or_else(|| {
                    invalid(format!(
                        "component `{name}` source `{source}` is not `<ecosystem>:<package>`"
                    ))
                })?;
                if ecosystem != "npm" {
                    return Err(invalid(format!(
                        "component `{name}` has an unknown source ecosystem `{ecosystem}`"
                    ))
                    .require("ecosystem", "npm")
                    .found("ecosystem", ecosystem));
                }
                if package.is_empty() || package.bytes().any(|b| b.is_ascii_whitespace()) {
                    return Err(invalid(format!("component `{name}` names no package")));
                }
                let constraint = component
                    .version
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| invalid(format!("package component `{name}` needs `version`")))?;
                if version_satisfies(&constraint, "1.0.0").is_none() {
                    return Err(invalid(format!(
                        "component `{name}` version constraint `{constraint}` is outside the supported grammar (exact, =, >=, >, <=, <, ^, ~)"
                    )));
                }
                Ok(ComponentSource::Package {
                    ecosystem: ecosystem.to_owned(),
                    package: package.to_owned(),
                    constraint: constraint.trim().to_owned(),
                })
            }
            other => Err(invalid(format!(
                "component `{name}` has an unknown kind `{other}`: a stack's components are package artifacts; an application runs on a stack and is declared by the project"
            ))
            .require("kind", "package")
            .found("kind", other)),
        }
    }

    pub fn identity(&self) -> StackIdentity {
        StackIdentity {
            name: self.name.clone(),
            version: self.version.clone(),
            fingerprint: self.fingerprint.clone(),
        }
    }

    /// Every credential name the stack references.
    pub fn credentials(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .components
            .iter()
            .flat_map(|component| component.credentials.iter().cloned())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Add the stack's requirements to `requirements`: its runtime joins the
    /// project's, and each component becomes a runtime dependency. A stack
    /// whose runtime contradicts the project's is an unresolved conflict.
    pub fn apply(&self, requirements: &mut ProjectRequirements) -> Result<(), ProjectError> {
        let origin = format!("stack:{}", self.name);
        if let Some((kind, constraint)) = &self.runtime {
            let none_yet = requirements.runtimes.is_empty();
            match requirements
                .runtimes
                .iter_mut()
                .find(|need| need.kind == *kind)
            {
                None if none_yet => requirements.runtimes.push(RuntimeNeed {
                    kind: *kind,
                    version: constraint.clone(),
                    origin: origin.clone(),
                }),
                None => {
                    let needed: Vec<_> = requirements
                        .runtimes
                        .iter()
                        .map(|n| n.kind.to_string())
                        .collect();
                    return Err(ProjectError::new(
                        FailureKind::RequirementsUnresolved,
                        format!(
                            "stack `{}` needs a runtime the project does not use",
                            self.name
                        ),
                    )
                    .require("stack runtime", kind.to_string())
                    .found("project runtime", needed.join(" | ")));
                }
                Some(need) => match (&need.version, constraint) {
                    (None, Some(_)) => need.version = constraint.clone(),
                    (Some(project), Some(stack)) if project != stack => {
                        return Err(ProjectError::new(
                            FailureKind::RequirementsUnresolved,
                            format!(
                                "stack `{}` and the project constrain {kind} differently",
                                self.name
                            ),
                        )
                        .require("stack runtime", format!("{kind} {stack}"))
                        .found("project runtime", format!("{kind} {project}")));
                    }
                    _ => {}
                },
            }
        }
        // Packages become runtime dependencies, satisfied by the dependency
        // capsule like any other.
        for component in &self.components {
            let ComponentSource::Package {
                package,
                constraint,
                ..
            } = &component.source;
            requirements.dependencies.push(DependencyNeed {
                name: package.clone(),
                specifier: constraint.clone(),
                group: DependencyGroup::Runtime,
                origin: Some(origin.clone()),
            });
        }
        requirements.normalize();
        Ok(())
    }

    /// Resolve every component against `capsule`. Nothing here fails: an unresolved or unsupported component is recorded as
    /// such, and the caller decides what that means.
    pub fn bind(&self, capsule: Option<&DependencyCapsule>) -> Result<StackBinding, ProjectError> {
        let components = self
            .components
            .iter()
            .map(|component| {
                let resolved = match &component.source {
                    ComponentSource::Package {
                        package,
                        constraint,
                        ..
                    } => capsule.and_then(|capsule| {
                        capsule
                            .dependencies
                            .iter()
                            .find(|entry| {
                                entry.name == *package
                                    && version_satisfies(constraint, &entry.version) == Some(true)
                            })
                            .map(|entry| ResolvedComponent::Package {
                                version: entry.version.clone(),
                                artifact: entry.sha256.clone(),
                            })
                    }),
                };
                ComponentBinding {
                    declared: ComponentDeclaration {
                        name: component.name.clone(),
                        source: component.source.clone(),
                        credentials: component.credentials.clone(),
                        platforms: component.platforms.clone(),
                        unsupported: component.unsupported.clone(),
                    },
                    resolved,
                }
            })
            .collect();
        Ok(StackBinding {
            identity: self.identity(),
            components,
            capsule_id: capsule
                .map(DependencyCapsule::capsule_id)
                .transpose()
                .map_err(|error| {
                    ProjectError::new(
                        FailureKind::EnvironmentMaterializationFailed,
                        error.to_string(),
                    )
                })?,
            probes: vec![],
        })
    }
}

fn sha256_of(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// The program a target's environment is verified with: an ordinary Node
/// workload that reports the version of each named package it finds in the
/// dependency environment it runs in and, when given an application bundle,
/// what `@appport/appboundry` says about it.
pub const PROBE: &str = include_str!("probes/probe.mjs");
/// The entrypoint file name the probe is bundled under.
pub const PROBE_ENTRYPOINT: &str = "probe.mjs";

/// The probe's identity, recorded in the evidence.
pub fn probe_identity() -> String {
    sha256_of(PROBE.as_bytes())
}

/// The smallest WASI module: `(module (func (export "_start")))`. Running it
/// proves the target executes WASM modules through its runtime; it proves
/// nothing about any application.
pub const WASM_PROBE: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic, version
    0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type: () -> ()
    0x03, 0x02, 0x01, 0x00, // one function of that type
    0x07, 0x0a, 0x01, 0x06, 0x5f, 0x73, 0x74, 0x61, 0x72, 0x74, 0x00, 0x00, // export "_start"
    0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b, // body: end
];
/// The entrypoint file name the WASM probe is bundled under.
pub const WASM_PROBE_ENTRYPOINT: &str = "probe.wasm";

pub fn wasm_probe_identity() -> String {
    sha256_of(WASM_PROBE)
}

/// Read the probe's report: package → the version it found (`None` when
/// absent), mapped back to the stack's component names. A component the
/// report does not mention is left out, so it is not claimed verified.
pub fn parse_probe_report(
    stdout: &str,
    binding: &StackBinding,
) -> std::collections::BTreeMap<String, Option<String>> {
    let mut by_package = std::collections::BTreeMap::new();
    for line in stdout.lines() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(package) = value["package"].as_str()
        {
            by_package.insert(
                package.to_owned(),
                value["version"].as_str().map(str::to_owned),
            );
        }
    }
    binding
        .components
        .iter()
        .filter_map(|component| match &component.declared.source {
            ComponentSource::Package { package, .. } => by_package
                .get(package)
                .map(|seen| (component.declared.name.clone(), seen.clone())),
        })
        .collect()
}

/// Find a stack by explicit path (a directory holding `stack.toml`, or the
/// file) or by name. A name is looked up in `<root>/stacks/<name>` for each
/// of `roots`, then in each `COMPUTE_STACKS` directory. Nothing is searched
/// upward, and nothing is remembered.
pub fn find_stack(selection: &str, roots: &[PathBuf]) -> Result<Stack, ProjectError> {
    let path = find_stack_file(selection, roots)?;
    let text = std::fs::read_to_string(&path).map_err(|error| {
        ProjectError::new(
            FailureKind::ProjectDiscoveryFailed,
            format!("{}: {error}", path.display()),
        )
    })?;
    let stack = Stack::parse(&text)?;
    if !selection.contains(std::path::MAIN_SEPARATOR)
        && !selection.starts_with('.')
        && stack.name != selection
    {
        return Err(invalid(format!(
            "stacks/{selection}/{STACK_FILE} declares the name `{}`",
            stack.name
        )));
    }
    Ok(stack)
}

/// The `stack.toml` `find_stack` reads for `selection`.
pub fn find_stack_file(selection: &str, roots: &[PathBuf]) -> Result<PathBuf, ProjectError> {
    let explicit = Path::new(selection);
    let path = if selection.contains(std::path::MAIN_SEPARATOR) || selection.starts_with('.') {
        let file = if explicit.is_dir() {
            explicit.join(STACK_FILE)
        } else {
            explicit.to_path_buf()
        };
        if !file.is_file() {
            return Err(ProjectError::new(
                FailureKind::ProjectDiscoveryFailed,
                format!("no stack at {}", explicit.display()),
            ));
        }
        file
    } else {
        let mut searched = vec![];
        let mut found = None;
        let extra = std::env::var_os(STACKS_ENV)
            .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
            .unwrap_or_default();
        for directory in roots.iter().map(|root| root.join("stacks")).chain(extra) {
            let candidate = directory.join(selection).join(STACK_FILE);
            searched.push(directory.display().to_string());
            if candidate.is_file() {
                found = Some(candidate);
                break;
            }
        }
        found.ok_or_else(|| {
            ProjectError::new(
                FailureKind::ProjectDiscoveryFailed,
                format!("stack `{selection}` was not found"),
            )
            .found("searched", searched.join(", "))
        })?
    };
    Ok(path)
}
