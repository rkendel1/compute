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
        Ok(Self {
            name: header.name,
            version: header.version,
            description: header.description,
            runtime,
            components,
            fingerprint: format!("sha256:{:x}", Sha256::digest(bytes)),
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
