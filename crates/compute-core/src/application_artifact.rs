//! A portable application artifact: `compute.application-artifact@1`.
//!
//! One file carries everything a provider needs to run an application: the
//! canonical workload bundle and the application manifest that says what
//! it is and what it needs.
//!
//! ```text
//! application.json    the manifest: identity, version, runtime, entrypoint,
//!                     port, requirements, env contract, capabilities,
//!                     metadata, and the bundle's identities
//! workload.compute    the canonical workload bundle
//! ```
//!
//! The archive is deterministic (the same inputs give the same bytes), so
//! its SHA-256 is the artifact's identity. Reading one verifies that the
//! manifest describes exactly the bundle it carries: a manifest cannot
//! claim a runtime, entrypoint, or requirement the bundle does not have.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    ApplicationIdentity, ComputeError, IsolationRequirement, NetworkPolicy, ResourceLimits, Result,
    RuntimeKind, WorkloadBundle, sha256_identity,
};

pub const APPLICATION_ARTIFACT_FORMAT: &str = "compute.application-artifact@1";
/// The largest artifact a provider or the CLI will fetch or accept.
pub const APPLICATION_ARTIFACT_MAX_BYTES: u64 = 256 * 1024 * 1024;

const MANIFEST: &str = "application.json";
const BUNDLE: &str = "workload.compute";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationManifest {
    pub format: String,
    /// The application's identity (`compute.application@1`). It has a port:
    /// an artifact is something that serves.
    pub application: ApplicationIdentity,
    /// The developer's label for this build (`1.4.0`), if any. Deployment
    /// versions (`v1`, `v2`) are the provider's, and separate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub runtime: ApplicationRuntime,
    pub entrypoint: PathBuf,
    pub requirements: ApplicationRequirements,
    #[serde(default, skip_serializing_if = "EnvContract::is_empty")]
    pub env: EnvContract,
    /// What the application offers, as capability names. Descriptive:
    /// Compute records and reports them.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    pub workload: ArtifactWorkload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationRuntime {
    pub name: RuntimeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationRequirements {
    pub resources: ResourceLimits,
    pub network: NetworkPolicy,
    pub isolation: IsolationRequirement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
}

/// The environment an application expects. `required` names must be
/// supplied by the deployment's configuration; `defaults` are the values
/// built into the artifact, which configuration may override.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvContract {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub required: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub defaults: BTreeMap<String, String>,
}

impl EnvContract {
    pub fn is_empty(&self) -> bool {
        self.required.is_empty() && self.defaults.is_empty()
    }

    /// The required names that neither `config` nor a default supplies.
    pub fn missing<'a>(&'a self, config: &BTreeMap<String, String>) -> Vec<&'a str> {
        self.required
            .iter()
            .filter(|name| !config.contains_key(*name) && !self.defaults.contains_key(*name))
            .map(String::as_str)
            .collect()
    }
}

/// The identities of the bundle an artifact carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactWorkload {
    pub workload_id: String,
    pub bundle_id: String,
    pub sha256: String,
    pub size: u64,
}

/// What a caller says about the application beyond its bundle.
#[derive(Debug, Clone, Default)]
pub struct ApplicationDescription {
    pub version: Option<String>,
    pub required_env: BTreeSet<String>,
    pub capabilities: BTreeSet<String>,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationArtifact {
    pub manifest: ApplicationManifest,
    bundle: Vec<u8>,
}

impl ApplicationArtifact {
    /// Package `bundle` as the application `identity`.
    pub fn new(
        identity: ApplicationIdentity,
        description: ApplicationDescription,
        bundle: &WorkloadBundle,
    ) -> Result<Self> {
        identity.verify()?;
        if identity.port.is_none() {
            return Err(invalid(
                "an application artifact needs the port it serves on",
            ));
        }
        let bytes = bundle.to_bytes()?;
        let workload = &bundle.workload;
        let manifest = ApplicationManifest {
            format: APPLICATION_ARTIFACT_FORMAT.into(),
            application: identity,
            version: description.version,
            runtime: ApplicationRuntime {
                name: workload.runtime,
                version: workload.runtime_version.clone(),
            },
            entrypoint: workload.entrypoint.clone(),
            requirements: ApplicationRequirements {
                resources: workload.resources.clone(),
                network: workload.network.clone(),
                isolation: workload.isolation.clone(),
                architecture: workload.architecture.clone(),
            },
            env: EnvContract {
                required: description.required_env,
                defaults: workload.env.clone(),
            },
            capabilities: description.capabilities,
            metadata: description.metadata,
            workload: ArtifactWorkload {
                workload_id: bundle.workload_id()?,
                bundle_id: bundle.bundle_id()?,
                sha256: sha256_identity(&bytes),
                size: bytes.len() as u64,
            },
        };
        let artifact = Self {
            manifest,
            bundle: bytes,
        };
        artifact.verify()?;
        Ok(artifact)
    }

    /// Whether `bytes` look like an application artifact (rather than, say,
    /// a bare workload bundle). It does not verify them.
    pub fn sniff(bytes: &[u8]) -> bool {
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        archive
            .entries()
            .ok()
            .and_then(|mut entries| entries.next())
            .and_then(|entry| entry.ok())
            .and_then(|entry| entry.path().ok().map(|path| path == Path::new(MANIFEST)))
            .unwrap_or(false)
    }

    pub fn read(path: &Path) -> Result<Self> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > APPLICATION_ARTIFACT_MAX_BYTES {
            return Err(invalid("application artifact is too large"));
        }
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let mut files = BTreeMap::<PathBuf, Vec<u8>>::new();
        for entry in archive.entries().map_err(archive_error)? {
            let mut entry = entry.map_err(archive_error)?;
            if !entry.header().entry_type().is_file() {
                return Err(invalid("an application artifact holds only regular files"));
            }
            let path = entry.path().map_err(archive_error)?.into_owned();
            if path != Path::new(MANIFEST) && path != Path::new(BUNDLE) {
                return Err(invalid(format!(
                    "unexpected entry in application artifact: {}",
                    path.display()
                )));
            }
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(archive_error)?;
            if files.insert(path, data).is_some() {
                return Err(invalid("duplicate entry in application artifact"));
            }
        }
        let manifest = files
            .remove(Path::new(MANIFEST))
            .ok_or_else(|| invalid("application artifact has no application.json"))?;
        let bundle = files
            .remove(Path::new(BUNDLE))
            .ok_or_else(|| invalid("application artifact has no workload.compute"))?;
        let manifest: ApplicationManifest = serde_json::from_slice(&manifest)
            .map_err(|error| invalid(format!("invalid application.json: {error}")))?;
        let artifact = Self { manifest, bundle };
        artifact.verify()?;
        if artifact.to_bytes()? != bytes {
            return Err(invalid("application artifact is not in canonical form"));
        }
        Ok(artifact)
    }

    /// The manifest describes exactly the bundle it carries.
    fn verify(&self) -> Result<()> {
        let manifest = &self.manifest;
        if manifest.format != APPLICATION_ARTIFACT_FORMAT {
            return Err(invalid(format!(
                "unsupported application artifact format: {}",
                manifest.format
            )));
        }
        manifest.application.verify()?;
        if manifest.application.port.is_none() {
            return Err(invalid(
                "an application artifact needs the port it serves on",
            ));
        }
        let bundle = WorkloadBundle::from_bytes(&self.bundle)?;
        let workload = &bundle.workload;
        let described = ArtifactWorkload {
            workload_id: bundle.workload_id()?,
            bundle_id: bundle.bundle_id()?,
            sha256: sha256_identity(&self.bundle),
            size: self.bundle.len() as u64,
        };
        if manifest.workload != described {
            return Err(invalid(
                "application manifest does not describe the bundle it carries",
            ));
        }
        let requirements = ApplicationRequirements {
            resources: workload.resources.clone(),
            network: workload.network.clone(),
            isolation: workload.isolation.clone(),
            architecture: workload.architecture.clone(),
        };
        if manifest.runtime.name != workload.runtime
            || manifest.runtime.version != workload.runtime_version
            || manifest.entrypoint != workload.entrypoint
            || manifest.requirements != requirements
            || manifest.env.defaults != workload.env
        {
            return Err(invalid(
                "application manifest's runtime, entrypoint, requirements, or environment differ from its bundle",
            ));
        }
        Ok(())
    }

    /// The deterministic archive.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let manifest = serde_json::to_vec(&self.manifest)?;
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            builder.mode(tar::HeaderMode::Deterministic);
            for (path, data) in [(MANIFEST, manifest.as_slice()), (BUNDLE, &self.bundle)] {
                let mut header = tar::Header::new_ustar();
                header.set_path(path).map_err(archive_error)?;
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                header.set_cksum();
                builder.append(&header, data).map_err(archive_error)?;
            }
            builder.finish().map_err(archive_error)?;
        }
        Ok(bytes)
    }

    /// The artifact's identity: the SHA-256 of its canonical bytes.
    pub fn artifact_id(&self) -> Result<String> {
        Ok(sha256_identity(&self.to_bytes()?))
    }

    /// The canonical workload bundle's bytes.
    pub fn bundle_bytes(&self) -> &[u8] {
        &self.bundle
    }

    pub fn bundle(&self) -> Result<WorkloadBundle> {
        WorkloadBundle::from_bytes(&self.bundle)
    }
}

/// Where a provider gets an artifact: a `file://` path on the provider, or
/// an `http(s)://` URL, pinned to the artifact's digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReference {
    pub url: String,
    /// `sha256:…` of the artifact's bytes. The fetched bytes must match.
    pub digest: String,
}

impl ArtifactReference {
    /// Fetch the referenced bytes and verify them against the digest.
    pub fn fetch(&self) -> Result<Vec<u8>> {
        if !self.digest.starts_with("sha256:") || self.digest.len() != 71 {
            return Err(invalid("an artifact reference needs a sha256: digest"));
        }
        let bytes = fetch(&self.url)?;
        let actual = sha256_identity(&bytes);
        if actual != self.digest {
            return Err(invalid(format!(
                "artifact at {} has digest {actual}, expected {}",
                self.url, self.digest
            )));
        }
        Ok(bytes)
    }
}

/// Read `url` (`file://` or `http(s)://`), up to the artifact size limit.
pub fn fetch(url: &str) -> Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        let path = Path::new(path);
        if !path.is_absolute() {
            return Err(invalid("a file:// artifact URL needs an absolute path"));
        }
        let size = std::fs::metadata(path)
            .map_err(|error| invalid(format!("cannot read {url}: {error}")))?
            .len();
        if size > APPLICATION_ARTIFACT_MAX_BYTES {
            return Err(invalid(format!("{url} is too large for an artifact")));
        }
        return std::fs::read(path).map_err(|error| invalid(format!("cannot read {url}: {error}")));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(invalid(format!(
            "unsupported artifact URL {url}: use file://, http://, or https://"
        )));
    }
    let output = std::process::Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=http,https",
            "--max-filesize",
            &APPLICATION_ARTIFACT_MAX_BYTES.to_string(),
        ])
        .arg(url)
        .output()
        .map_err(|error| invalid(format!("cannot fetch {url}: {error}")))?;
    if !output.status.success() {
        return Err(invalid(format!(
            "cannot fetch {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn invalid(message: impl Into<String>) -> ComputeError {
    ComputeError::InvalidBundle(message.into())
}

fn archive_error(error: std::io::Error) -> ComputeError {
    invalid(format!("invalid application artifact: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkloadSpec;

    fn bundle(directory: &Path) -> WorkloadBundle {
        std::fs::write(directory.join("main.py"), "print('hello')\n").unwrap();
        let workload: WorkloadSpec = serde_json::from_value(serde_json::json!({
            "version": "1",
            "runtime": "python",
            "entrypoint": "main.py",
            "env": {"GREETING": "hello"},
        }))
        .unwrap();
        WorkloadBundle::create_from(workload, directory).unwrap()
    }

    fn artifact(directory: &Path) -> ApplicationArtifact {
        ApplicationArtifact::new(
            ApplicationIdentity::new("hello", Some(3000)).unwrap(),
            ApplicationDescription {
                version: Some("1.0.0".into()),
                required_env: ["API_KEY".into()].into(),
                capabilities: ["http.hello".into()].into(),
                metadata: [("team".into(), "platform".into())].into(),
            },
            &bundle(directory),
        )
        .unwrap()
    }

    #[test]
    fn an_artifact_is_deterministic_and_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let first = artifact(directory.path());
        let second = artifact(directory.path());
        let bytes = first.to_bytes().unwrap();
        assert_eq!(bytes, second.to_bytes().unwrap());
        assert!(ApplicationArtifact::sniff(&bytes));
        assert!(!ApplicationArtifact::sniff(first.bundle_bytes()));
        let read = ApplicationArtifact::from_bytes(&bytes).unwrap();
        assert_eq!(read, first);
        assert_eq!(read.artifact_id().unwrap(), sha256_identity(&bytes));
        assert_eq!(read.manifest.runtime.name, RuntimeKind::Python);
        assert_eq!(read.manifest.env.defaults["GREETING"], "hello");
        assert_eq!(read.manifest.env.missing(&BTreeMap::new()), vec!["API_KEY"]);
        assert!(
            read.manifest
                .env
                .missing(&[("API_KEY".into(), "k".into())].into())
                .is_empty()
        );
    }

    #[test]
    fn a_manifest_cannot_misdescribe_its_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let mut artifact = artifact(directory.path());
        artifact.manifest.runtime.name = RuntimeKind::Node;
        let error = ApplicationArtifact::from_bytes(&artifact.to_bytes().unwrap()).unwrap_err();
        assert!(
            error.to_string().contains("differ from its bundle"),
            "{error}"
        );

        let mut artifact = self::artifact(directory.path());
        artifact.manifest.application.name = "other".into();
        assert!(ApplicationArtifact::from_bytes(&artifact.to_bytes().unwrap()).is_err());
    }

    #[test]
    fn a_reference_is_verified_against_its_digest() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = artifact(directory.path()).to_bytes().unwrap();
        let path = directory.path().join("hello.capp");
        std::fs::write(&path, &bytes).unwrap();
        let url = format!("file://{}", path.display());
        let reference = ArtifactReference {
            url: url.clone(),
            digest: sha256_identity(&bytes),
        };
        assert_eq!(reference.fetch().unwrap(), bytes);
        let wrong = ArtifactReference {
            url,
            digest: sha256_identity(b"other"),
        };
        assert!(wrong.fetch().unwrap_err().to_string().contains("expected"));
        assert!(fetch("ftp://example.invalid/a").is_err());
        assert!(fetch("file://relative/path").is_err());
    }
}
