//! The PAX adapter: the only code in Compute that knows how PAX represents a
//! project.
//!
//! PAX is an external, read-only project observer. It is consumed through
//! its versioned JSON output (`pax --json --dir <root> info|deps|scripts`,
//! schema `"1"`), never linked as a library. [`PaxObservation`] holds the
//! documents; [`PaxObservation::requirements`] normalizes them into
//! [`ProjectRequirements`], and nothing downstream sees a PAX structure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use compute_core::{
    CommandNeed, DependencyGroup, DependencyNeed, PROJECT_REQUIREMENTS_VERSION, PlatformNeed,
    ProjectIdentity, ProjectRequirements, RuntimeKind, RuntimeNeed, ToolNeed, ToolRole,
    UnresolvedNeed,
};
use serde_json::Value;

use crate::error::{FailureKind, ProjectError};

/// The PAX JSON schema this adapter understands.
pub const PAX_SCHEMA_VERSION: &str = "1";
/// The name Compute records as a project's source.
pub const PAX_SOURCE: &str = "pax";
/// Overrides the `pax` executable Compute observes projects with.
pub const PAX_ENV: &str = "COMPUTE_PAX";

/// Something that can observe a project directory the way PAX does. The
/// executable is one implementation; tests and embedders supply others.
pub trait PaxSource {
    fn observe(&self, root: &Path) -> Result<PaxObservation, ProjectError>;
}

/// Observes a project by running the external `pax` executable read-only.
/// It resolves the executable from its argument, `COMPUTE_PAX`, then `PATH`;
/// it holds no state between observations.
#[derive(Debug, Clone)]
pub struct PaxExecutable {
    program: PathBuf,
}

impl PaxExecutable {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// `COMPUTE_PAX` when set, otherwise `pax` on `PATH`.
    pub fn from_environment() -> Self {
        Self::new(std::env::var_os(PAX_ENV).unwrap_or_else(|| "pax".into()))
    }

    fn document(&self, root: &Path, command: &str) -> Result<Value, ProjectError> {
        let output = Command::new(&self.program)
            .arg("--json")
            .arg("--dir")
            .arg(root)
            .arg(command)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| {
                ProjectError::new(
                    FailureKind::ProjectDiscoveryFailed,
                    format!(
                        "the PAX executable `{}` could not be run ({error}); install PAX or set {PAX_ENV}",
                        self.program.display()
                    ),
                )
            })?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        match output.status.code() {
            Some(0) => {}
            // PAX reserves exit code 2 for invalid input, such as a
            // directory that does not exist.
            Some(2) => {
                return Err(ProjectError::new(
                    FailureKind::ProjectDiscoveryFailed,
                    format!("pax {command}: {}", first_line(stderr)),
                ));
            }
            _ => {
                return Err(ProjectError::new(
                    FailureKind::PaxMetadataInvalid,
                    format!("pax {command} failed: {}", first_line(stderr)),
                ));
            }
        }
        serde_json::from_slice(&output.stdout).map_err(|error| {
            ProjectError::new(
                FailureKind::PaxMetadataInvalid,
                format!("pax {command} did not produce JSON: {error}"),
            )
        })
    }
}

impl PaxSource for PaxExecutable {
    fn observe(&self, root: &Path) -> Result<PaxObservation, ProjectError> {
        let info = self.document(root, "info")?;
        let deps = self.document(root, "deps")?;
        let scripts = self.document(root, "scripts")?;
        PaxObservation::from_documents(info, deps, scripts)
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("no diagnostic")
}

/// PAX's observation of one project: its `info`, `deps`, and `scripts`
/// documents, validated but not interpreted.
#[derive(Debug, Clone)]
pub struct PaxObservation {
    info: Value,
    deps: Value,
    scripts: Value,
}

impl PaxObservation {
    /// Validate the three documents. Anything that is not schema `"1"` from
    /// the expected command is `pax_metadata_invalid`.
    pub fn from_documents(info: Value, deps: Value, scripts: Value) -> Result<Self, ProjectError> {
        for (document, command) in [(&info, "info"), (&deps, "deps"), (&scripts, "scripts")] {
            let invalid = |message: String| {
                ProjectError::new(
                    FailureKind::PaxMetadataInvalid,
                    format!("pax {command}: {message}"),
                )
            };
            let object = document
                .as_object()
                .ok_or_else(|| invalid("the document is not an object".into()))?;
            match object.get("schemaVersion").and_then(Value::as_str) {
                Some(PAX_SCHEMA_VERSION) => {}
                other => {
                    return Err(invalid(format!(
                        "unsupported schemaVersion {}",
                        other.unwrap_or("(missing)")
                    ))
                    .require("schemaVersion", PAX_SCHEMA_VERSION)
                    .found("schemaVersion", other.unwrap_or("(missing)")));
                }
            }
            if object.get("command").and_then(Value::as_str) != Some(command) {
                return Err(invalid("the document is for a different command".into()));
            }
        }
        let name = info["project"]["name"].as_str().unwrap_or_default();
        if name.is_empty() {
            return Err(ProjectError::new(
                FailureKind::PaxMetadataInvalid,
                "pax info: the project has no name",
            ));
        }
        if !info["components"].is_array() {
            return Err(ProjectError::new(
                FailureKind::PaxMetadataInvalid,
                "pax info: `components` is missing",
            ));
        }
        Ok(Self {
            info,
            deps,
            scripts,
        })
    }

    /// Whether PAX recognized any project component. A directory PAX finds
    /// nothing in is not a PAX project.
    pub fn is_project(&self) -> bool {
        self.info["components"]
            .as_array()
            .is_some_and(|components| !components.is_empty())
    }

    pub fn project_name(&self) -> &str {
        self.info["project"]["name"].as_str().unwrap_or_default()
    }

    /// Normalize into Compute's representation of project requirements.
    pub fn requirements(&self) -> Result<ProjectRequirements, ProjectError> {
        let ecosystem = self.info["ecosystem"].as_str().unwrap_or_default();
        let mut runtimes = vec![];
        let mut tools = vec![];
        let mut unresolved = vec![];

        match ecosystem {
            "javascript" => runtimes.push(RuntimeNeed {
                kind: RuntimeKind::Node,
                version: None,
                origin: "ecosystem:javascript".into(),
            }),
            "python" => runtimes.push(RuntimeNeed {
                kind: RuntimeKind::Python,
                version: None,
                origin: "ecosystem:python".into(),
            }),
            other => unresolved.push(UnresolvedNeed {
                subject: "ecosystem".into(),
                reason: format!(
                    "the `{}` ecosystem is not one Compute runs as a runtime workload",
                    if other.is_empty() { "unknown" } else { other }
                ),
            }),
        }

        // The package manager produces the environment; Compute never runs it.
        let manager = &self.info["manager"];
        let tool = manager["name"].as_str().map(str::to_owned).or_else(|| {
            self.info["components"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|component| component["ecosystem"].as_str() == Some(ecosystem))
                .and_then(|component| component["tool"].as_str().map(str::to_owned))
        });
        if let Some(name) = tool {
            tools.push(ToolNeed {
                name,
                version: manager["version"].as_str().map(str::to_owned),
                role: ToolRole::Provisioning,
            });
        }

        let mut dependencies = vec![];
        let declared = &self.deps["dependencies"];
        for (key, group) in [
            ("dependencies", DependencyGroup::Runtime),
            ("devDependencies", DependencyGroup::Development),
            ("optionalDependencies", DependencyGroup::Optional),
            ("peerDependencies", DependencyGroup::Peer),
        ] {
            for (name, specifier) in string_map(&declared[key], "deps", key)? {
                dependencies.push(DependencyNeed {
                    name,
                    specifier,
                    group,
                    origin: None,
                });
            }
        }
        if declared["nativeDependencies"]
            .as_array()
            .is_some_and(|native| !native.is_empty())
        {
            unresolved.push(UnresolvedNeed {
                subject: "dependencies".into(),
                reason: "PAX reports these dependencies only as ecosystem-native groups, which Compute does not normalize".into(),
            });
        }

        let commands = string_map(&self.scripts["scripts"], "scripts", "scripts")?
            .into_iter()
            .map(|(name, command)| CommandNeed { name, command })
            .collect();

        let mut requirements = ProjectRequirements {
            requirements_version: PROJECT_REQUIREMENTS_VERSION.into(),
            project: ProjectIdentity {
                source: PAX_SOURCE.into(),
                name: self.project_name().into(),
                source_schema: PAX_SCHEMA_VERSION.into(),
            },
            runtimes,
            tools,
            dependencies,
            // PAX does not report platform constraints; a project that needs
            // one states it to Compute (`[runtime] architecture`, `--platform`).
            platform: PlatformNeed::default(),
            environment: vec![],
            commands,
            unresolved,
        };
        requirements.normalize();
        Ok(requirements)
    }
}

/// An optional JSON object of strings. Absent or null is empty; anything
/// else is invalid metadata.
fn string_map(
    value: &Value,
    command: &str,
    key: &str,
) -> Result<BTreeMap<String, String>, ProjectError> {
    match value {
        Value::Null => Ok(BTreeMap::new()),
        Value::Object(map) => map
            .iter()
            .map(|(name, value)| {
                value
                    .as_str()
                    .map(|text| (name.clone(), text.to_owned()))
                    .ok_or_else(|| {
                        ProjectError::new(
                            FailureKind::PaxMetadataInvalid,
                            format!("pax {command}: `{key}.{name}` is not a string"),
                        )
                    })
            })
            .collect(),
        _ => Err(ProjectError::new(
            FailureKind::PaxMetadataInvalid,
            format!("pax {command}: `{key}` is not an object"),
        )),
    }
}
