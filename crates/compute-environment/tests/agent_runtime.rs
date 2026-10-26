//! The agent the Work page runs for you: Compute starts the configured
//! install's service itself, supplying everything it needs. The executable
//! here is a stand-in that records what it was started with and answers
//! `GET /health`. This is the only test in its binary, so it alone sets the
//! process environment.

use std::os::unix::fs::PermissionsExt;

use compute_environment::agent_runtime::{AgentRuntime, AgentSetup, ModelSettings};
use serde_json::Value;

const STAND_IN: &str = r#"#!/usr/bin/env python3
import http.server, json, os, sys
port = int(sys.argv[sys.argv.index("--port") + 1])
record = {"args": sys.argv[1:], "env": dict(os.environ)}
open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "started.json"), "w").write(json.dumps(record))
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.send_header("content-length", "2"); self.end_headers(); self.wfile.write(b"ok")
    def log_message(self, *args): pass
http.server.HTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#;

async fn alive(url: &str) -> bool {
    compute_environment::agent_service::status_of(url).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_is_started_with_everything_derived_and_nothing_leaks() {
    // SAFETY: the only test in this binary.
    for name in [
        "CHIP_PROVIDER",
        "CHIP_MODEL",
        "CHIP_ENDPOINT",
        "CHIP_API_KEY",
    ] {
        unsafe { std::env::remove_var(name) };
    }
    unsafe { std::env::set_var("UNRELATED_SECRET", "must-not-reach-the-agent") };

    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let executable = home.path().join("agent");
    std::fs::write(&executable, STAND_IN).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let token = state.path().join("target.token");
    std::fs::write(&token, "token").unwrap();
    let runtime = AgentRuntime::new(Some(AgentSetup {
        home: home.path().to_path_buf(),
        executable: "agent".into(),
        state_dir: state.path().to_path_buf(),
        target: Some(("http://127.0.0.1:8788".into(), token.clone())),
    }));

    // No model yet: it says what is missing and starts nothing.
    let status = runtime.status(Some("https://example.com/app.git")).await;
    assert_eq!(status["native"], true);
    assert_eq!(status["model"]["configured"], false);
    assert_eq!(status["running"], false);
    let missing = runtime
        .ensure("https://example.com/app.git")
        .await
        .unwrap_err();
    assert!(missing.message().contains("no model"), "{missing:?}");
    assert!(!home.path().join("started.json").exists());

    // An incomplete model is refused.
    assert!(
        runtime
            .set_model(ModelSettings {
                provider: "openai-compatible".into(),
                ..Default::default()
            })
            .await
            .is_err()
    );

    // Saved: private to the user, and never shown again.
    runtime
        .set_model(ModelSettings {
            provider: "openai-compatible".into(),
            model: "qwen".into(),
            endpoint: "http://127.0.0.1:1337/v1/chat/completions".into(),
            api_key: "sk-secret".into(),
        })
        .await
        .unwrap();
    let file = state.path().join("agent").join("model.json");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let status = runtime.status(Some("https://example.com/app.git")).await;
    assert_eq!(status["model"]["configured"], true);
    assert_eq!(status["model"]["source"], "saved");
    assert_eq!(status["model"]["api_key_set"], true);
    assert!(!status.to_string().contains("sk-secret"), "{status}");

    // The first goal starts it, with the target, project, worker and model.
    let url = runtime.ensure("https://example.com/app.git").await.unwrap();
    assert!(alive(&url).await);
    let started: Value =
        serde_json::from_str(&std::fs::read_to_string(home.path().join("started.json")).unwrap())
            .unwrap();
    let env = &started["env"];
    assert_eq!(started["args"][0], "serve");
    assert_eq!(env["COMPUTE_STACK_AGENT_TARGET"], "http://127.0.0.1:8788");
    assert_eq!(
        env["COMPUTE_STACK_AGENT_TARGET_TOKEN_FILE"],
        token.to_str().unwrap()
    );
    assert_eq!(
        env["COMPUTE_STACK_AGENT_PROJECT"],
        "https://example.com/app.git"
    );
    assert_eq!(
        env["COMPUTE_STACK_AGENT_WORKER"],
        executable.to_str().unwrap()
    );
    assert_eq!(env["CHIP_MODEL"], "qwen");
    assert_eq!(env["CHIP_API_KEY"], "sk-secret");
    assert!(
        env.get("UNRELATED_SECRET").is_none(),
        "the agent starts from an empty environment"
    );
    assert!(runtime.status(Some("https://example.com/app.git")).await["running"] == true);

    // Asking again reuses it; another project gets its own.
    assert_eq!(
        runtime.ensure("https://example.com/app.git").await.unwrap(),
        url
    );
    let other = runtime
        .ensure("https://example.com/other.git")
        .await
        .unwrap();
    assert_ne!(other, url);
    assert!(alive(&other).await);

    // Changing the model stops them, and the next goal starts a fresh one.
    runtime
        .set_model(ModelSettings {
            provider: "openai-compatible".into(),
            model: "bigger".into(),
            endpoint: "http://127.0.0.1:1337/v1/chat/completions".into(),
            api_key: String::new(),
        })
        .await
        .unwrap();
    assert!(!alive(&url).await && !alive(&other).await);
    let again = runtime.ensure("https://example.com/app.git").await.unwrap();
    assert!(alive(&again).await);

    runtime.stop_all().await;
    assert!(!alive(&again).await);

    // An agent that dies as it starts says why, in its own words.
    std::fs::write(
        &executable,
        "#!/bin/sh\necho 'cannot read the target token file' >&2\nexit 3\n",
    )
    .unwrap();
    let failed = runtime
        .ensure("https://example.com/app.git")
        .await
        .unwrap_err();
    assert!(
        failed
            .message()
            .contains("cannot read the target token file"),
        "{failed:?}"
    );

    // Base Compute ships no agent: nothing to start.
    let none = AgentRuntime::new(None);
    assert_eq!(none.status(None).await["native"], false);
}
