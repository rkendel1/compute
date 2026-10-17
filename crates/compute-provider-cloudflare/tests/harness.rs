//! The Worker harness (worker/harness.mjs) run under Node against real
//! WASI modules. Node stands in for the Workers runtime: both are V8 with
//! the same WebAssembly, Request/Response, and Web Crypto APIs.
//!
//! Requires `node` on PATH. Without it the test says it was skipped and
//! passes nothing: set COMPUTE_REQUIRE_NODE=1 (as CI does) to make a
//! missing Node a failure.

mod support;

use std::path::Path;
use std::process::Command;

use base64::Engine as _;
use serde_json::{Value, json};
use support::modules;

const PROTOCOL: &str = "compute.cloudflare.invoke@1";

fn node() -> Option<String> {
    let found = Command::new("node")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|_| "node".to_owned());
    if found.is_none() {
        if std::env::var_os("COMPUTE_REQUIRE_NODE").is_some() {
            panic!("COMPUTE_REQUIRE_NODE is set but node is not on PATH");
        }
        eprintln!("SKIPPED: the harness test needs node on PATH; nothing was tested");
    }
    found
}

fn decode(value: &Value) -> String {
    String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(value.as_str().unwrap())
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn the_harness_runs_wasi_commands_and_refuses_strangers() {
    let Some(node) = node() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, bytes: Vec<u8>| {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_string_lossy().into_owned()
    };
    let hello = write("hello.wasm", modules::hello());
    let exit3 = write("exit3.wasm", modules::exit3());
    let trap = write("trap.wasm", modules::trap());
    let echo = write("echo.wasm", modules::echo_stdin());
    let show = write("show.wasm", modules::show());
    let library = write("library.wasm", modules::library());
    let body = |extra: Value| {
        let mut body =
            json!({ "protocol": PROTOCOL, "execution_id": "exec_1", "args": ["workload.wasm"] });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    };
    let ok = |module: &str, extra: Value| json!({ "module": module, "token": "secret", "header": "Bearer secret", "body": body(extra) });
    let cases = json!([
        ok(&hello, json!({})),
        ok(&exit3, json!({})),
        ok(&trap, json!({})),
        ok(&echo, json!({ "stdin": base64::engine::general_purpose::STANDARD.encode("piped in") })),
        ok(&show, json!({ "args": ["workload.wasm", "a b"], "env": [["K", "v=1"]] })),
        ok(&library, json!({})),
        ok(&hello, json!({ "max_output_bytes": 5 })),
        { "module": hello, "token": "secret", "header": "Bearer wrong", "body": body(json!({})) },
        { "module": hello, "token": "secret", "body": body(json!({})) },
        { "module": hello, "token": "", "header": "Bearer ", "body": body(json!({})) },
        { "module": hello, "token": "secret", "header": "Bearer secret", "method": "GET" },
        { "module": hello, "token": "secret", "header": "Bearer secret", "body": { "protocol": "other@1" } },
        { "module": hello, "token": "secret", "header": "Bearer secret", "raw": "not json" },
    ]);
    let cases_path = dir.path().join("cases.json");
    std::fs::write(&cases_path, cases.to_string()).unwrap();
    let driver = Path::new(env!("CARGO_MANIFEST_DIR")).join("worker/harness.test.mjs");
    let output = Command::new(node)
        .arg(driver)
        .arg(&cases_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let results = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 13);

    let hello = &results[0]["body"];
    assert_eq!(results[0]["status"], 200);
    assert_eq!(hello["protocol"], PROTOCOL);
    assert_eq!(hello["harness"], "compute-harness/1");
    assert_eq!(hello["workload"], "sha256:test");
    assert_eq!(hello["execution_id"], "exec_1");
    assert_eq!(hello["status"], "exited");
    assert_eq!(hello["exit_code"], 0);
    assert_eq!(decode(&hello["stdout"]), "hello from cloudflare\n");

    let exit3 = &results[1]["body"];
    assert_eq!(exit3["status"], "exited");
    assert_eq!(exit3["exit_code"], 3);
    assert_eq!(decode(&exit3["stderr"]), "failing\n");

    let trap = &results[2]["body"];
    assert_eq!(trap["status"], "trapped");
    assert!(trap["exit_code"].is_null());
    assert!(trap["trap"].as_str().unwrap().contains("unreachable"));

    assert_eq!(decode(&results[3]["body"]["stdout"]), "piped in");
    assert_eq!(
        decode(&results[4]["body"]["stdout"]),
        "workload.wasm\0a b\0K=v=1\0"
    );

    let library = &results[5]["body"];
    assert_eq!(library["status"], "invalid_module");
    assert!(library["message"].as_str().unwrap().contains("_start"));

    let truncated = &results[6]["body"];
    assert_eq!(decode(&truncated["stdout"]), "hello");
    assert_eq!(truncated["stdout_bytes"], 22);

    for (index, status, error) in [
        (7, 401, "unauthorized"),
        (8, 401, "unauthorized"),
        (9, 401, "unauthorized"),
        (10, 405, "method_not_allowed"),
        (11, 400, "protocol_unsupported"),
        (12, 400, "bad_request"),
    ] {
        assert_eq!(results[index]["status"], status, "case {index}");
        assert_eq!(results[index]["body"]["error"], error, "case {index}");
    }
}
