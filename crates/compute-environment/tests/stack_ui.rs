//! The vendored shared design system (`ui/vendor/stack-ui`).
//!
//! It is served inside the existing UI assets, not beside them: the UI still
//! loads only `/ui/`, `/ui/app.js` and `/ui/app.css`, and the API's route table
//! is unchanged.

mod common;

use std::path::Path;
use std::sync::Arc;

use compute_environment::*;
use sha2::{Digest, Sha256};

fn vendor() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/vendor/stack-ui")
}

#[test]
fn vendored_files_match_their_checksums() {
    let sums = std::fs::read_to_string(vendor().join("SHA256SUMS")).expect("SHA256SUMS");
    let mut checked = Vec::new();
    for line in sums.lines().filter(|line| !line.trim().is_empty()) {
        let (digest, file) = line.split_once("  ").expect("`<sha256>  <file>`");
        let bytes =
            std::fs::read(vendor().join(file)).unwrap_or_else(|_| panic!("{file} is vendored"));
        let actual = format!("{:x}", Sha256::digest(&bytes));
        assert_eq!(
            actual, digest,
            "{file} differs from the stack-ui release it claims to be"
        );
        checked.push(file.to_string());
    }
    checked.sort();
    assert_eq!(checked, ["core.css", "stack-ui-core.js", "tokens.css"]);
    let version = std::fs::read_to_string(vendor().join("VERSION")).expect("VERSION");
    assert!(!version.trim().is_empty());
}

struct Served {
    #[allow(dead_code)]
    daemon: Arc<Daemon>,
    endpoint: String,
    _dir: tempfile::TempDir,
}

async fn with_ui() -> Served {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(dir.path().join("node"), store, artifacts);
    config.provider = Arc::new(common::provider());
    config.ui = true;
    let daemon = Daemon::start(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(api::serve(listener, daemon.clone(), None));
    Served {
        daemon,
        endpoint,
        _dir: dir,
    }
}

async fn get(endpoint: &str, path: &str) -> String {
    let response = reqwest::get(format!("{endpoint}{path}")).await.unwrap();
    assert_eq!(response.status(), 200, "GET {path}");
    response.text().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_served_script_is_strict_and_carries_the_design_system_first() {
    let node = with_ui().await;
    let script = get(&node.endpoint, "/ui/app.js").await;
    // A directive prologue must be the first statement; `app.js`'s own
    // `'use strict'` is no longer first once the design system precedes it.
    assert!(
        script.starts_with("'use strict';\n"),
        "{}",
        &script[..60.min(script.len())]
    );
    let shared = script
        .find("global.StackUI = Object.freeze")
        .expect("the design system");
    let app = script
        .find("// Compute control plane UI.")
        .expect("the control plane");
    assert!(
        shared < app,
        "StackUI is defined before the control plane uses it"
    );
    let vendored = std::fs::read_to_string(vendor().join("stack-ui-core.js")).unwrap();
    assert!(
        script.contains(&vendored),
        "the vendored file is served byte for byte"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_served_styles_put_shared_tokens_before_the_control_plane() {
    let node = with_ui().await;
    let style = get(&node.endpoint, "/ui/app.css").await;
    let tokens = style
        .find("--stack-color-bg-canvas")
        .expect("shared tokens");
    let component = style.find(".stack-status").expect("shared components");
    let own = style
        .find("--bg: #f6f7f9")
        .expect("the control plane's own styles");
    assert!(tokens < component && component < own);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_design_system_adds_no_served_path() {
    let node = with_ui().await;
    for path in [
        "/ui/vendor/stack-ui/stack-ui-core.js",
        "/ui/stack-ui-core.js",
        "/ui/tokens.css",
    ] {
        let status = reqwest::get(format!("{}{path}", node.endpoint))
            .await
            .unwrap()
            .status();
        assert_ne!(status, 200, "{path} must not be served on its own");
    }
}
