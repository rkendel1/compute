//! The local provider reports what this machine can actually do, and a
//! host condition it does not understand never takes the whole provider
//! down.
//!
//! GitHub's Ubuntu runners have .NET installed with several runtimes. The
//! local provider reported dotnet's detected version as the raw output of
//! `dotnet --list-runtimes` — every runtime, framework, and install path —
//! which exceeds the capability validator's bound on a version, so the
//! whole local provider was `provider_capabilities_invalid` and nothing
//! could be placed on it. These tests use a deterministic stand-in for that
//! runner condition, so they do not depend on the host having .NET.

#[path = "support/runtimes.rs"]
mod runtimes;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A `dotnet` that answers `--list-runtimes` the way a runner image with
/// several .NET versions does.
fn runner_dotnet(directory: &Path, runtimes: &[&str]) -> PathBuf {
    let bin = directory.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mut listing = String::new();
    for version in runtimes {
        for framework in [
            "Microsoft.AspNetCore.App",
            "Microsoft.NETCore.App",
            "Microsoft.WindowsDesktop.App",
        ] {
            listing.push_str(&format!(
                "{framework} {version} [/usr/share/dotnet/shared/{framework}]\n"
            ));
        }
    }
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--list-runtimes\" ]; then\ncat <<'LIST'\n{listing}LIST\nexit 0\nfi\nexit 1\n"
    );
    let dotnet = bin.join("dotnet");
    std::fs::write(&dotnet, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dotnet, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

fn compute(bin: &Path, work: &Path, arguments: &[&str]) -> Output {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_compute"));
    runtimes::with_fixture_runtimes(&mut command);
    command
        .current_dir(work)
        .env("PATH", path)
        .env("COMPUTE_HOME", work.join("home"))
        .env("COMPUTE_CAPABILITY_CACHE", work.join("capabilities.json"))
        .env_remove("COMPUTE_POOL_CONFIG")
        .args(arguments)
        .output()
        .unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

const RUNNER: &[&str] = &[
    "6.0.36", "7.0.20", "8.0.11", "8.0.12", "9.0.0", "9.0.1", "10.0.0",
];

#[test]
fn a_host_with_many_dotnet_runtimes_is_still_a_valid_local_provider() {
    let root = tempfile::tempdir().unwrap();
    let bin = runner_dotnet(root.path(), RUNNER);
    std::fs::write(root.path().join("main.py"), "print('placed')\n").unwrap();

    // The run the CI runner refused: placement on the local provider.
    let ran = compute(
        &bin,
        root.path(),
        &["run", "main.py", "--network", "network", "--json"],
    );
    assert!(ran.status.success(), "{}", text(&ran));
    let result: serde_json::Value = serde_json::from_slice(&ran.stdout).unwrap();
    assert_eq!(result["stdout"]["text"], "placed\n", "{result:#}");

    // What it reports about dotnet is the runtimes it can run, as versions:
    // no install paths, no other frameworks, within a version's bounds.
    let dotnet = dotnet(&bin, root.path());
    assert_eq!(dotnet["compatible"], true, "{dotnet:#}");
    let version = dotnet["detected_version"]
        .as_str()
        .unwrap_or_else(|| panic!("no detected version in {dotnet:#}"));
    assert!(!version.contains('/'), "{version}");
    assert!(!version.contains("AspNetCore"), "{version}");
    assert!(version.len() <= 1024, "{} bytes", version.len());
    for runtime in RUNNER {
        assert!(
            version.contains(&format!("Microsoft.NETCore.App {runtime}")),
            "{version}"
        );
    }
}

/// The local provider's raw capability entry for dotnet.
fn dotnet(bin: &Path, work: &Path) -> serde_json::Value {
    let capabilities = compute(bin, work, &["provider", "capabilities", "local", "--json"]);
    assert!(capabilities.status.success(), "{}", text(&capabilities));
    let capabilities: serde_json::Value = serde_json::from_slice(&capabilities.stdout).unwrap();
    capabilities["inventory"]["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|runtime| runtime["id"] == "dotnet")
        .cloned()
        .unwrap_or_else(|| panic!("no dotnet in {capabilities:#}"))
}

/// A `dotnet` that runs no .NET runtime is not advertised as one.
#[test]
fn a_dotnet_without_a_runtime_is_not_advertised() {
    let root = tempfile::tempdir().unwrap();
    let bin = runner_dotnet(root.path(), &[]);
    let dotnet = dotnet(&bin, root.path());
    assert_eq!(dotnet["compatible"], false, "{dotnet:#}");
    assert!(dotnet["detected_version"].is_null(), "{dotnet:#}");
}
