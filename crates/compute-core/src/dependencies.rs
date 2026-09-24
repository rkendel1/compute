use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::{ComputeError, Result, RuntimeKind, validate_sha256_identity};

pub const DEPENDENCY_CAPSULE_FORMAT: &str = "compute.deps";
pub const DEPENDENCY_CAPSULE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformIdentity {
    pub os: String,
    pub architecture: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_abi: Option<String>,
}

impl PlatformIdentity {
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            runtime_abi: None,
        }
    }

    pub fn label(&self) -> String {
        format!("{}-{}", self.os, self.architecture)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyEntry {
    pub name: String,
    pub version: String,
    pub source: String,
    pub file_count: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyProvenance {
    pub resolver: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_identity: Option<String>,
    pub source_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyFileManifest {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyFile {
    pub path: PathBuf,
    #[serde(with = "crate::bytes_json")]
    pub data: Vec<u8>,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyCapsule {
    pub runtime: RuntimeKind,
    pub runtime_version: Option<String>,
    pub platform: PlatformIdentity,
    pub dependencies: Vec<DependencyEntry>,
    pub provenance: DependencyProvenance,
    pub files: Vec<DependencyFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyCapsuleManifest {
    pub format: String,
    pub version: u32,
    pub capsule_id: String,
    pub runtime: RuntimeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    pub platform: PlatformIdentity,
    pub dependencies: Vec<DependencyEntry>,
    pub provenance: DependencyProvenance,
    pub files: Vec<DependencyFileManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyCapsuleInspection {
    pub format: String,
    pub version: u32,
    pub capsule_id: String,
    pub runtime: RuntimeKind,
    pub runtime_version: Option<String>,
    pub platform: PlatformIdentity,
    pub file_count: u64,
    pub size_bytes: u64,
    pub dependency_count: u64,
    pub lock_identity: Option<String>,
    pub valid: bool,
}

pub trait DependencyResolver {
    fn resolve(&self, request: DependencyRequest) -> Result<ResolvedDependencies>;
}

#[derive(Debug, Clone)]
pub struct DependencyRequest {
    pub runtime: RuntimeKind,
    pub lockfile: Option<PathBuf>,
    pub output_directory: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ResolvedDependencies {
    pub root: PathBuf,
    pub dependencies: Vec<DependencyEntry>,
    pub resolver: String,
}

impl DependencyCapsule {
    pub fn create(
        root: &Path,
        runtime: RuntimeKind,
        runtime_version: Option<String>,
        platform: PlatformIdentity,
        mut dependencies: Vec<DependencyEntry>,
        lockfile: Option<&Path>,
    ) -> Result<Self> {
        let root = root.canonicalize().map_err(|error| {
            ComputeError::InvalidDependencyCapsule(format!(
                "resolved dependency directory is unavailable: {error}"
            ))
        })?;
        if !root.is_dir() {
            return Err(ComputeError::InvalidDependencyCapsule(
                "resolved dependency payload must be a directory".into(),
            ));
        }
        let mut files = Vec::new();
        let mut case_paths = BTreeSet::new();
        for entry in WalkDir::new(&root).follow_links(false).min_depth(1) {
            let entry =
                entry.map_err(|error| ComputeError::InvalidDependencyCapsule(error.to_string()))?;
            if entry.file_type().is_symlink() {
                return Err(ComputeError::InvalidDependencyCapsule(format!(
                    "dependency payload may not contain symbolic links: {}",
                    entry.path().display()
                )));
            }
            if entry.file_type().is_dir() {
                continue;
            }
            if !entry.file_type().is_file() {
                return Err(ComputeError::InvalidDependencyCapsule(format!(
                    "dependency payload may contain only regular files: {}",
                    entry.path().display()
                )));
            }
            let path = entry
                .path()
                .strip_prefix(&root)
                .expect("walk entry is below root")
                .to_path_buf();
            validate_dependency_path(&path)?;
            let folded = path.to_string_lossy().to_lowercase();
            if !case_paths.insert(folded) {
                return Err(ComputeError::InvalidDependencyCapsule(format!(
                    "case-colliding dependency path: {}",
                    path.display()
                )));
            }
            files.push(DependencyFile {
                path,
                data: fs::read(entry.path())?,
                executable: executable(entry.path())?,
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        dependencies.sort_by(|left, right| {
            (&left.name, &left.version, &left.source).cmp(&(
                &right.name,
                &right.version,
                &right.source,
            ))
        });
        let source_identity = payload_identity(&files);
        for dependency in &mut dependencies {
            if dependency.source == "resolved" {
                dependency.file_count = files.len() as u64;
                dependency.sha256 = source_identity.clone();
            }
        }
        for dependency in &dependencies {
            validate_sha256_identity(&dependency.sha256)?;
        }
        let lock_identity = lockfile
            .map(fs::read)
            .transpose()?
            .map(|bytes| identity(&bytes));
        let capsule = Self {
            runtime,
            runtime_version,
            platform,
            dependencies,
            provenance: DependencyProvenance {
                resolver: "external".into(),
                lock_identity,
                source_identity,
            },
            files,
        };
        capsule.validate()?;
        Ok(capsule)
    }

    pub fn read(path: &Path) -> Result<Self> {
        Self::from_bytes(&fs::read(path)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let mut entries = BTreeMap::<PathBuf, Vec<u8>>::new();
        for entry in archive.entries().map_err(capsule_io)? {
            let mut entry = entry.map_err(capsule_io)?;
            if !entry.header().entry_type().is_file() {
                return Err(ComputeError::InvalidDependencyCapsule(
                    "capsule archive may contain only regular files".into(),
                ));
            }
            let path = entry.path().map_err(capsule_io)?.into_owned();
            validate_dependency_path(&path)?;
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(capsule_io)?;
            if entries.insert(path.clone(), data).is_some() {
                return Err(ComputeError::InvalidDependencyCapsule(format!(
                    "duplicate capsule archive entry: {}",
                    path.display()
                )));
            }
        }
        let manifest_data = entries.remove(Path::new("manifest.json")).ok_or_else(|| {
            ComputeError::InvalidDependencyCapsule("missing capsule manifest".into())
        })?;
        let manifest: DependencyCapsuleManifest = serde_json::from_slice(&manifest_data)?;
        if manifest.format != DEPENDENCY_CAPSULE_FORMAT
            || manifest.version != DEPENDENCY_CAPSULE_VERSION
        {
            return Err(ComputeError::InvalidDependencyCapsule(format!(
                "unsupported capsule format/version: {}@{}",
                manifest.format, manifest.version
            )));
        }
        let mut files = Vec::new();
        for declared in &manifest.files {
            let archive_path = Path::new("files").join(&declared.path);
            let data = entries.remove(&archive_path).ok_or_else(|| {
                ComputeError::InvalidDependencyCapsule(format!(
                    "missing dependency file: {}",
                    declared.path.display()
                ))
            })?;
            files.push(DependencyFile {
                path: declared.path.clone(),
                data,
                executable: declared.executable,
            });
        }
        if let Some(path) = entries.keys().next() {
            return Err(ComputeError::InvalidDependencyCapsule(format!(
                "unexpected capsule archive entry: {}",
                path.display()
            )));
        }
        let capsule = Self {
            runtime: manifest.runtime,
            runtime_version: manifest.runtime_version.clone(),
            platform: manifest.platform.clone(),
            dependencies: manifest.dependencies.clone(),
            provenance: manifest.provenance.clone(),
            files,
        };
        capsule.validate()?;
        if capsule.manifest()? != manifest {
            return Err(ComputeError::InvalidDependencyCapsule(
                "capsule manifest or identity mismatch".into(),
            ));
        }
        if capsule.to_bytes()? != bytes {
            return Err(ComputeError::InvalidDependencyCapsule(
                "capsule archive is not canonical".into(),
            ));
        }
        Ok(capsule)
    }

    pub fn validate(&self) -> Result<()> {
        if self.runtime_version.as_deref().is_none_or(str::is_empty) {
            return Err(ComputeError::InvalidDependencyCapsule(
                "dependency capsule must declare an exact runtime version".into(),
            ));
        }
        if !matches!(
            self.runtime,
            RuntimeKind::Python
                | RuntimeKind::Node
                | RuntimeKind::Ruby
                | RuntimeKind::Jvm
                | RuntimeKind::Dotnet
        ) {
            return Err(ComputeError::InvalidDependencyCapsule(format!(
                "dependency capsules are not supported for runtime {}",
                self.runtime
            )));
        }
        for (label, value) in [
            ("platform OS", self.platform.os.as_str()),
            ("platform architecture", self.platform.architecture.as_str()),
            ("resolver", self.provenance.resolver.as_str()),
        ] {
            if value.is_empty() || value.contains('\0') {
                return Err(ComputeError::InvalidDependencyCapsule(format!(
                    "invalid {label}"
                )));
            }
        }
        let mut dependency_keys = BTreeSet::new();
        for dependency in &self.dependencies {
            if dependency.name.is_empty()
                || dependency.version.is_empty()
                || dependency.source.is_empty()
                || dependency.name.contains('\0')
                || dependency.version.contains('\0')
                || dependency.source.contains('\0')
            {
                return Err(ComputeError::InvalidDependencyCapsule(
                    "dependency inventory contains malformed metadata".into(),
                ));
            }
            if !dependency_keys.insert((
                dependency.name.clone(),
                dependency.version.clone(),
                dependency.source.clone(),
            )) {
                return Err(ComputeError::InvalidDependencyCapsule(format!(
                    "duplicate dependency inventory entry: {}",
                    dependency.name
                )));
            }
            validate_sha256_identity(&dependency.sha256)?;
        }
        if self.files.is_empty() {
            return Err(ComputeError::InvalidDependencyCapsule(
                "dependency capsule payload is empty".into(),
            ));
        }
        let mut paths = BTreeSet::new();
        let mut folded = BTreeSet::new();
        for file in &self.files {
            validate_dependency_path(&file.path)?;
            if !paths.insert(file.path.clone())
                || !folded.insert(file.path.to_string_lossy().to_lowercase())
            {
                return Err(ComputeError::InvalidDependencyCapsule(format!(
                    "duplicate or case-colliding dependency file: {}",
                    file.path.display()
                )));
            }
        }
        if self.provenance.source_identity != payload_identity(&self.files) {
            return Err(ComputeError::InvalidDependencyCapsule(
                "dependency source identity mismatch".into(),
            ));
        }
        validate_sha256_identity(&self.provenance.source_identity)?;
        if let Some(lock) = &self.provenance.lock_identity {
            validate_sha256_identity(lock)?;
        }
        Ok(())
    }

    pub fn capsule_id(&self) -> Result<String> {
        self.validate()?;
        let mut hasher = Sha256::new();
        hash_field(&mut hasher, DEPENDENCY_CAPSULE_FORMAT.as_bytes());
        hash_field(&mut hasher, &DEPENDENCY_CAPSULE_VERSION.to_be_bytes());
        hash_field(&mut hasher, self.runtime.as_str().as_bytes());
        hash_field(
            &mut hasher,
            self.runtime_version.as_deref().unwrap_or("").as_bytes(),
        );
        hash_field(&mut hasher, &serde_json::to_vec(&self.platform)?);
        hash_field(&mut hasher, &serde_json::to_vec(&self.dependencies)?);
        hash_field(&mut hasher, &serde_json::to_vec(&self.provenance)?);
        let mut files = self.files.clone();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        for file in files {
            hash_field(&mut hasher, file.path.to_string_lossy().as_bytes());
            hash_field(&mut hasher, &[u8::from(file.executable)]);
            hash_field(&mut hasher, &file.data);
        }
        Ok(format!("sha256:{:x}", hasher.finalize()))
    }

    pub fn manifest(&self) -> Result<DependencyCapsuleManifest> {
        let mut files = self
            .files
            .iter()
            .map(|file| DependencyFileManifest {
                path: file.path.clone(),
                size: file.data.len() as u64,
                sha256: identity(&file.data),
                executable: file.executable,
            })
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(DependencyCapsuleManifest {
            format: DEPENDENCY_CAPSULE_FORMAT.into(),
            version: DEPENDENCY_CAPSULE_VERSION,
            capsule_id: self.capsule_id()?,
            runtime: self.runtime,
            runtime_version: self.runtime_version.clone(),
            platform: self.platform.clone(),
            dependencies: self.dependencies.clone(),
            provenance: self.provenance.clone(),
            files,
        })
    }

    pub fn inspection(&self) -> Result<DependencyCapsuleInspection> {
        let manifest = self.manifest()?;
        Ok(DependencyCapsuleInspection {
            format: manifest.format,
            version: manifest.version,
            capsule_id: manifest.capsule_id,
            runtime: manifest.runtime,
            runtime_version: manifest.runtime_version,
            platform: manifest.platform,
            file_count: manifest.files.len() as u64,
            size_bytes: manifest.files.iter().map(|file| file.size).sum(),
            dependency_count: manifest.dependencies.len() as u64,
            lock_identity: manifest.provenance.lock_identity,
            valid: true,
        })
    }

    pub fn write(&self, path: &Path) -> Result<u64> {
        let bytes = self.to_bytes()?;
        fs::write(path, &bytes)?;
        Ok(bytes.len() as u64)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut entries = vec![(
            PathBuf::from("manifest.json"),
            serde_json::to_vec(&self.manifest()?)?,
            false,
        )];
        entries.extend(self.files.iter().map(|file| {
            (
                Path::new("files").join(&file.path),
                file.data.clone(),
                file.executable,
            )
        }));
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            builder.mode(tar::HeaderMode::Deterministic);
            for (path, data, executable) in entries {
                validate_dependency_path(&path)?;
                let mut header = tar::Header::new_ustar();
                header.set_path(path).map_err(capsule_io)?;
                header.set_size(data.len() as u64);
                header.set_mode(if executable { 0o755 } else { 0o644 });
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                header.set_cksum();
                builder
                    .append(&header, Cursor::new(data))
                    .map_err(capsule_io)?;
            }
            builder.finish().map_err(capsule_io)?;
        }
        Ok(bytes)
    }

    pub fn require_compatible(&self, runtime: RuntimeKind) -> Result<()> {
        if self.runtime != runtime {
            return Err(ComputeError::InvalidDependencyCapsule(format!(
                "dependency capsule runtime {} does not match workload runtime {runtime}",
                self.runtime
            )));
        }
        let current = PlatformIdentity::current();
        if self.platform.os != current.os || self.platform.architecture != current.architecture {
            return Err(ComputeError::InvalidDependencyCapsule(format!(
                "dependency capsule platform {} does not match execution platform {}",
                self.platform.label(),
                current.label()
            )));
        }
        Ok(())
    }
}

fn validate_dependency_path(path: &Path) -> Result<()> {
    let text = path.to_str().ok_or_else(|| {
        ComputeError::InvalidDependencyCapsule("dependency paths must be UTF-8".into())
    })?;
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || text.contains('\\')
        || text.as_bytes().get(1) == Some(&b':')
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(ComputeError::InvalidDependencyCapsule(format!(
            "invalid dependency path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn payload_identity(files: &[DependencyFile]) -> String {
    let mut files = files.to_vec();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let mut hasher = Sha256::new();
    for file in files {
        hash_field(&mut hasher, file.path.to_string_lossy().as_bytes());
        hash_field(&mut hasher, &[u8::from(file.executable)]);
        hash_field(&mut hasher, &file.data);
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn identity(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(unix)]
fn executable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(_path: &Path) -> Result<bool> {
    Ok(false)
}

fn capsule_io(error: std::io::Error) -> ComputeError {
    ComputeError::InvalidDependencyCapsule(format!("invalid capsule archive: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, DependencyCapsule) {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("package")).unwrap();
        fs::write(root.path().join("package/module.py"), "VALUE = 42\n").unwrap();
        fs::write(root.path().join("metadata.txt"), "resolved\n").unwrap();
        let capsule = DependencyCapsule::create(
            root.path(),
            RuntimeKind::Python,
            Some("test-runtime".into()),
            PlatformIdentity::current(),
            vec![],
            None,
        )
        .unwrap();
        (root, capsule)
    }

    #[test]
    fn capsule_archive_and_identity_are_reproducible() {
        let (root, first) = fixture();
        let second = DependencyCapsule::create(
            root.path(),
            RuntimeKind::Python,
            Some("test-runtime".into()),
            PlatformIdentity::current(),
            vec![],
            None,
        )
        .unwrap();
        assert_eq!(first.capsule_id().unwrap(), second.capsule_id().unwrap());
        assert_eq!(first.to_bytes().unwrap(), second.to_bytes().unwrap());
        let parsed = DependencyCapsule::from_bytes(&first.to_bytes().unwrap()).unwrap();
        assert_eq!(parsed, first);
    }

    #[test]
    fn capsule_identity_covers_payload_and_bindings() {
        let (root, first) = fixture();
        fs::write(root.path().join("package/module.py"), "VALUE = 43\n").unwrap();
        let modified = DependencyCapsule::create(
            root.path(),
            RuntimeKind::Python,
            Some("test-runtime".into()),
            PlatformIdentity::current(),
            vec![],
            None,
        )
        .unwrap();
        assert_ne!(first.capsule_id().unwrap(), modified.capsule_id().unwrap());

        let mut wrong_runtime = first.clone();
        wrong_runtime.runtime = RuntimeKind::Node;
        assert!(
            wrong_runtime
                .require_compatible(RuntimeKind::Python)
                .is_err()
        );
        let mut wrong_platform = first;
        wrong_platform.platform.os = "not-this-os".into();
        assert!(
            wrong_platform
                .require_compatible(RuntimeKind::Python)
                .is_err()
        );
    }

    #[test]
    fn tampering_and_unsafe_sources_fail_closed() {
        let (_root, capsule) = fixture();
        let mut bytes = capsule.to_bytes().unwrap();
        let index = bytes.len() / 2;
        bytes[index] ^= 1;
        assert!(DependencyCapsule::from_bytes(&bytes).is_err());

        #[cfg(unix)]
        {
            let root = tempfile::tempdir().unwrap();
            let outside = root.path().join("outside");
            fs::write(&outside, "outside").unwrap();
            let payload = root.path().join("payload");
            fs::create_dir(&payload).unwrap();
            std::os::unix::fs::symlink(&outside, payload.join("escape")).unwrap();
            assert!(
                DependencyCapsule::create(
                    &payload,
                    RuntimeKind::Python,
                    Some("test-runtime".into()),
                    PlatformIdentity::current(),
                    vec![],
                    None,
                )
                .is_err()
            );
        }
    }
}
