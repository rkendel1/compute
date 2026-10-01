//! Architecture guards for the execution-fabric capabilities audited in
//! `docs/audit-2026-09-30-celesto-capability-audit.md`. Each one states an
//! invariant the audit relied on, so a later change that breaks it fails here
//! rather than in review.

use std::path::{Path, PathBuf};

use serde_json::Value;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rust_files(directory: &Path) -> Vec<PathBuf> {
    let mut found = vec![];
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(rust_files(&path));
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Every crate except the CLI (the composition root) and the GitHub worker
/// itself.
fn core_crates() -> Vec<PathBuf> {
    let mut crates = std::fs::read_dir(root().join("crates"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            name != "compute-cli" && name != "compute-worker-github"
        })
        .collect::<Vec<_>>();
    crates.sort();
    crates
}

#[test]
fn github_actions_is_an_adapter_not_a_core_special_case() {
    let needles = [
        "actions/runner",
        "actions_runner",
        "actions-runner",
        "ACTIONS_RUNNER",
        "registration-token",
        "registration_token",
        "github-actions",
        "github_actions",
    ];
    for crate_dir in core_crates() {
        for file in rust_files(&crate_dir.join("src")) {
            let text = std::fs::read_to_string(&file).unwrap();
            for needle in needles {
                assert!(
                    !text.contains(needle),
                    "{} mentions `{needle}`: provider-specific behaviour belongs in an adapter crate",
                    file.strip_prefix(root()).unwrap().display()
                );
            }
        }
    }
    // The CLI only composes the adapter, in one module.
    for file in rust_files(&root().join("crates/compute-cli/src")) {
        let text = std::fs::read_to_string(&file).unwrap();
        let mentions = text.contains("compute_worker_github") || text.contains("github-actions");
        let allowed = file.file_name().is_some_and(|name| name == "worker_cmd.rs");
        let main = file.file_name().is_some_and(|name| name == "main.rs");
        assert!(
            !mentions || allowed || main,
            "{} reaches into the GitHub adapter",
            file.display()
        );
    }
}

#[test]
fn the_worker_adds_no_second_state_system_and_no_fixed_paths() {
    let manifest =
        std::fs::read_to_string(root().join("crates/compute-worker-github/Cargo.toml")).unwrap();
    for forbidden in [
        "compute-state",
        "feltdb",
        "rusqlite",
        "sled",
        "compute-provider",
        "compute-environment",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "the worker depends on {forbidden}: it composes the execution layer, not state or the daemon"
        );
    }
    for file in rust_files(&root().join("crates/compute-worker-github/src")) {
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains("\"/tmp"),
            "{} hard-codes /tmp",
            file.display()
        );
        assert!(
            !text.contains("static mut") && !text.contains("lazy_static"),
            "{}",
            file.display()
        );
        assert!(
            !text.contains("OnceLock") && !text.contains("thread_local!"),
            "{} keeps global state",
            file.display()
        );
    }
    let script =
        std::fs::read_to_string(root().join("crates/compute-worker-github/src/runner.sh")).unwrap();
    assert!(
        !script.contains("/tmp"),
        "the worker script hard-codes /tmp"
    );
    assert!(
        !script.contains(" &\n") && !script.contains("nohup") && !script.contains("disown"),
        "the worker script must not background the runner: Compute owns its lifecycle"
    );
}

#[test]
fn a_secret_cannot_be_printed_or_serialized() {
    let secret =
        std::fs::read_to_string(root().join("crates/compute-worker-github/src/secret.rs")).unwrap();
    let code = secret.split("#[cfg(test)]").next().unwrap();
    assert!(
        !code.contains("impl fmt::Display for Secret") && !code.contains("impl Display for Secret")
    );
    assert!(
        !code.contains("derive(")
            || !code
                .lines()
                .any(|line| line.contains("derive(") && line.contains("Serialize")),
        "Secret must not derive Serialize"
    );
    assert!(!code.contains("impl Serialize") && !code.contains("impl serde::Serialize"));
    assert!(code.contains("<redacted>"));
}

/// Keys and values that would put a credential or a repository identity into
/// a recipe.
fn credential_findings(path: &str, value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let lower = key.to_lowercase();
                for word in [
                    "token",
                    "secret",
                    "password",
                    "credential",
                    "api_key",
                    "apikey",
                    "private_key",
                    "repository",
                    "repo_url",
                ] {
                    if lower.contains(word) {
                        found.push(format!("{path}.{key}"));
                    }
                }
                credential_findings(&format!("{path}.{key}"), child, found);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                credential_findings(&format!("{path}[{index}]"), child, found);
            }
        }
        Value::String(text) => {
            for marker in [
                "ghp_",
                "gho_",
                "ghs_",
                "github_pat_",
                "BEGIN PRIVATE KEY",
                "BEGIN RSA PRIVATE KEY",
                "AKIA",
            ] {
                if text.contains(marker) {
                    found.push(format!("{path} contains {marker}"));
                }
            }
        }
        _ => {}
    }
}

#[test]
fn recipes_describe_environments_and_never_hold_credentials() {
    let mut checked = 0;
    for directory in ["recipes/starters", "examples/recipes"] {
        for entry in std::fs::read_dir(root().join(directory)).unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let value: Value = serde_json::from_str(&text).unwrap();
            let mut found = vec![];
            credential_findings("", &value, &mut found);
            assert!(found.is_empty(), "{} holds {found:?}", path.display());
            // `deny_unknown_fields`: a recipe cannot carry any field a spec
            // does not define, so nothing smuggles inputs in.
            let spec: compute_core::RecipeSpec = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("{} is not a recipe: {error}", path.display()));
            assert!(
                spec.problems().is_empty(),
                "{}: {:?}",
                path.display(),
                spec.problems()
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 8,
        "the starters and the runner recipe were not all checked ({checked})"
    );
}

#[test]
fn lifecycle_is_never_inferred_from_a_terminal() {
    for file in rust_files(&root().join("crates/compute-cli/src")) {
        let text = std::fs::read_to_string(&file).unwrap();
        // (`JobStatus::is_terminal` is about a job ending, not a TTY.)
        for needle in [
            "IsTerminal",
            "isatty",
            "atty::",
            "stdin().is_terminal",
            "stdout().is_terminal",
        ] {
            assert!(
                !text.contains(needle),
                "{} consults the terminal ({needle}): interactive vs detached must be explicit flags, not a TTY guess",
                file.display()
            );
        }
    }
}

#[test]
fn receipts_record_environment_names_never_values() {
    let receipt =
        std::fs::read_to_string(root().join("crates/compute-core/src/receipt.rs")).unwrap();
    assert!(
        receipt.contains("environment_names: request.env.iter().map(|v| v.key.clone()).collect()")
    );
    assert!(!receipt.contains("v.value.clone()"));
}

#[test]
fn error_codes_are_stable_identifiers() {
    // The failure envelope's `code` is an interface: lower snake case, and
    // unique to its variant.
    let core = std::fs::read_to_string(root().join("crates/compute-core/src/lib.rs")).unwrap();
    let start = core.find("pub fn code(&self) -> &str").unwrap();
    let body = &core[start..core[start..].find("\n    }\n").unwrap() + start];
    let mut seen = std::collections::BTreeSet::new();
    for code in body.split('"').skip(1).step_by(2) {
        assert!(
            code.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "{code} is not lower snake case"
        );
        assert!(seen.insert(code.to_owned()), "{code} is used twice");
    }
    assert!(seen.len() >= 15);
}
