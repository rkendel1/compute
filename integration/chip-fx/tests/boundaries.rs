//! Architecture checks for the integration: the dependency direction and the capability boundary.
//! They read the real resolved graph (`cargo tree`) and the harness source, so they fail when the
//! structure changes, not when a comment does.

use std::process::Command;

fn tree(args: &[&str]) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["tree", "--offline", "--prefix", "none", "--edges", "normal"])
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn names(tree: &str) -> Vec<String> {
    tree.lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(String::from)
        .collect()
}

/// FX must not depend on Chip, and neither may depend on Compute.
#[test]
fn fx_depends_on_neither_chip_nor_compute() {
    for package in ["fx-core", "fx-single-attempt"] {
        for name in names(&tree(&["-p", package])) {
            assert!(
                !name.starts_with("chip") && !name.starts_with("compute"),
                "{package} must not depend on {name}"
            );
        }
    }
}

#[test]
fn chip_depends_on_no_compute_crate() {
    for package in ["chip-core", "chip-fx", "chip-inference"] {
        for name in names(&tree(&["-p", package])) {
            assert!(
                !name.starts_with("compute"),
                "{package} must stay Compute-neutral: {name}"
            );
        }
    }
}

/// The crates that must also build for WebAssembly carry no native provider stack.
#[test]
fn wasm_bound_crates_carry_no_native_provider_dependencies() {
    for package in ["chip-core", "fx-core", "fx-single-attempt"] {
        for name in names(&tree(&["-p", package])) {
            for native in [
                "reqwest",
                "hyper",
                "rustls",
                "tokio",
                "fx-native-provider",
                "chip-fx-native",
                "chip-legacy-http",
                "native-tls",
                "openssl",
            ] {
                assert!(
                    name != native,
                    "{package} (Wasm-bound) must not depend on {native}"
                );
            }
        }
    }
}

/// The harness gets effects only through the capability boundary: its source never starts a process,
/// touches the filesystem or opens a socket. (The one fixture "effect" is a counter.)
#[test]
fn harness_source_has_no_direct_host_access() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")).unwrap();
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "std::process",
        "tokio::process",
        "std::fs",
        "tokio::fs",
        "std::net",
        "tokio::net",
        "libc::",
        "Command::new",
    ] {
        assert!(
            !code.contains(forbidden),
            "the harness must reach effects only through the capability boundary: found {forbidden}"
        );
    }
}
