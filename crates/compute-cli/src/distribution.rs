use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use compute_core::{ComputeError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

const LOCK_SCHEMA: u32 = 2;
const MANIFEST_SCHEMA: u32 = 2;

pub struct BuildOptions {
    pub output: PathBuf,
    pub offline: bool,
    pub verify: bool,
    pub cache: Option<PathBuf>,
    pub platform: Option<String>,
    pub lock: Option<PathBuf>,
    pub compute_binary: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeLock {
    schema_version: u32,
    runtimes: BTreeMap<String, LockedRuntime>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedRuntime {
    version: String,
    executable: String,
    #[serde(default)]
    artifacts: BTreeMap<String, LockedArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedArtifact {
    url: String,
    sha256: String,
    format: ArchiveFormat,
    install: Vec<InstallMapping>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ArchiveFormat {
    TarGz,
    TarXz,
    Zip,
    Apk,
    File,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallMapping {
    source: String,
    destination: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DistributionManifest {
    schema_version: u32,
    compute_version: String,
    distribution_id: String,
    distribution_version: String,
    platform: String,
    os: String,
    architecture: String,
    runtime_lock_sha256: String,
    certification_status: String,
    build: BuildMetadata,
    runtimes: BTreeMap<String, ManifestRuntime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildMetadata {
    format: String,
    reproducible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestRuntime {
    version: String,
    executable: String,
    artifact_sha256: String,
    payload_sha256: String,
    reported_version: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct VerificationReport {
    pub(crate) distribution_id: Option<String>,
    platform: Option<String>,
    checks: Vec<VerificationCheck>,
    pub(crate) passed: bool,
}

#[derive(Debug, Serialize)]
struct VerificationCheck {
    name: String,
    passed: bool,
    detail: String,
}

pub fn build(options: BuildOptions) -> Result<()> {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lock_path = options
        .lock
        .unwrap_or_else(|| repository.join("distribution/runtime-lock.json"));
    let lock_bytes = fs::read(&lock_path).map_err(error)?;
    let lock: RuntimeLock = serde_json::from_slice(&lock_bytes).map_err(error)?;
    if lock.schema_version != LOCK_SCHEMA {
        return fail(format!(
            "unsupported runtime lock schema {}; expected {LOCK_SCHEMA}",
            lock.schema_version
        ));
    }
    let platform = options.platform.unwrap_or_else(host_platform);
    let (os, architecture) = split_platform(&platform)?;
    if options.output.exists() {
        return fail(format!(
            "refusing to overwrite {}",
            options.output.display()
        ));
    }
    let parent = options
        .output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(error)?;
    let staging = tempfile::Builder::new()
        .prefix(".compute-distribution-")
        .tempdir_in(parent)
        .map_err(error)?;
    let root = staging.path().join("root");
    fs::create_dir_all(root.join("bin")).map_err(error)?;
    fs::create_dir_all(root.join("runtimes")).map_err(error)?;

    let compute_binary = options
        .compute_binary
        .unwrap_or(std::env::current_exe().map_err(error)?);
    copy_file(&compute_binary, &root.join("bin/compute"))?;
    let cache = options.cache.unwrap_or_else(default_cache);
    fs::create_dir_all(cache.join("sha256")).map_err(error)?;

    let mut runtimes = BTreeMap::new();
    for (name, runtime) in &lock.runtimes {
        let artifact_hash = if runtime.artifacts.is_empty() {
            sha256_file(&root.join("bin/compute"))?
        } else {
            let artifact = runtime.artifacts.get(&platform).ok_or_else(|| {
                ComputeError::Runtime(format!(
                    "unsupported platform {platform}: runtime {name} has no compatible artifact"
                ))
            })?;
            validate_digest(&artifact.sha256)?;
            let cached = cache.join("sha256").join(&artifact.sha256);
            acquire(&artifact.url, &cached, &artifact.sha256, options.offline)?;
            install_artifact(&cached, artifact, &root)?;
            artifact.sha256.clone()
        };
        let executable = safe_join(&root, &runtime.executable)?;
        let (payload_hash, reported_version) = if runtime.executable.starts_with('<') {
            (artifact_hash.clone(), runtime.version.clone())
        } else {
            if !executable.is_file() {
                return fail(format!(
                    "assembled runtime {name} is missing executable {}",
                    executable.display()
                ));
            }
            make_executable(&executable)?;
            let reported = probe(name, &executable, &runtime.version)
                .map_err(|message| ComputeError::Runtime(format!("runtime {name} {message}")))?;
            let runtime_root = root.join("runtimes").join(name);
            (hash_tree(&runtime_root)?, reported)
        };
        runtimes.insert(
            name.clone(),
            ManifestRuntime {
                version: runtime.version.clone(),
                executable: runtime.executable.clone(),
                artifact_sha256: artifact_hash,
                payload_sha256: payload_hash,
                reported_version,
            },
        );
    }

    fs::write(root.join("runtime-lock.json"), &lock_bytes).map_err(error)?;
    let lock_hash = sha256_bytes(&lock_bytes);
    let compute_version = env!("CARGO_PKG_VERSION").to_string();
    let identity = distribution_identity(&compute_version, &platform, &lock_hash, &runtimes)?;
    let mut manifest = DistributionManifest {
        schema_version: MANIFEST_SCHEMA,
        compute_version: compute_version.clone(),
        distribution_id: identity,
        distribution_version: format!("compute-{compute_version}-{platform}"),
        platform,
        os,
        architecture,
        runtime_lock_sha256: lock_hash,
        certification_status: "not_run".into(),
        build: BuildMetadata {
            format: "compute-distribution-v2".into(),
            reproducible: true,
        },
        runtimes,
    };
    write_json(&root.join("runtime-manifest.json"), &manifest)?;
    write_json(&root.join("runtime-inventory.json"), &manifest.runtimes)?;

    if options.verify {
        prepare_certification(&repository, &root, &cache, options.offline)?;
        for (name, runtime) in &mut manifest.runtimes {
            if !runtime.executable.starts_with('<') {
                runtime.payload_sha256 = hash_tree(&root.join("runtimes").join(name))?;
            }
        }
        manifest.distribution_id = distribution_identity(
            &manifest.compute_version,
            &manifest.platform,
            &manifest.runtime_lock_sha256,
            &manifest.runtimes,
        )?;
        write_json(&root.join("runtime-manifest.json"), &manifest)?;
        write_json(&root.join("runtime-inventory.json"), &manifest.runtimes)?;
        run_distribution_command(&root, &["doctor", "--json"])?;
        run_distribution_command(&root, &["certify", "--json"])?;
        manifest.certification_status = "pass".into();
        write_json(&root.join("runtime-manifest.json"), &manifest)?;
    }
    let report = verify_root(&root);
    if !report.passed {
        return fail(format_report(&report));
    }

    fs::rename(&root, &options.output).map_err(error)?;
    let archive = PathBuf::from(format!("{}.tar", options.output.display()));
    write_archive(&options.output, &archive)?;
    println!("{}", options.output.display());
    println!("{}", archive.display());
    Ok(())
}

pub fn inspect(path: &Path, json: bool) -> Result<()> {
    let manifest: DistributionManifest = read_json(&path.join("runtime-manifest.json"))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&manifest).map_err(error)?
        );
    } else {
        println!("Distribution: {}", manifest.distribution_id);
        println!("Platform: {}", manifest.platform);
        println!("Certification: {}", manifest.certification_status);
        for (name, runtime) in manifest.runtimes {
            println!(
                "{name}: {} artifact sha256:{}",
                runtime.version, runtime.artifact_sha256
            );
        }
    }
    Ok(())
}

pub fn verify(path: &Path, json: bool) -> Result<()> {
    let report = verify_root(path);
    if json {
        println!("{}", serde_json::to_string_pretty(&report).map_err(error)?);
    } else {
        for check in &report.checks {
            println!(
                "{}: {} — {}",
                check.name,
                status(check.passed),
                check.detail
            );
        }
        println!("DISTRIBUTION VERIFY: {}", status(report.passed));
    }
    if report.passed {
        Ok(())
    } else {
        fail("distribution verification failed")
    }
}

pub fn doctor_provenance() -> BTreeMap<String, serde_json::Value> {
    let Some(root) = distribution_root() else {
        return BTreeMap::new();
    };
    let Ok(manifest) = read_json::<DistributionManifest>(&root.join("runtime-manifest.json"))
    else {
        return BTreeMap::new();
    };
    manifest
        .runtimes
        .into_iter()
        .map(|(name, runtime)| {
            let actual = if runtime.executable.starts_with('<') {
                Some(runtime.payload_sha256.clone())
            } else {
                hash_tree(&root.join("runtimes").join(&name)).ok()
            };
            let status = if actual.as_deref() == Some(runtime.payload_sha256.as_str()) {
                "pass"
            } else {
                "fail"
            };
            (
                name,
                serde_json::json!({
                    "source": "compute-distribution",
                    "artifact_sha256": runtime.artifact_sha256,
                    "expected_payload_sha256": runtime.payload_sha256,
                    "actual_payload_sha256": actual,
                    "status": status,
                }),
            )
        })
        .collect()
}

fn distribution_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("COMPUTE_HOME") {
        return Some(PathBuf::from(root));
    }
    let executable = std::env::current_exe().ok()?;
    let root = executable.parent()?.parent()?.to_path_buf();
    root.join("runtime-manifest.json").is_file().then_some(root)
}

pub(crate) fn verify_root(root: &Path) -> VerificationReport {
    let mut report = VerificationReport {
        distribution_id: None,
        platform: None,
        checks: Vec::new(),
        passed: false,
    };
    let manifest: DistributionManifest = match read_json(&root.join("runtime-manifest.json")) {
        Ok(value) => value,
        Err(failure) => {
            push_check(&mut report, "manifest", false, failure.to_string());
            return report;
        }
    };
    report.distribution_id = Some(manifest.distribution_id.clone());
    report.platform = Some(manifest.platform.clone());
    push_check(
        &mut report,
        "manifest",
        manifest.schema_version == MANIFEST_SCHEMA,
        "manifest is readable and versioned",
    );
    let lock_bytes = match fs::read(root.join("runtime-lock.json")) {
        Ok(bytes) => bytes,
        Err(failure) => {
            push_check(&mut report, "runtime_lock", false, failure.to_string());
            return report;
        }
    };
    let lock: RuntimeLock = match serde_json::from_slice(&lock_bytes) {
        Ok(value) => value,
        Err(failure) => {
            push_check(&mut report, "runtime_lock", false, failure.to_string());
            return report;
        }
    };
    push_check(
        &mut report,
        "runtime_lock",
        lock.schema_version == LOCK_SCHEMA
            && sha256_bytes(&lock_bytes) == manifest.runtime_lock_sha256,
        "lock hash matches the assembled manifest",
    );
    push_check(
        &mut report,
        "platform",
        manifest.platform == host_platform(),
        format!(
            "distribution is {} and verifier is {}",
            manifest.platform,
            host_platform()
        ),
    );
    let compute_binary = root.join("bin/compute");
    let compute_valid = compute_binary.is_file()
        && Command::new(&compute_binary)
            .args(["version", "--json"])
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains(&manifest.compute_version)
            });
    push_check(
        &mut report,
        "compute_binary",
        compute_valid,
        "Compute executable exists and reports the manifest version",
    );
    let inventory =
        read_json::<BTreeMap<String, ManifestRuntime>>(&root.join("runtime-inventory.json"));
    push_check(
        &mut report,
        "runtime_inventory",
        inventory
            .as_ref()
            .is_ok_and(|value| value == &manifest.runtimes),
        "inventory exactly matches the manifest",
    );
    if lock.runtimes.len() != manifest.runtimes.len() {
        push_check(
            &mut report,
            "runtime_set",
            false,
            "manifest runtime set differs from the lock",
        );
    }
    for (name, locked) in &lock.runtimes {
        let Some(installed) = manifest.runtimes.get(name) else {
            push_check(
                &mut report,
                &format!("runtime:{name}"),
                false,
                "runtime missing from manifest",
            );
            continue;
        };
        let expected_artifact = locked
            .artifacts
            .get(&manifest.platform)
            .map(|artifact| artifact.sha256.as_str());
        let declared = installed.version == locked.version
            && installed.executable == locked.executable
            && expected_artifact.is_none_or(|hash| hash == installed.artifact_sha256);
        if locked.executable.starts_with('<') {
            push_check(
                &mut report,
                &format!("runtime:{name}"),
                declared,
                "embedded/workload runtime declaration matches",
            );
            continue;
        }
        let executable = match safe_join(root, &locked.executable) {
            Ok(path) => path,
            Err(failure) => {
                push_check(
                    &mut report,
                    &format!("runtime:{name}"),
                    false,
                    failure.to_string(),
                );
                continue;
            }
        };
        let executable_exists = executable.is_file();
        let runtime_root = root.join("runtimes").join(name);
        let payload_matches = hash_tree(&runtime_root)
            .map(|value| value == installed.payload_sha256)
            .unwrap_or(false);
        let launched = probe(name, &executable, &locked.version).is_ok();
        push_check(
            &mut report,
            &format!("runtime:{name}"),
            declared && payload_matches && launched,
            if !declared {
                "manifest/lock or artifact hash mismatch"
            } else if !executable_exists {
                "runtime executable is missing"
            } else if !payload_matches {
                "payload hash mismatch"
            } else if !launched {
                "runtime startup/version failure"
            } else {
                "payload hash and runtime identity match"
            },
        );
    }
    let identity = distribution_identity(
        &manifest.compute_version,
        &manifest.platform,
        &manifest.runtime_lock_sha256,
        &manifest.runtimes,
    );
    push_check(
        &mut report,
        "distribution_identity",
        identity
            .as_deref()
            .is_ok_and(|value| value == manifest.distribution_id),
        "portable identity matches actual inventory",
    );
    report.passed = report.checks.iter().all(|check| check.passed);
    report
}

fn acquire(url: &str, cached: &Path, expected: &str, offline: bool) -> Result<()> {
    if cached.is_file() {
        if sha256_file(cached)? == expected {
            return Ok(());
        }
        return fail(format!(
            "cached artifact hash mismatch: {}",
            cached.display()
        ));
    }
    if offline {
        return fail(format!("offline cache miss for sha256:{expected}"));
    }
    let parent = cached
        .parent()
        .ok_or_else(|| ComputeError::Runtime("invalid cache path".into()))?;
    fs::create_dir_all(parent).map_err(error)?;
    let temporary = parent.join(format!(".{expected}.download"));
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
        .map_err(|failure| ComputeError::Runtime(format!("cannot launch curl: {failure}")))?;
    if !status.success() {
        let _ = fs::remove_file(&temporary);
        return fail(format!("artifact download failed: {url}"));
    }
    let actual = sha256_file(&temporary)?;
    if actual != expected {
        let _ = fs::remove_file(&temporary);
        return fail(format!(
            "artifact hash mismatch: expected {expected}, got {actual}"
        ));
    }
    fs::rename(temporary, cached).map_err(error)?;
    Ok(())
}

fn install_artifact(cached: &Path, artifact: &LockedArtifact, root: &Path) -> Result<()> {
    let temporary = tempfile::tempdir().map_err(error)?;
    match artifact.format {
        ArchiveFormat::File => {
            copy_file(cached, &temporary.path().join("artifact"))?;
        }
        ArchiveFormat::TarGz | ArchiveFormat::Apk => run_unpack(
            "tar",
            &[
                OsStr::new("-xzf"),
                cached.as_os_str(),
                OsStr::new("-C"),
                temporary.path().as_os_str(),
            ],
        )?,
        ArchiveFormat::TarXz => run_unpack(
            "tar",
            &[
                OsStr::new("-xJf"),
                cached.as_os_str(),
                OsStr::new("-C"),
                temporary.path().as_os_str(),
            ],
        )?,
        ArchiveFormat::Zip => run_unpack(
            "unzip",
            &[
                OsStr::new("-q"),
                cached.as_os_str(),
                OsStr::new("-d"),
                temporary.path().as_os_str(),
            ],
        )?,
    }
    for mapping in &artifact.install {
        let source = safe_join(temporary.path(), &mapping.source)?;
        let destination = safe_join(root, &mapping.destination)?;
        copy_tree(&source, &destination)?;
    }
    Ok(())
}

fn run_unpack(program: &str, args: &[&OsStr]) -> Result<()> {
    let status = Command::new(program).args(args).status().map_err(error)?;
    if status.success() {
        Ok(())
    } else {
        fail(format!("{program} failed to unpack runtime artifact"))
    }
}

fn prepare_certification(
    repository: &Path,
    root: &Path,
    cache: &Path,
    offline: bool,
) -> Result<()> {
    let script = repository.join("distribution/prepare-certification-fixtures.sh");
    let lock: RuntimeLock = read_json(&root.join("runtime-lock.json"))?;
    let native_version = lock
        .runtimes
        .get("native")
        .ok_or_else(|| ComputeError::Runtime("native runtime is missing from lock".into()))?
        .version
        .clone();
    let wasm_version = lock
        .runtimes
        .get("wasm")
        .ok_or_else(|| ComputeError::Runtime("WASM runtime is missing from lock".into()))?
        .version
        .clone();
    let appport_source = repository.join("packages/compute-appport");
    let appport_build = root.join(".build/compute-appport");
    fs::create_dir_all(appport_build.join("src")).map_err(error)?;
    for name in ["package.json", "package-lock.json", "tsconfig.json"] {
        copy_file(&appport_source.join(name), &appport_build.join(name))?;
    }
    copy_tree(&appport_source.join("src"), &appport_build.join("src"))?;
    let path = [
        root.join("runtimes/jvm/bin"),
        root.join("runtimes/dotnet"),
        root.join("runtimes/node/bin"),
        std::env::var_os("PATH")
            .map(PathBuf::from)
            .unwrap_or_default(),
    ]
    .iter()
    .map(|item| item.to_string_lossy())
    .collect::<Vec<_>>()
    .join(":");
    let npm = root.join("runtimes/node/bin/npm");
    let mut npm_ci = Command::new(&npm);
    npm_ci
        .arg("ci")
        .current_dir(&appport_build)
        .env("PATH", &path)
        .env("npm_config_cache", cache.join("npm"));
    if offline {
        npm_ci.arg("--offline");
    }
    let status = npm_ci.status().map_err(error)?;
    if !status.success() {
        return fail("failed to install locked AppPort dependencies");
    }
    let status = Command::new(&npm)
        .args(["run", "build"])
        .current_dir(&appport_build)
        .env("PATH", &path)
        .env("npm_config_cache", cache.join("npm"))
        .status()
        .map_err(error)?;
    if !status.success() {
        return fail("failed to build the AppPort certification runner");
    }
    let status = Command::new(&script)
        .arg(root)
        .env("PATH", path)
        .env("COMPUTE_APPPORT_ROOT", &appport_build)
        .env("COMPUTE_NATIVE_VERSION", native_version)
        .env("COMPUTE_WASM_VERSION", wasm_version)
        .status()
        .map_err(error)?;
    if status.success() {
        fs::remove_dir_all(root.join(".build")).map_err(error)
    } else {
        fail("failed to build certification fixtures from pinned runtimes")
    }
}

fn run_distribution_command(root: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new(root.join("bin/compute"))
        .args(args)
        .env("COMPUTE_HOME", root)
        .env("COMPUTE_REQUIRE_ALL_RUNTIMES", "1")
        .status()
        .map_err(error)?;
    if status.success() {
        Ok(())
    } else {
        fail(format!("compute {} failed", args.join(" ")))
    }
}

fn probe(runtime: &str, executable: &Path, expected: &str) -> std::result::Result<String, String> {
    let probe_argument = match runtime {
        "dotnet" => "--list-runtimes",
        "shell" => "--help",
        _ => "--version",
    };
    let output = Command::new(executable)
        .arg(probe_argument)
        .output()
        .map_err(|failure| format!("startup failure: {failure}"))?;
    let detected = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .trim()
    .to_string();
    if !output.status.success() {
        return Err(format!("startup failure: {detected}"));
    }
    if !detected.contains(expected) {
        return Err(format!(
            "version mismatch: expected {expected}, detected {detected}"
        ));
    }
    if runtime == "dotnet" {
        Ok(detected
            .lines()
            .map(|line| line.split_once(" [").map_or(line, |(name, _)| name))
            .collect::<Vec<_>>()
            .join("\n"))
    } else {
        Ok(detected)
    }
}

fn distribution_identity(
    compute: &str,
    platform: &str,
    lock: &str,
    runtimes: &BTreeMap<String, ManifestRuntime>,
) -> Result<String> {
    let bytes = serde_json::to_vec(&(compute, platform, lock, runtimes)).map_err(error)?;
    Ok(format!("sha256:{}", sha256_bytes(&bytes)))
}

fn hash_tree(root: &Path) -> Result<String> {
    if !root.is_dir() {
        return fail(format!("missing runtime payload: {}", root.display()));
    }
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(error)?;
    entries.sort_by_key(|entry| entry.path().strip_prefix(root).unwrap().to_path_buf());
    let mut digest = Sha256::new();
    for entry in entries.into_iter().filter(|entry| entry.path() != root) {
        let relative = entry.path().strip_prefix(root).map_err(error)?;
        digest.update(relative.to_string_lossy().as_bytes());
        if entry.file_type().is_file() {
            digest.update(b"f\0");
            let mut file = File::open(entry.path()).map_err(error)?;
            std::io::copy(&mut file, &mut DigestWriter(&mut digest)).map_err(error)?;
        } else if entry.file_type().is_symlink() {
            digest.update(b"l\0");
            digest.update(
                fs::read_link(entry.path())
                    .map_err(error)?
                    .to_string_lossy()
                    .as_bytes(),
            );
        } else {
            digest.update(b"d\0");
        }
    }
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

fn write_archive(root: &Path, archive: &Path) -> Result<()> {
    let file = File::create(archive).map_err(error)?;
    let mut builder = tar::Builder::new(file);
    builder.mode(tar::HeaderMode::Deterministic);
    let base = OsStr::new("compute-distribution");
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(error)?;
    entries.sort_by_key(|entry| entry.path().strip_prefix(root).unwrap().to_path_buf());
    for entry in entries {
        let relative = entry.path().strip_prefix(root).map_err(error)?;
        let archive_path = Path::new(base).join(relative);
        let metadata = fs::symlink_metadata(entry.path()).map_err(error)?;
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        if metadata.file_type().is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header.set_cksum();
            builder
                .append_data(&mut header, archive_path, std::io::empty())
                .map_err(error)?;
        } else if metadata.file_type().is_symlink() {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            header.set_size(0);
            header
                .set_link_name(fs::read_link(entry.path()).map_err(error)?)
                .map_err(error)?;
            header.set_cksum();
            builder
                .append_data(&mut header, archive_path, std::io::empty())
                .map_err(error)?;
        } else {
            let mut input = File::open(entry.path()).map_err(error)?;
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(if is_executable(&metadata) {
                0o755
            } else {
                0o644
            });
            header.set_size(metadata.len());
            header.set_cksum();
            builder
                .append_data(&mut header, archive_path, &mut input)
                .map_err(error)?;
        }
    }
    builder.finish().map_err(error)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    if source.is_file() {
        return copy_file(source, destination);
    }
    if !source.is_dir() {
        return fail(format!(
            "artifact install source is missing: {}",
            source.display()
        ));
    }
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(error)?;
        let relative = entry.path().strip_prefix(source).map_err(error)?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(error)?;
        } else if entry.file_type().is_symlink() {
            let link = fs::read_link(entry.path()).map_err(error)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link, &target).map_err(|failure| {
                ComputeError::Runtime(format!(
                    "cannot install symlink {} -> {}: {failure}",
                    target.display(),
                    link.display()
                ))
            })?;
            #[cfg(not(unix))]
            return fail("runtime archives containing symlinks require Unix");
        } else {
            copy_file(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(error)?;
    }
    fs::copy(source, destination).map_err(error)?;
    let permissions = fs::metadata(source).map_err(error)?.permissions();
    fs::set_permissions(destination, permissions).map_err(error)
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return fail(format!(
            "non-portable path in distribution metadata: {relative}"
        ));
    }
    Ok(root.join(path))
}

fn host_platform() -> String {
    let architecture = std::env::consts::ARCH;
    format!("{}-{architecture}", std::env::consts::OS)
}

fn split_platform(platform: &str) -> Result<(String, String)> {
    platform
        .split_once('-')
        .map(|(os, arch)| (os.into(), arch.into()))
        .ok_or_else(|| ComputeError::Runtime(format!("invalid platform: {platform}")))
}

fn default_cache() -> PathBuf {
    if let Some(value) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(value).join("compute/runtimes");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".cache/compute/runtimes")
}

fn validate_digest(value: &str) -> Result<()> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        fail(format!("invalid SHA-256 digest in runtime lock: {value}"))
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(error)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(error)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(error)?;
    bytes.push(b'\n');
    fs::write(path, bytes).map_err(error)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(path).map_err(error)?).map_err(error)
}

fn push_check(
    report: &mut VerificationReport,
    name: &str,
    passed: bool,
    detail: impl Into<String>,
) {
    report.checks.push(VerificationCheck {
        name: name.into(),
        passed,
        detail: detail.into(),
    });
}

fn format_report(report: &VerificationReport) -> String {
    report
        .checks
        .iter()
        .filter(|check| !check.passed)
        .map(|check| format!("{}: {}", check.name, check.detail))
        .collect::<Vec<_>>()
        .join("; ")
}

fn status(value: bool) -> &'static str {
    if value { "PASS" } else { "FAIL" }
}
fn error(failure: impl std::fmt::Display) -> ComputeError {
    ComputeError::Runtime(failure.to_string())
}
fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(ComputeError::Runtime(message.into()))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).map_err(error)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions).map_err(error)
}
#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}
