use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use compute_core::{
    PlatformIdentity, ProviderRuntimeRequirement, RuntimeCapabilities, RuntimeDistribution,
    RuntimeLifecycleStatus, RuntimePreparation, RuntimePreparationStep, RuntimeResolution,
    sha256_identity,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use walkdir::WalkDir;

use crate::{ProviderError, ProviderErrorKind};

#[derive(Debug)]
pub(crate) struct RuntimeManager {
    root: PathBuf,
    lock: RuntimeLock,
    mutation: Mutex<()>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeLock {
    schema_version: u32,
    runtimes: BTreeMap<String, LockedRuntime>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedRuntime {
    version: String,
    executable: String,
    #[serde(default)]
    artifacts: BTreeMap<String, LockedArtifact>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedArtifact {
    url: String,
    sha256: String,
    format: ArchiveFormat,
    install: Vec<InstallMapping>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ArchiveFormat {
    TarGz,
    TarXz,
    Zip,
    Apk,
    File,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallMapping {
    source: String,
    destination: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeManifest {
    schema_version: u32,
    compute_version: String,
    distribution_id: String,
    distribution_version: String,
    platform: String,
    os: String,
    architecture: String,
    runtime_lock_sha256: String,
    certification_status: String,
    build: serde_json::Value,
    runtimes: BTreeMap<String, PreparedRuntime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PreparedRuntime {
    version: String,
    executable: String,
    artifact_sha256: String,
    payload_sha256: String,
    reported_version: String,
    distribution_id: Option<String>,
    distribution_digest: Option<String>,
    capabilities: Option<RuntimeCapabilities>,
}

impl RuntimeManager {
    pub(crate) fn new(provider_key: &str) -> Self {
        let lock: RuntimeLock =
            serde_json::from_slice(include_bytes!("../../../distribution/runtime-lock.json"))
                .expect("valid embedded runtime lock");
        assert_eq!(lock.schema_version, 2, "supported runtime lock schema");
        let root = std::env::var_os("COMPUTE_RUNTIME_STORE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let key = format!("{:x}", Sha256::digest(provider_key.as_bytes()));
                std::env::temp_dir().join("compute-runtime-store").join(key)
            });
        Self {
            root,
            lock,
            mutation: Mutex::new(()),
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn resolve(
        &self,
        requirement: ProviderRuntimeRequirement,
        capabilities: RuntimeCapabilities,
    ) -> RuntimeResolution {
        let platform = PlatformIdentity {
            runtime_abi: None,
            ..PlatformIdentity::current()
        };
        let unsupported = |detail: String| RuntimeResolution {
            requirement: requirement.clone(),
            status: RuntimeLifecycleStatus::Unsupported,
            distribution: None,
            detail: Some(detail),
        };
        if requirement.platform.as_ref().is_some_and(|required| {
            required.os != platform.os || required.architecture != platform.architecture
        }) {
            return unsupported(format!(
                "runtime requires {}, provider is {}",
                requirement.platform.as_ref().expect("checked").label(),
                platform.label()
            ));
        }
        let Some(locked) = self.lock.runtimes.get(requirement.runtime.as_str()) else {
            return unsupported("runtime is absent from the provider catalog".into());
        };
        if requirement.version.as_ref().is_some_and(|version| {
            !compute_core::runtime_version_matches(requirement.runtime, version, &locked.version)
        }) {
            return unsupported(format!(
                "requested {}, catalog contains {}",
                requirement.version.as_deref().unwrap_or_default(),
                locked.version
            ));
        }
        let platform_label = platform.label();
        let Some(artifact) = locked.artifacts.get(&platform_label) else {
            return unsupported(format!("no artifact for {platform_label}"));
        };
        let digest = format!("sha256:{}", artifact.sha256);
        let mut distribution = RuntimeDistribution {
            id: String::new(),
            runtime: requirement.runtime,
            version: locked.version.clone(),
            platform,
            artifact: artifact.url.clone(),
            digest,
            source: "compute-runtime-lock".into(),
            executable: locked.executable.clone(),
            capabilities,
        };
        distribution.id = distribution
            .canonical_id()
            .expect("runtime distribution is serializable");
        let (status, detail) = self.status_inner(&distribution);
        RuntimeResolution {
            requirement,
            status,
            distribution: Some(distribution),
            detail,
        }
    }

    pub(crate) fn status(&self, distribution: &RuntimeDistribution) -> RuntimeResolution {
        let requirement = ProviderRuntimeRequirement {
            runtime: distribution.runtime,
            version: Some(distribution.version.clone()),
            platform: Some(distribution.platform.clone()),
        };
        let (status, detail) = self.status_inner(distribution);
        RuntimeResolution {
            requirement,
            status,
            distribution: Some(distribution.clone()),
            detail,
        }
    }

    fn status_inner(
        &self,
        distribution: &RuntimeDistribution,
    ) -> (RuntimeLifecycleStatus, Option<String>) {
        let failure = self.failure_path(&distribution.id);
        let manifest = self.read_manifest();
        let Some(prepared) = manifest
            .as_ref()
            .ok()
            .and_then(|manifest| manifest.runtimes.get(distribution.runtime.as_str()))
        else {
            if failure.is_file() {
                return (
                    RuntimeLifecycleStatus::Failed,
                    fs::read_to_string(failure).ok(),
                );
            }
            return (RuntimeLifecycleStatus::Available, None);
        };
        let expected_digest = distribution.digest.trim_start_matches("sha256:");
        if prepared.distribution_id.as_deref() != Some(&distribution.id)
            || prepared.artifact_sha256 != expected_digest
            || prepared.version != distribution.version
            || prepared.executable != distribution.executable
        {
            return (
                RuntimeLifecycleStatus::Failed,
                Some("prepared runtime metadata differs from the resolved distribution".into()),
            );
        }
        let payload = self
            .root
            .join("runtimes")
            .join(distribution.runtime.as_str());
        match hash_tree(&payload) {
            Ok(actual) if actual == prepared.payload_sha256 => {
                (RuntimeLifecycleStatus::Ready, None)
            }
            Ok(_) => (
                RuntimeLifecycleStatus::Failed,
                Some("prepared runtime payload digest mismatch".into()),
            ),
            Err(error) => (RuntimeLifecycleStatus::Failed, Some(error.message)),
        }
    }

    pub(crate) fn prepare(
        &self,
        distribution: &RuntimeDistribution,
    ) -> Result<RuntimePreparation, ProviderError> {
        distribution
            .validate()
            .map_err(|error| unavailable(error.to_string()))?;
        let catalog = self.resolve(
            ProviderRuntimeRequirement {
                runtime: distribution.runtime,
                version: Some(distribution.version.clone()),
                platform: Some(distribution.platform.clone()),
            },
            distribution.capabilities.clone(),
        );
        if catalog.distribution.as_ref() != Some(distribution) {
            return Err(unavailable(
                "runtime distribution is not the provider's canonical catalog entry",
            ));
        }
        let _guard = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let status = self.status(distribution);
        if status.status == RuntimeLifecycleStatus::Ready {
            return Ok(RuntimePreparation {
                distribution: distribution.clone(),
                status: RuntimeLifecycleStatus::Ready,
                verified: true,
                steps: vec![step("status", "ready", Some("already prepared"))],
            });
        }
        let result = self.prepare_inner(distribution);
        if let Err(error) = &result {
            let _ = fs::create_dir_all(self.root.join("failures"));
            let _ = fs::write(self.failure_path(&distribution.id), &error.message);
        }
        result
    }

    fn prepare_inner(
        &self,
        distribution: &RuntimeDistribution,
    ) -> Result<RuntimePreparation, ProviderError> {
        let locked = self
            .lock
            .runtimes
            .get(distribution.runtime.as_str())
            .ok_or_else(|| unavailable("runtime is absent from the provider catalog"))?;
        let artifact = locked
            .artifacts
            .get(&distribution.platform.label())
            .ok_or_else(|| unavailable("runtime has no artifact for this platform"))?;
        let expected = distribution.digest.trim_start_matches("sha256:");
        if artifact.sha256 != expected {
            return Err(unavailable(
                "resolved runtime digest differs from the catalog",
            ));
        }
        fs::create_dir_all(self.root.join("cache/sha256")).map_err(io_error)?;
        fs::create_dir_all(self.root.join("runtimes")).map_err(io_error)?;
        let cached = self.root.join("cache/sha256").join(expected);
        acquire(&distribution.artifact, &cached, expected)?;
        let actual = sha256_file(&cached)?;
        if actual != expected {
            return Err(unavailable(format!(
                "runtime artifact digest mismatch: expected sha256:{expected}, got sha256:{actual}"
            )));
        }

        let staging = tempfile::Builder::new()
            .prefix(".runtime-prepare-")
            .tempdir_in(&self.root)
            .map_err(io_error)?;
        let unpacked = staging.path().join("unpacked");
        fs::create_dir_all(&unpacked).map_err(io_error)?;
        unpack(&cached, artifact.format, &unpacked)?;
        let assembled = staging.path().join("assembled");
        fs::create_dir_all(&assembled).map_err(io_error)?;
        for mapping in &artifact.install {
            let source = safe_join(&unpacked, &mapping.source)?;
            let destination = safe_join(&assembled, &mapping.destination)?;
            copy_tree(&source, &destination)?;
        }
        let executable = safe_join(&assembled, &locked.executable)?;
        if !executable.is_file() {
            return Err(unavailable(format!(
                "prepared runtime is missing executable {}",
                locked.executable
            )));
        }
        let output = Command::new(&executable)
            .arg("--version")
            .output()
            .map_err(io_error)?;
        let reported = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .trim()
        .to_string();
        if !output.status.success() || !reported.contains(&locked.version) {
            return Err(unavailable(format!(
                "prepared runtime version mismatch: expected {}, observed {reported}",
                locked.version
            )));
        }
        let staged_runtime = assembled
            .join("runtimes")
            .join(distribution.runtime.as_str());
        let payload_sha256 = hash_tree(&staged_runtime)?
            .trim_start_matches("sha256:")
            .to_owned();
        let target = self
            .root
            .join("runtimes")
            .join(distribution.runtime.as_str());
        if target.exists() {
            fs::remove_dir_all(&target).map_err(io_error)?;
        }
        fs::rename(&staged_runtime, &target).map_err(io_error)?;

        let mut manifest = self
            .read_manifest()
            .unwrap_or_else(|_| self.empty_manifest());
        manifest.runtimes.insert(
            distribution.runtime.as_str().into(),
            PreparedRuntime {
                version: locked.version.clone(),
                executable: locked.executable.clone(),
                artifact_sha256: expected.into(),
                payload_sha256,
                reported_version: reported,
                distribution_id: Some(distribution.id.clone()),
                distribution_digest: Some(distribution.digest.clone()),
                capabilities: Some(distribution.capabilities.clone()),
            },
        );
        write_json_atomic(&self.root.join("runtime-manifest.json"), &manifest)?;
        write_json_atomic(
            &self.root.join("runtime-inventory.json"),
            &manifest.runtimes,
        )?;
        fs::write(
            self.root.join("runtime-lock.json"),
            include_bytes!("../../../distribution/runtime-lock.json"),
        )
        .map_err(io_error)?;
        let _ = fs::remove_file(self.failure_path(&distribution.id));
        let final_status = self.status(distribution);
        if final_status.status != RuntimeLifecycleStatus::Ready {
            return Err(unavailable(
                final_status
                    .detail
                    .unwrap_or_else(|| "prepared runtime did not become ready".into()),
            ));
        }
        Ok(RuntimePreparation {
            distribution: distribution.clone(),
            status: RuntimeLifecycleStatus::Ready,
            verified: true,
            steps: vec![
                step("resolve", "complete", None),
                step("acquire", "complete", None),
                step("verify", "complete", Some(&distribution.digest)),
                step("prepare", "complete", None),
                step("available", "ready", None),
            ],
        })
    }

    fn empty_manifest(&self) -> RuntimeManifest {
        let platform = PlatformIdentity::current();
        let lock = include_bytes!("../../../distribution/runtime-lock.json");
        // Runtime preparation is mutable provider state, not a new Compute
        // distribution. Keep the provider distribution identity stable while
        // exact runtime distribution identities are recorded independently.
        let lock_identity = sha256_identity(lock);
        let descriptor = serde_json::to_vec(&serde_json::json!({
            "kind": "source-development",
            "compute_version": env!("CARGO_PKG_VERSION"),
            "platform": platform.label(),
            "runtime_lock": lock_identity,
        }))
        .expect("source distribution descriptor serializes");
        RuntimeManifest {
            schema_version: 2,
            compute_version: env!("CARGO_PKG_VERSION").into(),
            distribution_id: sha256_identity(&descriptor),
            distribution_version: format!(
                "compute-{}-{}",
                env!("CARGO_PKG_VERSION"),
                platform.label()
            ),
            platform: platform.label(),
            os: platform.os,
            architecture: platform.architecture,
            runtime_lock_sha256: format!("{:x}", Sha256::digest(lock)),
            certification_status: "runtime-prepared".into(),
            build: serde_json::json!({"format": "compute-runtime-provider-v1", "reproducible": true}),
            runtimes: BTreeMap::new(),
        }
    }

    fn read_manifest(&self) -> Result<RuntimeManifest, ProviderError> {
        let bytes = fs::read(self.root.join("runtime-manifest.json")).map_err(io_error)?;
        serde_json::from_slice(&bytes).map_err(|error| unavailable(error.to_string()))
    }

    fn failure_path(&self, id: &str) -> PathBuf {
        self.root
            .join("failures")
            .join(id.trim_start_matches("sha256:"))
    }
}

fn acquire(url: &str, cached: &Path, expected: &str) -> Result<(), ProviderError> {
    if cached.is_file() {
        let actual = sha256_file(cached)?;
        return if actual == expected {
            Ok(())
        } else {
            Err(unavailable(format!(
                "cached runtime artifact digest mismatch: expected {expected}, got {actual}"
            )))
        };
    }
    let temporary = cached.with_extension("download");
    let result = if let Some(path) = url.strip_prefix("file://") {
        fs::copy(path, &temporary).map(|_| ()).map_err(io_error)
    } else {
        let status = Command::new("curl")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--output",
            ])
            .arg(&temporary)
            .arg(url)
            .status()
            .map_err(io_error)?;
        if status.success() {
            Ok(())
        } else {
            Err(unavailable(format!("runtime acquisition failed: {url}")))
        }
    };
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let actual = sha256_file(&temporary)?;
    if actual != expected {
        let _ = fs::remove_file(&temporary);
        return Err(unavailable(format!(
            "runtime artifact digest mismatch: expected sha256:{expected}, got sha256:{actual}"
        )));
    }
    fs::rename(temporary, cached).map_err(io_error)
}

fn unpack(artifact: &Path, format: ArchiveFormat, destination: &Path) -> Result<(), ProviderError> {
    if matches!(format, ArchiveFormat::File) {
        return fs::copy(artifact, destination.join("artifact"))
            .map(|_| ())
            .map_err(io_error);
    }
    let (program, args): (&str, Vec<&OsStr>) = match format {
        ArchiveFormat::TarGz | ArchiveFormat::Apk => (
            "tar",
            vec![
                OsStr::new("-xzf"),
                artifact.as_os_str(),
                OsStr::new("-C"),
                destination.as_os_str(),
            ],
        ),
        ArchiveFormat::TarXz => (
            "tar",
            vec![
                OsStr::new("-xJf"),
                artifact.as_os_str(),
                OsStr::new("-C"),
                destination.as_os_str(),
            ],
        ),
        ArchiveFormat::Zip => (
            "unzip",
            vec![
                OsStr::new("-q"),
                artifact.as_os_str(),
                OsStr::new("-d"),
                destination.as_os_str(),
            ],
        ),
        ArchiveFormat::File => unreachable!(),
    };
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(io_error)?;
    if status.success() {
        Ok(())
    } else {
        Err(unavailable(format!(
            "{program} failed to unpack runtime artifact"
        )))
    }
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), ProviderError> {
    if source.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        fs::copy(source, destination).map_err(io_error)?;
        fs::set_permissions(
            destination,
            fs::metadata(source).map_err(io_error)?.permissions(),
        )
        .map_err(io_error)?;
        return Ok(());
    }
    if !source.is_dir() {
        return Err(unavailable(format!(
            "runtime install source is missing: {}",
            source.display()
        )));
    }
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|error| unavailable(error.to_string()))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| unavailable(error.to_string()))?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(io_error)?;
        } else if entry.file_type().is_symlink() {
            let link = fs::read_link(entry.path()).map_err(io_error)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(link, target).map_err(io_error)?;
            #[cfg(not(unix))]
            return Err(unavailable("runtime symlinks require Unix"));
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(io_error)?;
            }
            fs::copy(entry.path(), &target).map_err(io_error)?;
            fs::set_permissions(
                &target,
                fs::metadata(entry.path()).map_err(io_error)?.permissions(),
            )
            .map_err(io_error)?;
        }
    }
    Ok(())
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, ProviderError> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(unavailable(format!(
            "non-portable runtime path: {relative}"
        )));
    }
    Ok(root.join(path))
}

fn hash_tree(root: &Path) -> Result<String, ProviderError> {
    if !root.is_dir() {
        return Err(unavailable(format!(
            "missing runtime payload: {}",
            root.display()
        )));
    }
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| unavailable(error.to_string()))?;
    entries.sort_by_key(|entry| entry.path().strip_prefix(root).unwrap().to_path_buf());
    let mut digest = Sha256::new();
    for entry in entries.into_iter().filter(|entry| entry.path() != root) {
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| unavailable(error.to_string()))?;
        digest.update(relative.to_string_lossy().as_bytes());
        if entry.file_type().is_file() {
            digest.update(b"f\0");
            let mut file = File::open(entry.path()).map_err(io_error)?;
            std::io::copy(&mut file, &mut DigestWriter(&mut digest)).map_err(io_error)?;
        } else if entry.file_type().is_symlink() {
            digest.update(b"l\0");
            digest.update(
                fs::read_link(entry.path())
                    .map_err(io_error)?
                    .to_string_lossy()
                    .as_bytes(),
            );
        } else {
            digest.update(b"d\0");
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn sha256_file(path: &Path) -> Result<String, ProviderError> {
    let mut input = File::open(path).map_err(io_error)?;
    let mut digest = Sha256::new();
    std::io::copy(&mut input, &mut DigestWriter(&mut digest)).map_err(io_error)?;
    Ok(format!("{:x}", digest.finalize()))
}

struct DigestWriter<'a>(&'a mut Sha256);
impl Write for DigestWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), ProviderError> {
    let parent = path
        .parent()
        .ok_or_else(|| unavailable("invalid runtime state path"))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(io_error)?;
    serde_json::to_writer_pretty(&mut temporary, value)
        .map_err(|error| unavailable(error.to_string()))?;
    temporary.write_all(b"\n").map_err(io_error)?;
    temporary.flush().map_err(io_error)?;
    temporary
        .persist(path)
        .map_err(|error| io_error(error.error))?;
    Ok(())
}

fn step(name: &str, status: &str, detail: Option<&str>) -> RuntimePreparationStep {
    RuntimePreparationStep {
        name: name.into(),
        status: status.into(),
        detail: detail.map(str::to_owned),
    }
}

fn unavailable(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::DistributionUnavailable, message)
}

fn io_error(error: std::io::Error) -> ProviderError {
    unavailable(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use compute_core::RuntimeKind;

    fn fixture() -> (tempfile::TempDir, RuntimeManager, RuntimeDistribution) {
        let directory = tempfile::tempdir().unwrap();
        let artifact = directory.path().join("node");
        fs::write(
            &artifact,
            b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo v24.18.0; else /bin/sh \"$1\"; fi\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&artifact, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let digest = sha256_file(&artifact).unwrap();
        let platform = PlatformIdentity {
            runtime_abi: None,
            ..PlatformIdentity::current()
        };
        let lock = RuntimeLock {
            schema_version: 2,
            runtimes: BTreeMap::from([(
                "node".into(),
                LockedRuntime {
                    version: "24.18.0".into(),
                    executable: "runtimes/node/bin/node".into(),
                    artifacts: BTreeMap::from([(
                        platform.label(),
                        LockedArtifact {
                            url: format!("file://{}", artifact.display()),
                            sha256: digest,
                            format: ArchiveFormat::File,
                            install: vec![InstallMapping {
                                source: "artifact".into(),
                                destination: "runtimes/node/bin/node".into(),
                            }],
                        },
                    )]),
                },
            )]),
        };
        let manager = RuntimeManager {
            root: directory.path().join("store"),
            lock,
            mutation: Mutex::new(()),
        };
        let resolution = manager.resolve(
            ProviderRuntimeRequirement {
                runtime: RuntimeKind::Node,
                version: Some("24".into()),
                platform: Some(platform),
            },
            RuntimeCapabilities::process(),
        );
        assert_eq!(resolution.status, RuntimeLifecycleStatus::Available);
        (directory, manager, resolution.distribution.unwrap())
    }

    #[test]
    fn preparation_is_verified_persistent_and_tamper_evident() {
        let (_directory, manager, distribution) = fixture();
        let compute = compute_runtime::Compute::with_distribution_root(manager.root.clone());
        let identity_before = compute.installed_distribution_identity().unwrap();
        let prepared = manager.prepare(&distribution).unwrap();
        assert!(prepared.verified);
        assert_eq!(prepared.status, RuntimeLifecycleStatus::Ready);
        let identity_after = compute.installed_distribution_identity().unwrap();
        assert_eq!(identity_after.id, identity_before.id);
        assert_eq!(identity_after.platform, identity_before.platform);

        let restarted = RuntimeManager {
            root: manager.root.clone(),
            lock: manager.lock.clone(),
            mutation: Mutex::new(()),
        };
        assert_eq!(
            restarted.status(&distribution).status,
            RuntimeLifecycleStatus::Ready
        );
        fs::write(restarted.root.join("runtimes/node/bin/node"), b"tampered").unwrap();
        assert_eq!(
            restarted.status(&distribution).status,
            RuntimeLifecycleStatus::Failed
        );
    }

    #[test]
    fn digest_mismatch_fails_without_creating_a_runnable_distribution() {
        let (_directory, manager, mut distribution) = fixture();
        distribution.digest = compute_core::sha256_identity(b"wrong");
        distribution.id = distribution.canonical_id().unwrap();
        let error = manager.prepare(&distribution).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::DistributionUnavailable);
        assert!(!manager.root.join("runtime-manifest.json").exists());
        assert!(!manager.root.join("runtimes/node/bin/node").exists());
    }

    #[test]
    fn platform_and_version_mismatches_are_unsupported() {
        let (_directory, manager, distribution) = fixture();
        let wrong_platform = manager.resolve(
            ProviderRuntimeRequirement {
                runtime: RuntimeKind::Node,
                version: Some("24".into()),
                platform: Some(PlatformIdentity {
                    os: distribution.platform.os.clone(),
                    architecture: "not-this-architecture".into(),
                    runtime_abi: None,
                }),
            },
            RuntimeCapabilities::process(),
        );
        assert_eq!(wrong_platform.status, RuntimeLifecycleStatus::Unsupported);
        let wrong_version = manager.resolve(
            ProviderRuntimeRequirement {
                runtime: RuntimeKind::Node,
                version: Some("23".into()),
                platform: Some(distribution.platform),
            },
            RuntimeCapabilities::process(),
        );
        assert_eq!(wrong_version.status, RuntimeLifecycleStatus::Unsupported);
    }

    #[test]
    fn catalog_resolution_is_runtime_agnostic_and_accepts_constraints() {
        let (_directory, mut manager, _) = fixture();
        let node = manager.lock.runtimes["node"].clone();
        manager.lock.runtimes.extend([
            (
                "python".into(),
                LockedRuntime {
                    version: "3.13.15".into(),
                    ..node.clone()
                },
            ),
            (
                "deno".into(),
                LockedRuntime {
                    version: "2.9.7".into(),
                    ..node.clone()
                },
            ),
            (
                "bun".into(),
                LockedRuntime {
                    version: "1.4.2".into(),
                    ..node
                },
            ),
        ]);
        let platform = PlatformIdentity {
            runtime_abi: None,
            ..PlatformIdentity::current()
        };
        for (runtime, requirement) in [
            (RuntimeKind::Python, ">=3.12,<3.14"),
            (RuntimeKind::Deno, ">=2.9,<3"),
            (RuntimeKind::Bun, ">=1.4,<2"),
        ] {
            let resolution = manager.resolve(
                ProviderRuntimeRequirement {
                    runtime,
                    version: Some(requirement.into()),
                    platform: Some(platform.clone()),
                },
                RuntimeCapabilities::process(),
            );
            assert_eq!(resolution.status, RuntimeLifecycleStatus::Available);
            assert_eq!(resolution.distribution.unwrap().runtime, runtime);
        }
    }

    #[test]
    fn portable_catalog_covers_both_linux_architectures() {
        let lock: RuntimeLock =
            serde_json::from_slice(include_bytes!("../../../distribution/runtime-lock.json"))
                .unwrap();
        for runtime in ["node", "python", "deno", "bun"] {
            let entry = &lock.runtimes[runtime];
            for platform in ["linux-x86_64", "linux-aarch64"] {
                let artifact = entry
                    .artifacts
                    .get(platform)
                    .unwrap_or_else(|| panic!("catalog is missing {runtime} for {platform}"));
                assert_eq!(artifact.sha256.len(), 64);
                assert!(
                    artifact
                        .sha256
                        .chars()
                        .all(|character| character.is_ascii_hexdigit())
                );
                assert!(artifact.url.starts_with("https://"));
                assert!(!artifact.install.is_empty());
            }
        }
    }
}
