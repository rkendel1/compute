//! Deterministic runtime catalogs for tests: no network, no CDN.
//!
//! Each fixture artifact is a tiny script that reports the version Compute
//! pins and hands execution to an interpreter already on the host. It is
//! acquired from a `file://` URL, digest-verified, unpacked, installed, and
//! probed exactly as a downloaded artifact is. It proves the runtime
//! lifecycle, not the upstream runtime, so it is never used outside tests.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use compute_core::PlatformIdentity;
use sha2::{Digest, Sha256};

use crate::{ProviderError, ProviderErrorKind, RuntimeCatalog};

/// A fixture catalog written to disk, for processes that read
/// `$COMPUTE_RUNTIME_CATALOG`.
pub struct FixtureCatalog {
    pub catalog: RuntimeCatalog,
    pub path: PathBuf,
}

/// Write fixture artifacts for `shell` (the host `/bin/sh`), and for
/// `python` and `node` when the host has them, into `directory`, and a
/// catalog naming them. Versions and executables are the pinned ones.
pub fn host_fixture_catalog(directory: &Path) -> Result<FixtureCatalog, ProviderError> {
    let io = |error: std::io::Error| {
        ProviderError::new(ProviderErrorKind::RuntimeUnavailable, error.to_string())
    };
    fs::create_dir_all(directory).map_err(io)?;
    let pinned: serde_json::Value =
        serde_json::from_slice(include_bytes!("../../../distribution/runtime-lock.json"))
            .expect("valid embedded runtime lock");
    let platform = PlatformIdentity {
        runtime_abi: None,
        ..PlatformIdentity::current()
    }
    .label();
    let mut runtimes = BTreeMap::new();
    for runtime in ["wasm", "native"] {
        runtimes.insert(runtime.to_owned(), pinned["runtimes"][runtime].clone());
    }
    let hosts = [
        (
            "shell",
            Some(PathBuf::from("/bin/sh")),
            "BusyBox v{version}",
        ),
        ("python", which::which("python3").ok(), "Python {version}"),
        ("node", which::which("node").ok(), "v{version}"),
    ];
    for (runtime, host, banner) in hosts {
        let Some(host) = host else { continue };
        let locked = &pinned["runtimes"][runtime];
        let version = locked["version"].as_str().expect("pinned version");
        let executable = locked["executable"].as_str().expect("pinned executable");
        let banner = banner.replace("{version}", version);
        let script = format!(
            "#!/bin/sh\n# Compute test fixture for {runtime} {version}: runs {host}.\ncase \"$1\" in --version|--help) echo \"{banner}\"; exit 0;; esac\nexec {host} \"$@\"\n",
            host = host.display()
        );
        let artifact = directory.join(format!("{runtime}-fixture"));
        fs::write(&artifact, &script).map_err(io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&artifact, fs::Permissions::from_mode(0o755)).map_err(io)?;
        }
        runtimes.insert(
            runtime.to_owned(),
            serde_json::json!({
                "version": version,
                "executable": executable,
                "artifacts": {
                    (platform.clone()): {
                        "url": format!("file://{}", artifact.display()),
                        "sha256": format!("{:x}", Sha256::digest(script.as_bytes())),
                        "format": "file",
                        "install": [{ "source": "artifact", "destination": executable }],
                    }
                }
            }),
        );
    }
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "schema_version": 2,
        "runtimes": runtimes,
    }))
    .expect("catalog serializes");
    let path = directory.join("runtime-catalog.json");
    fs::write(&path, &bytes).map_err(io)?;
    Ok(FixtureCatalog {
        catalog: RuntimeCatalog::from_bytes(bytes)?,
        path,
    })
}
