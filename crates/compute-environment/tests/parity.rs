//! UI/API parity: the UI performs no operation the Compute API does not
//! expose, and every route in the API's table is really served.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use compute_environment::api::ROUTES;
use compute_environment::*;

const UI: &str = include_str!("../ui/app.js");

/// `/environments/{environment}/stop` → `/environments/{}/stop`.
fn normalize(path: &str) -> String {
    let path = path.split('?').next().unwrap_or_default();
    let mut normalized = String::new();
    let mut chars = path.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '{' => {
                for next in chars.by_ref() {
                    if next == '}' {
                        break;
                    }
                }
                normalized.push_str("{}");
            }
            '$' if chars.peek() == Some(&'{') => {
                chars.next();
                let mut depth = 1;
                for next in chars.by_ref() {
                    match next {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                normalized.push_str("{}");
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// Every `api('METHOD', 'path')` or `` api('METHOD', `path`) `` in the UI,
/// and every event stream it opens.
fn ui_operations() -> BTreeSet<(String, String)> {
    let mut operations = BTreeSet::new();
    for (index, _) in UI.match_indices("api('") {
        let rest = &UI[index + 5..];
        let method = &rest[..rest.find('\'').unwrap()];
        let rest = &rest[method.len() + 1..];
        let rest = rest.trim_start_matches(',').trim_start();
        let quote = rest.chars().next().unwrap();
        assert!(
            quote == '\'' || quote == '`',
            "the UI builds a path the parity test cannot read: {}",
            &rest[..rest.len().min(60)]
        );
        let path = &rest[1..rest[1..].find(quote).unwrap() + 1];
        operations.insert((method.to_string(), normalize(path)));
    }
    // The event stream is read with fetch, so it carries the credential.
    for (index, _) in UI.match_indices("fetch(`") {
        let rest = &UI[index + "fetch(`".len()..];
        operations.insert(("GET".into(), normalize(&rest[..rest.find('`').unwrap()])));
    }
    operations
}

#[test]
fn every_ui_operation_is_an_api_operation() {
    let api = ROUTES
        .iter()
        .map(|route| (route.method.to_string(), normalize(route.path)))
        .collect::<BTreeSet<_>>();
    let ui = ui_operations();
    assert!(ui.len() > 20, "the UI's operations were found: {ui:?}");
    let missing = ui.difference(&api).collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "the UI uses operations the API does not expose: {missing:?}"
    );
    // The UI covers the lifecycle the CLI does.
    for operation in [
        ("POST", "/deployments"),
        ("POST", "/deployments/promote"),
        ("POST", "/environments/{}/projects/{}/stop"),
        ("POST", "/environments/{}/start"),
        ("DELETE", "/environments/{}/projects/{}"),
        ("GET", "/events/stream"),
        ("GET", "/deployments/{}"),
        ("POST", "/deployments/{}/rollback"),
        ("GET", "/deployments/{}/receipt"),
        ("GET", "/domains"),
        ("POST", "/domains"),
        ("GET", "/domains/{}"),
        ("DELETE", "/domains/{}"),
        ("POST", "/dns/reconcile"),
        ("POST", "/certificates/{}/renew"),
    ] {
        assert!(
            ui.contains(&(operation.0.to_string(), operation.1.to_string())),
            "the UI does not offer {operation:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_api_route_is_served() {
    let node = tempfile::tempdir().unwrap();
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let daemon = Daemon::start({
        let mut config = DaemonConfig::new(node.path(), store, artifacts);
        config.provider = std::sync::Arc::new(common::provider());
        config
    })
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(api::serve(listener, daemon.clone(), None));
    let client = client::DaemonClient::new(&endpoint).unwrap();
    for route in ROUTES {
        let (method, path) = (&route.method, &route.path);
        if *path == "/shutdown" || *path == "/events/stream" {
            continue;
        }
        let concrete = path
            .replace("{environment}", "nowhere")
            .replace("{project}", "nothing")
            .replace("{workload}", "none")
            .replace("{deployment}", "dep_none")
            .replace("{execution}", "exec_none")
            .replace("{receipt}", "sha256:none")
            .replace("{service}", "none")
            .replace("{domain}", "none.example.com")
            .replace("{credential}", "cred_none")
            .replace("{session}", "ses_none")
            .replace("{job}", "job_none");
        let result = match *method {
            "GET" => client.get::<serde_json::Value>(&concrete).await.map(|_| ()),
            "POST" => client
                .post::<_, serde_json::Value>(&concrete, Some(&serde_json::json!({})))
                .await
                .map(|_| ()),
            "DELETE" => client
                .delete::<serde_json::Value>(&concrete)
                .await
                .map(|_| ()),
            other => panic!("unexpected method {other}"),
        };
        if let Err(error) = result {
            assert_ne!(
                error.kind(),
                "no_route",
                "{method} {path} is in the route table but not served"
            );
        }
    }
    // And the other direction: nothing undeclared is served, even to a
    // caller with every scope.
    for path in ["/undeclared", "/environments/nowhere/undeclared"] {
        let error = client
            .get::<serde_json::Value>(path)
            .await
            .expect_err("undeclared");
        assert_eq!(error.kind(), "no_route", "{path}");
    }
    daemon.shutdown().await;
}
