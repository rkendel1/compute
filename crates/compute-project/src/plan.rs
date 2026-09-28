//! Discovery, command selection, and environment materialization: how a
//! project's normalized requirements become the inputs Compute plans,
//! places, and executes with.

use std::path::{Path, PathBuf};

use compute_core::{
    DeclaredRequirements, DependencyCapsule, ProjectBinding, ProjectRequirements, ResolvedCommand,
    ResolvedRequirements, RuntimeKind, ToolRole,
};

use crate::error::{FailureKind, ProjectError};
use crate::pax::PaxSource;

/// A project found on disk with its requirements normalized.
#[derive(Debug, Clone)]
pub struct DiscoveredProject {
    /// The canonical project root.
    pub root: PathBuf,
    pub requirements: ProjectRequirements,
}

/// Find and normalize the project at `root`. `root` is used exactly as
/// given; there is no upward search and no ambient state.
pub fn discover(root: &Path, source: &dyn PaxSource) -> Result<DiscoveredProject, ProjectError> {
    let root = std::fs::canonicalize(root).map_err(|error| {
        ProjectError::new(
            FailureKind::ProjectDiscoveryFailed,
            format!("project directory {}: {error}", root.display()),
        )
    })?;
    if !root.is_dir() {
        return Err(ProjectError::new(
            FailureKind::ProjectDiscoveryFailed,
            format!("{} is not a directory", root.display()),
        ));
    }
    let observation = source.observe(&root)?;
    if !observation.is_project() {
        return Err(ProjectError::new(
            FailureKind::ProjectDiscoveryFailed,
            format!(
                "no PAX project found at {}: PAX recognized no manifest there",
                root.display()
            ),
        ));
    }
    let requirements = observation.requirements()?;
    if !requirements.unresolved.is_empty() {
        let mut error = ProjectError::new(
            FailureKind::RequirementsUnresolved,
            format!(
                "project `{}` declares requirements Compute cannot resolve",
                requirements.project.name
            ),
        );
        for need in &requirements.unresolved {
            error = error.require(&need.subject, need.reason.clone());
        }
        return Err(error);
    }
    Ok(DiscoveredProject { root, requirements })
}

/// Which project command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandSelection {
    /// The project's `start` command when it has one; otherwise the
    /// entrypoint Compute's own conventions find.
    Default,
    Named(String),
}

/// A project command translated into what Compute executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedCommand {
    pub name: Option<String>,
    /// The runtime the command's program implies.
    pub runtime: Option<RuntimeKind>,
    /// The entrypoint file, relative to the project root.
    pub entrypoint: Option<PathBuf>,
    pub args: Vec<String>,
}

/// Choose the command to run. Compute executes a runtime on an entrypoint
/// file; a command that is anything else (a shell pipeline, another tool) is
/// refused rather than run through a shell.
pub fn select_command(
    requirements: &ProjectRequirements,
    selection: &CommandSelection,
) -> Result<SelectedCommand, ProjectError> {
    let name = match selection {
        CommandSelection::Named(name) => name.as_str(),
        CommandSelection::Default => {
            if requirements.commands.iter().any(|c| c.name == "start") {
                "start"
            } else {
                return Ok(SelectedCommand {
                    name: None,
                    runtime: None,
                    entrypoint: None,
                    args: vec![],
                });
            }
        }
    };
    let Some(command) = requirements.commands.iter().find(|c| c.name == name) else {
        let available: Vec<_> = requirements
            .commands
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        return Err(ProjectError::new(
            FailureKind::RequirementsUnresolved,
            format!(
                "project `{}` has no command `{name}`",
                requirements.project.name
            ),
        )
        .require("command", name)
        .found(
            "commands",
            if available.is_empty() {
                "none".into()
            } else {
                available.join(", ")
            },
        ));
    };
    let unsupported = |reason: &str| {
        ProjectError::new(
            FailureKind::RequirementsUnresolved,
            format!("command `{name}` cannot run on Compute: {reason}"),
        )
        .require("command", format!("{name} = {}", command.command))
    };
    let text = command.command.as_str();
    if text
        .chars()
        .any(|c| "&|;<>$`\"'\\*?(){}[]~!#\n".contains(c))
    {
        return Err(unsupported(
            "it uses shell syntax, and Compute does not run a shell",
        ));
    }
    let mut tokens = text.split_whitespace();
    let program = tokens.next().unwrap_or_default();
    let runtime = match program {
        "node" => RuntimeKind::Node,
        "bun" => RuntimeKind::Bun,
        "python" | "python3" => RuntimeKind::Python,
        other => {
            return Err(unsupported(&format!(
                "`{other}` is not a runtime Compute executes an entrypoint with"
            )));
        }
    };
    let entrypoint = tokens.next().unwrap_or_default();
    if entrypoint.is_empty() || entrypoint.starts_with('-') || entrypoint.contains('=') {
        return Err(unsupported(
            "it must name an entrypoint file directly after the runtime",
        ));
    }
    if !requirements.runtimes.iter().any(|need| {
        need.kind == runtime || (need.kind == RuntimeKind::Node && runtime == RuntimeKind::Bun)
    }) {
        return Err(unsupported(&format!(
            "it needs {runtime}, which the project's ecosystem does not provide"
        )));
    }
    Ok(SelectedCommand {
        name: Some(name.to_owned()),
        runtime: Some(runtime),
        entrypoint: Some(PathBuf::from(entrypoint)),
        args: tokens.map(str::to_owned).collect(),
    })
}

/// What the environment was resolved to, from Compute's workload and
/// dependency capsule. Nothing here is read from the host.
#[derive(Debug)]
pub struct EnvironmentInputs<'a> {
    pub runtime: RuntimeKind,
    pub runtime_constraint: Option<&'a str>,
    pub os: Option<&'a str>,
    pub architecture: Option<&'a str>,
    pub capsule: Option<&'a DependencyCapsule>,
    pub environment_names: Vec<String>,
    /// Portable path of the entrypoint.
    pub entrypoint: String,
    pub argument_count: usize,
    pub command_name: Option<String>,
    /// Tools the targets in play advertise. No Compute target advertises
    /// tools today, so this is empty.
    pub offered_tools: &'a [String],
}

/// Turn requirements into a verified environment plan, or say exactly what
/// cannot exist. Runtime dependencies must be satisfied by the capsule
/// (Compute never resolves or installs them); execution tools must be
/// offered; the runtime must be one the project needs.
pub fn materialize(
    requirements: &ProjectRequirements,
    inputs: &EnvironmentInputs<'_>,
) -> Result<ProjectBinding, ProjectError> {
    let project = &requirements.project.name;

    if !requirements
        .runtimes
        .iter()
        .any(|need| need.kind == inputs.runtime)
    {
        let needed: Vec<_> = requirements
            .runtimes
            .iter()
            .map(|n| n.kind.to_string())
            .collect();
        return Err(ProjectError::new(
            FailureKind::RequirementsUnresolved,
            format!("the requested runtime conflicts with project `{project}`"),
        )
        .require("runtime", needed.join(" | "))
        .found("runtime", inputs.runtime.to_string()));
    }
    for need in requirements
        .runtimes
        .iter()
        .filter(|need| need.kind == inputs.runtime)
    {
        if let (Some(required), Some(constraint)) = (&need.version, inputs.runtime_constraint)
            && required != constraint
        {
            return Err(ProjectError::new(
                FailureKind::RequirementsUnresolved,
                "the project's runtime version conflicts with the configured constraint",
            )
            .require("runtime", format!("{} {required}", need.kind))
            .found("runtime", format!("{} {constraint}", need.kind)));
        }
    }

    for tool in requirements
        .tools
        .iter()
        .filter(|tool| tool.role == ToolRole::Execution)
    {
        if !inputs.offered_tools.contains(&tool.name) {
            return Err(ProjectError::new(
                FailureKind::NoTargetSatisfiesRequirements,
                format!(
                    "project `{project}` needs the tool `{}` on its target",
                    tool.name
                ),
            )
            .require("tool", &tool.name)
            .found(
                "tools",
                if inputs.offered_tools.is_empty() {
                    "none advertised".into()
                } else {
                    inputs.offered_tools.join(", ")
                },
            ));
        }
    }

    let needed: Vec<_> = requirements.runtime_dependencies().collect();
    let capsule_id = match (needed.is_empty(), inputs.capsule) {
        (true, capsule) => capsule
            .map(DependencyCapsule::capsule_id)
            .transpose()
            .map_err(|error| {
                ProjectError::new(
                    FailureKind::EnvironmentMaterializationFailed,
                    error.to_string(),
                )
            })?,
        (false, None) => {
            let mut error = ProjectError::new(
                FailureKind::DependencyUnavailable,
                format!(
                    "project `{project}` needs {} runtime dependencies and no dependency capsule was supplied; Compute consumes capsules and does not install dependencies",
                    needed.len()
                ),
            );
            for dependency in needed.iter().take(8) {
                error = error.require(&dependency.name, dependency.specifier.clone());
            }
            return Err(error.found("capsule", "none"));
        }
        (false, Some(capsule)) => {
            if capsule.runtime != inputs.runtime {
                return Err(ProjectError::new(
                    FailureKind::DependencyUnavailable,
                    "the dependency capsule is for a different runtime",
                )
                .require("runtime", inputs.runtime.to_string())
                .found("capsule runtime", capsule.runtime.to_string()));
            }
            let mut failure = ProjectError::new(
                FailureKind::DependencyUnavailable,
                format!("the dependency capsule does not provide what project `{project}` needs"),
            );
            let mut missing = false;
            for dependency in &needed {
                let entry = capsule
                    .dependencies
                    .iter()
                    .find(|entry| entry.name == dependency.name);
                let satisfied = match (entry, dependency.pinned_version()) {
                    (None, _) => false,
                    (Some(entry), Some(pin)) => entry.version == pin,
                    (Some(_), None) => true,
                };
                if !satisfied {
                    missing = true;
                    failure = failure
                        .require(&dependency.name, dependency.specifier.clone())
                        .found(
                            &dependency.name,
                            entry.map_or("absent".to_owned(), |entry| entry.version.clone()),
                        );
                }
            }
            if missing {
                return Err(failure);
            }
            Some(capsule.capsule_id().map_err(|error| {
                ProjectError::new(
                    FailureKind::EnvironmentMaterializationFailed,
                    error.to_string(),
                )
            })?)
        }
    };

    let internal = |error: compute_core::ComputeError| {
        ProjectError::new(
            FailureKind::EnvironmentMaterializationFailed,
            error.to_string(),
        )
    };
    let mut environment_names = inputs.environment_names.clone();
    environment_names.sort();
    environment_names.dedup();
    Ok(ProjectBinding {
        identity: requirements.project.clone(),
        requirements_id: requirements.requirements_id().map_err(internal)?,
        declared: DeclaredRequirements {
            runtimes: requirements.runtimes.clone(),
            tools: requirements.tools.clone(),
            dependency_count: needed.len() as u64,
            dependencies_id: requirements.runtime_dependencies_id().map_err(internal)?,
        },
        resolved: ResolvedRequirements {
            runtime: inputs.runtime,
            runtime_constraint: inputs.runtime_constraint.map(str::to_owned),
            os: inputs
                .os
                .map(str::to_owned)
                .or(requirements.platform.os.clone()),
            architecture: inputs
                .architecture
                .map(str::to_owned)
                .or(requirements.platform.architecture.clone()),
            capsule_id,
            command: ResolvedCommand {
                name: inputs.command_name.clone(),
                entrypoint: inputs.entrypoint.clone(),
                argument_count: inputs.argument_count as u64,
            },
            environment_names,
        },
    })
}
