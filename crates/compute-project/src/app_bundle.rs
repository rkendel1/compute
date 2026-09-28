//! Application bundles: the application that runs on a configured Computer.
//!
//! A project declares the bundle it runs (`[artifact]` in `compute.toml`).
//! It is not part of any stack: a stack configures the Computer, and the
//! bundle is the application that runs there. Compute finds the bundle among
//! the files supplied to the workload, by content, and records what the
//! target's own AppBoundry package reports about it.
//!
//! What this module reads from a manifest is only what it needs to *find* the
//! bundle: the protocol, the application id and version, the module's path
//! and format, its hash, and its package identity. It does not parse or
//! certify the manifest; AppBoundry's API does that on the target.

use std::path::Path;

use compute_core::{
    AppBundleBinding, AppBundleDeclaration, AppInspection, ResolvedAppBundle, RuntimeKind,
    version_satisfies,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{FailureKind, ProjectError};

/// The protocol identifier of an application bundle's manifest.
pub const APP_BUNDLE_PROTOCOL: &str = "AppPort/application-bundle/1";

/// A file supplied to the workload: its portable path and bytes.
#[derive(Debug, Clone)]
pub struct SuppliedFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

fn sha256_of(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Section {
    application: String,
    #[serde(default)]
    version: Option<String>,
    runtime: String,
    abi: String,
    #[serde(default)]
    artifact: Option<String>,
}

fn invalid(message: impl Into<String>) -> ProjectError {
    ProjectError::new(FailureKind::RequirementsUnresolved, message)
}

/// The bundle the project at `root` declares in `compute.toml` `[artifact]`,
/// if it declares one.
pub fn declared_bundle(root: &Path) -> Result<Option<AppBundleDeclaration>, ProjectError> {
    let path = root.join("compute.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let value: toml::Value =
        toml::from_str(&text).map_err(|error| invalid(format!("compute.toml: {error}")))?;
    let Some(table) = value.get("artifact") else {
        return Ok(None);
    };
    let section: Section = table
        .clone()
        .try_into()
        .map_err(|error| invalid(format!("compute.toml [artifact]: {error}")))?;
    let runtime = section
        .runtime
        .parse::<RuntimeKind>()
        .map_err(|_| invalid(format!("[artifact] unknown runtime `{}`", section.runtime)))?;
    if let Some(pin) = &section.artifact {
        compute_core::validate_sha256_identity(pin).map_err(|_| {
            invalid("[artifact] `artifact` must be `sha256:` followed by 64 hex digits")
        })?;
    }
    if let Some(version) = &section.version
        && version_satisfies(version, "1.0.0").is_none()
    {
        return Err(invalid(format!(
            "[artifact] version constraint `{version}` is outside the supported grammar"
        )));
    }
    Ok(Some(AppBundleDeclaration {
        application: section.application,
        version: section.version,
        runtime,
        abi: section.abi,
        artifact: section.artifact,
    }))
}

/// Find the supplied files that are the declared bundle: a manifest (any
/// JSON file with the bundle protocol) that declares the application, and the
/// module it names — beside it, with the hash it carries.
pub fn resolve_bundle(
    declaration: &AppBundleDeclaration,
    files: &[SuppliedFile],
) -> Option<ResolvedAppBundle> {
    files.iter().find_map(|manifest| {
        if manifest.bytes.first() != Some(&b'{') || manifest.bytes.len() > 4 * 1024 * 1024 {
            return None;
        }
        let value: serde_json::Value = serde_json::from_slice(&manifest.bytes).ok()?;
        if value["protocol"] != APP_BUNDLE_PROTOCOL
            || value["application"]["id"] != declaration.application.as_str()
            || value["artifact"]["format"] != declaration.abi.as_str()
        {
            return None;
        }
        let version = value["application"]["version"].as_str()?;
        if let Some(constraint) = &declaration.version
            && version_satisfies(constraint, version) == Some(false)
        {
            return None;
        }
        let module_name = value["artifact"]["path"].as_str()?;
        let hash = value["artifact"]["hash"].as_str()?;
        let directory = manifest
            .path
            .rsplit_once('/')
            .map(|(directory, _)| directory);
        let module_path = match directory {
            Some(directory) => format!("{directory}/{module_name}"),
            None => module_name.to_owned(),
        };
        let module = files.iter().find(|file| file.path == module_path)?;
        let module_sha256 = sha256_of(&module.bytes);
        if module_sha256 != format!("sha256:{hash}")
            || declaration
                .artifact
                .as_ref()
                .is_some_and(|pin| *pin != module_sha256)
        {
            return None;
        }
        Some(ResolvedAppBundle {
            manifest_path: manifest.path.clone(),
            manifest_sha256: sha256_of(&manifest.bytes),
            module_path,
            module_sha256,
            version: version.to_owned(),
            package_identity: value["packageIdentity"]["contentAddress"]
                .as_str()?
                .to_owned(),
        })
    })
}

/// Start a binding for a declared bundle.
pub fn bind_bundle(declaration: AppBundleDeclaration, files: &[SuppliedFile]) -> AppBundleBinding {
    let resolved = resolve_bundle(&declaration, files);
    AppBundleBinding {
        declared: declaration,
        resolved,
        runtime_probe: None,
        inspection: None,
    }
}

/// Read the application line of the probe's report (AppBoundry's verdict).
/// `None` when the probe reported no verdict or an error (the platform
/// package was absent, or refused to run): the bundle is then not inspected.
pub fn parse_inspection(stdout: &str, receipt: &str) -> Option<AppInspection> {
    stdout.lines().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let application = value.get("application")?;
        let text = |key: &str| application[key].as_str().map(str::to_owned);
        Some(AppInspection {
            receipt: receipt.to_owned(),
            package_version: text("packageVersion")?,
            certification: text("certification")?,
            failed_checks: application["failedChecks"]
                .as_array()?
                .iter()
                .filter_map(|check| check.as_str().map(str::to_owned))
                .collect(),
            application_id: text("applicationId").unwrap_or_default(),
            artifact_hash: text("artifactHash").unwrap_or_default(),
            package_identity: text("packageIdentity").unwrap_or_default(),
            readiness: text("readiness")?,
            missing_providers: application["missingProviders"]
                .as_array()?
                .iter()
                .filter_map(|provider| provider.as_str().map(str::to_owned))
                .collect(),
        })
    })
}
