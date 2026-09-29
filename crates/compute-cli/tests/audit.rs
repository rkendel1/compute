//! The product audit (`docs/audit.json`) stays true to the code: every CLI
//! command and API route it inventories exists, nothing that exists is
//! missing from it, its classifications use the audit's vocabulary, and the
//! evidence it cites is there. A command or route added without an audit
//! entry fails here; update the generators in
//! `docs/audit-evidence/2026-09-27/` and re-run them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn audit() -> Value {
    let text = std::fs::read_to_string(root().join("docs/audit.json")).expect("docs/audit.json");
    serde_json::from_str(&text).expect("audit.json is JSON")
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The subcommands `compute <path> --help` lists, without `help`.
fn subcommands(path: &[String]) -> Vec<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_compute"))
        .args(path)
        .arg("--help")
        .output()
        .expect("run compute --help");
    assert!(output.status.success(), "compute {path:?} --help failed");
    let help = String::from_utf8_lossy(&output.stdout).into_owned();
    help.lines()
        .skip_while(|line| line.trim() != "Commands:")
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .filter(|name| name != "help")
        .collect()
}

/// Whether `compute <path>` runs on its own although it also has
/// subcommands: some line of its usage does not need one.
fn runs_without_a_subcommand(path: &[String]) -> bool {
    let output = Command::new(env!("CARGO_BIN_EXE_compute"))
        .args(path)
        .arg("--help")
        .output()
        .expect("run compute --help");
    let help = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut usage = help
        .lines()
        .skip_while(|line| !line.starts_with("Usage:"))
        .take_while(|line| !line.trim().is_empty());
    usage.any(|line| !line.contains("<COMMAND>"))
}

fn leaves(path: Vec<String>, out: &mut BTreeSet<String>) {
    let children = subcommands(&path);
    if !children.is_empty() && !path.is_empty() && runs_without_a_subcommand(&path) {
        out.insert(format!("compute {}", path.join(" ")));
    }
    if children.is_empty() {
        out.insert(format!("compute {}", path.join(" ")));
        return;
    }
    for child in children {
        let mut next = path.clone();
        next.push(child);
        leaves(next, out);
    }
}

#[test]
fn the_audit_lists_every_cli_command_and_no_other() {
    let mut actual = BTreeSet::new();
    leaves(Vec::new(), &mut actual);
    let audited: BTreeSet<String> = audit()["cli"]
        .as_array()
        .expect("cli")
        .iter()
        .map(|command| command["command"].as_str().expect("command").to_owned())
        .collect();
    let unaudited: Vec<_> = actual.difference(&audited).collect();
    let gone: Vec<_> = audited.difference(&actual).collect();
    assert!(
        unaudited.is_empty() && gone.is_empty(),
        "commands missing from docs/audit.json: {unaudited:?}; audited commands that no longer exist: {gone:?}"
    );
}

#[test]
fn the_audit_lists_every_api_route_with_its_scope() {
    let actual: BTreeMap<(String, String), String> = compute_environment::api::ROUTES
        .iter()
        .map(|(method, path)| {
            let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
            let scope = compute_environment::auth::required_scope(method, &segments);
            ((method.to_string(), path.to_string()), format!("{scope:?}"))
        })
        .collect();
    let audited: BTreeMap<(String, String), String> = audit()["api"]
        .as_array()
        .expect("api")
        .iter()
        .map(|route| {
            (
                (
                    route["method"].as_str().expect("method").to_owned(),
                    route["path"].as_str().expect("path").to_owned(),
                ),
                route["scope"].as_str().expect("scope").to_owned(),
            )
        })
        .collect();
    let unaudited: Vec<_> = actual
        .keys()
        .filter(|route| !audited.contains_key(*route))
        .collect();
    let gone: Vec<_> = audited
        .keys()
        .filter(|route| !actual.contains_key(*route))
        .collect();
    assert!(
        unaudited.is_empty() && gone.is_empty(),
        "routes missing from docs/audit.json: {unaudited:?}; audited routes that no longer exist: {gone:?}"
    );
    for (route, scope) in &actual {
        assert_eq!(&audited[route], scope, "the audited scope of {route:?}");
    }
}

#[test]
fn every_classification_uses_the_audit_vocabulary() {
    let audit = audit();
    let statuses = strings(&audit["audit"]["status_vocabulary"]);
    let journeys = strings(&audit["audit"]["journey_vocabulary"]);
    let mut ids = BTreeSet::new();
    for capability in audit["capabilities"].as_array().expect("capabilities") {
        let id = capability["id"].as_str().expect("id");
        assert!(ids.insert(id), "capability {id} is listed twice");
        let status = capability["status"].as_str().expect("status");
        assert!(statuses.iter().any(|s| s == status), "{id}: {status}");
    }
    for journey in audit["journeys"].as_array().expect("journeys") {
        let status = journey["status"].as_str().expect("status");
        assert!(
            journeys.iter().any(|s| s == status),
            "{}: {status}",
            journey["id"]
        );
    }
    for doc in audit["documentation"].as_array().expect("documentation") {
        let status = doc["status"].as_str().expect("status");
        assert!(
            [
                "DOCUMENTED CORRECTLY",
                "OUTDATED",
                "INCOMPLETE",
                "MISLEADING",
                "MISSING"
            ]
            .iter()
            .any(|s| status.starts_with(s)),
            "{}: {status}",
            doc["doc"]
        );
    }
    for area in audit["readiness"].as_array().expect("readiness") {
        let status = area["status"].as_str().expect("status");
        assert!(
            ["PASS", "PARTIAL", "FAIL"].contains(&status),
            "{}: {status}",
            area["area"]
        );
    }
}

#[test]
fn the_evidence_the_audit_cites_exists() {
    let audit = audit();
    let root = root();
    let evidence_dir = root.join("docs/audit-evidence/2026-09-27");
    let experiments = &audit["experiments"];
    let mut missing = Vec::new();
    let mut check = |cited: &str| {
        let cited = cited.trim_matches('`');
        let (path, anchor) = cited.split_once('#').unwrap_or((cited, ""));
        if path == "experiments.json" {
            if !anchor.is_empty() && experiments.get(anchor).is_none() {
                missing.push(cited.to_owned());
            }
        } else if (path.starts_with("crates/")
            || path.starts_with("packages/")
            || path.starts_with("docs/")
            || path.starts_with(".github/"))
            && !root.join(path).exists()
            && !evidence_dir.join(path).exists()
        {
            missing.push(cited.to_owned());
        } else if path.ends_with(".rs") && !anchor.is_empty() {
            // `file.rs#Name` cites a definition: it must still be there.
            let text = std::fs::read_to_string(root.join(path)).unwrap_or_default();
            for name in anchor.split(',') {
                if !text.contains(name) {
                    missing.push(format!("{path}#{name}"));
                }
            }
        }
    };
    for capability in audit["capabilities"].as_array().expect("capabilities") {
        for key in ["source", "tests"] {
            strings(&capability["evidence"][key])
                .iter()
                .for_each(|p| check(p));
        }
    }
    for journey in audit["journeys"].as_array().expect("journeys") {
        strings(&journey["evidence"]).iter().for_each(|p| check(p));
    }
    for command in audit["cli"].as_array().expect("cli") {
        strings(&command["tests"]).iter().for_each(|p| check(p));
    }
    for file in audit["tests"]["files"].as_array().expect("test files") {
        check(file["path"].as_str().expect("path"));
    }
    for model in audit["models"].as_array().expect("models") {
        for source in model["source"].as_str().expect("source").split("; ") {
            check(source);
        }
    }
    for doc in audit["documentation"].as_array().expect("documentation") {
        let doc = doc["doc"].as_str().expect("doc");
        if !root.join(doc).exists() {
            missing.push(doc.to_owned());
        }
    }
    assert!(
        missing.is_empty(),
        "cited evidence that does not exist: {missing:?}"
    );
}

#[test]
fn gaps_cited_by_the_readiness_matrix_and_the_path_exist() {
    let audit = audit();
    let gaps: BTreeSet<String> = audit["gaps"]
        .as_array()
        .expect("gaps")
        .iter()
        .map(|gap| gap["id"].as_str().expect("id").to_owned())
        .collect();
    let mut cited = String::new();
    for area in audit["readiness"].as_array().expect("readiness") {
        cited.push_str(area["blocking_gap"].as_str().unwrap_or_default());
        cited.push(' ');
    }
    for stage in audit["backlog"].as_array().expect("backlog") {
        cited.push_str(&strings(&stage["items"]).join(" "));
    }
    for word in cited.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
        if word.starts_with("G-") {
            assert!(gaps.contains(word), "{word} is cited but not a gap");
        }
    }
}
