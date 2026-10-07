//! The dependency direction is Compute → agent executable, never Compute → agent internals.

use std::process::Command;

fn tree(package: &str) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["tree", "--offline", "-p", package, "--prefix", "none"])
        .output()
        .expect("cargo tree");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn the_agent_boundary_depends_on_no_agent_runtime() {
    for line in tree("compute-agent").lines() {
        let name = line.split_whitespace().next().unwrap_or("");
        for agent in [
            "chip",
            "fx-",
            "pax",
            "claude",
            "codex",
            "anthropic",
            "openai",
        ] {
            assert!(
                !name.starts_with(agent),
                "compute-agent must not depend on an agent runtime: {line}"
            );
        }
    }
}

#[test]
fn rust_chips_contract_crates_do_not_depend_on_compute() {
    for package in ["chip-core", "chip-remote-env"] {
        for line in tree(package).lines() {
            let name = line.split_whitespace().next().unwrap_or("");
            assert!(
                !name.starts_with("compute-"),
                "{package} must stay Compute-neutral: {line}"
            );
        }
    }
}
