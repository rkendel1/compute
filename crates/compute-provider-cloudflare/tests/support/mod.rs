//! A deterministic, local stand-in for Cloudflare: the v4 API endpoints
//! the provider uses, and workers.dev invocations. Invocations really run
//! the uploaded WASI module (with wasmtime) and answer in the harness's
//! protocol, so tests exercise the provider end to end with no account,
//! credential, or network.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use compute_provider_cloudflare::{
    CloudflareProvider, CloudflareSettings, Credential, CredentialSource,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
pub const TOKEN: &str = "cf-test-token-7f3a9c";
pub const SUBDOMAIN: &str = "compute-test";

/// What the fake does instead of answering normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    /// Invocations answer with error 1027 (daily request quota).
    DailyQuota,
    /// Invocations answer with error 1102 (CPU or memory limit).
    CpuLimit,
    /// Invocations never answer.
    Hang,
    /// The API rate-limits every call (HTTP 429).
    ApiRateLimited,
    /// Uploads fail: the account already has its maximum of Workers.
    ScriptLimit,
    /// The API answers 500.
    ApiDown,
    /// Invocations answer for a different workload than was deployed.
    WrongWorkload,
    /// The first upload is accepted but its response is lost (HTTP 502),
    /// as when a connection drops after Cloudflare stored the Worker.
    UploadResponseLost,
}

#[derive(Debug, Clone)]
pub struct Script {
    pub module: Vec<u8>,
    pub secret: String,
    pub workload: String,
    pub deployment: String,
    pub metadata: Value,
    pub workers_dev: bool,
}

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct State {
    pub mode: Mode,
    pub scripts: BTreeMap<String, Script>,
    pub uploads: u32,
    pub invocations: u32,
    pub requests: Vec<Recorded>,
    pub subdomain: Option<String>,
}

#[derive(Clone)]
pub struct FakeCloudflare {
    pub addr: SocketAddr,
    pub state: Arc<Mutex<State>>,
}

impl FakeCloudflare {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State {
            subdomain: Some(SUBDOMAIN.into()),
            ..State::default()
        }));
        let shared = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let state = shared.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, state).await;
                });
            }
        });
        Self { addr, state }
    }

    pub fn set_mode(&self, mode: Mode) {
        self.state.lock().unwrap().mode = mode;
    }

    pub fn settings(&self) -> CloudflareSettings {
        let mut settings = CloudflareSettings::new(ACCOUNT);
        settings.api_base = Some(format!("http://{}/client/v4", self.addr));
        settings.workers_base = Some(format!("http://{}/w", self.addr));
        settings
    }

    pub fn provider(&self) -> CloudflareProvider {
        self.provider_with(self.settings(), Some(TOKEN))
    }

    pub fn provider_with(
        &self,
        settings: CloudflareSettings,
        token: Option<&str>,
    ) -> CloudflareProvider {
        CloudflareProvider::new(
            "edge",
            settings,
            token.and_then(Credential::new),
            CredentialSource::Environment("CLOUDFLARE_API_TOKEN".into()),
        )
        .unwrap()
    }

    pub fn uploads(&self) -> u32 {
        self.state.lock().unwrap().uploads
    }

    pub fn invocations(&self) -> u32 {
        self.state.lock().unwrap().invocations
    }

    pub fn scripts(&self) -> Vec<String> {
        self.state.lock().unwrap().scripts.keys().cloned().collect()
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }
}

struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buffer = vec![];
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\ncf-ray: 8f00test-LAX\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

fn envelope(result: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"success": true, "errors": [], "messages": [], "result": result}))
        .unwrap()
}

fn failure(code: u64, message: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "success": false,
        "errors": [{"code": code, "message": message}],
        "messages": [],
        "result": null
    }))
    .unwrap()
}

async fn serve(mut stream: TcpStream, state: Arc<Mutex<State>>) -> std::io::Result<()> {
    let Some(request) = read_request(&mut stream).await? else {
        return Ok(());
    };
    state.lock().unwrap().requests.push(Recorded {
        method: request.method.clone(),
        path: request.path.clone(),
        authorization: request.headers.get("authorization").cloned(),
        body: request.body.clone(),
    });
    if let Some(worker) = request.path.strip_prefix("/w/") {
        return invoke(&mut stream, &state, worker.trim_end_matches('/'), &request).await;
    }
    let (status, body) = api(&state, &request);
    respond(&mut stream, status, "application/json", &body).await
}

fn api(state: &Arc<Mutex<State>>, request: &Request) -> (u16, Vec<u8>) {
    let mode = state.lock().unwrap().mode;
    match mode {
        Mode::ApiRateLimited => {
            return (
                429,
                failure(
                    971,
                    "Please wait and consider throttling your request speed",
                ),
            );
        }
        Mode::ApiDown => return (500, failure(10013, "An unknown error has occurred")),
        _ => {}
    }
    if request.headers.get("authorization").map(String::as_str) != Some(&format!("Bearer {TOKEN}"))
    {
        return (403, failure(10000, "Authentication error"));
    }
    let prefix = format!("/client/v4/accounts/{ACCOUNT}");
    let Some(path) = request.path.strip_prefix(&prefix) else {
        return (
            404,
            failure(
                7003,
                "Could not route to /client/v4/accounts/..., perhaps your object identifier is invalid?",
            ),
        );
    };
    let path = path.split('?').next().unwrap_or_default();
    let mut state = state.lock().unwrap();
    let segments = path.trim_matches('/').split('/').collect::<Vec<_>>();
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["workers", "subdomain"]) => match &state.subdomain {
            Some(subdomain) => (200, envelope(json!({ "subdomain": subdomain }))),
            None => (404, failure(10007, "This account does not have a workers.dev subdomain")),
        },
        ("GET", ["workers", "scripts"]) => (
            200,
            envelope(Value::Array(
                state
                    .scripts
                    .keys()
                    .map(|name| json!({"id": name, "modified_on": "2026-10-17T00:00:00Z"}))
                    .chain(std::iter::once(
                        json!({"id": "someone-elses-worker", "modified_on": "2026-01-01T00:00:00Z"}),
                    ))
                    .collect(),
            )),
        ),
        ("GET", ["workers", "scripts", name, "settings"]) => match state.scripts.get(*name) {
            Some(script) => {
                let mut bindings = script.metadata["bindings"].clone();
                // Cloudflare never returns a secret's text.
                for binding in bindings.as_array_mut().into_iter().flatten() {
                    if binding["type"] == "secret_text" {
                        binding.as_object_mut().unwrap().remove("text");
                    }
                }
                (200, envelope(json!({ "bindings": bindings, "compatibility_date": script.metadata["compatibility_date"] })))
            }
            None => (404, failure(10007, "This Worker does not exist on your account.")),
        },
        ("PUT", ["workers", "scripts", name]) => {
            if state.mode == Mode::ScriptLimit && !state.scripts.contains_key(*name) {
                return (
                    400,
                    failure(10037, "You have exceeded the limit of 100 Workers on your account."),
                );
            }
            let content_type = request.headers.get("content-type").cloned().unwrap_or_default();
            let Some(parts) = multipart(&content_type, &request.body) else {
                return (400, failure(10021, "Uploaded script could not be parsed"));
            };
            let Some(metadata) = parts
                .get("metadata")
                .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
            else {
                return (400, failure(10021, "metadata is missing"));
            };
            for module in ["worker.mjs", "harness.mjs", "workload.wasm"] {
                if !parts.contains_key(module) {
                    return (400, failure(10021, &format!("{module} is missing")));
                }
            }
            if metadata["main_module"] != "worker.mjs" {
                return (400, failure(10021, "main_module must name an uploaded module"));
            }
            let binding = |name: &str| {
                metadata["bindings"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|binding| binding["name"] == name)
                    .and_then(|binding| binding["text"].as_str())
                    .unwrap_or_default()
                    .to_owned()
            };
            let script = Script {
                module: parts["workload.wasm"].clone(),
                secret: binding("COMPUTE_INVOKE_TOKEN"),
                workload: binding("COMPUTE_WORKLOAD_DIGEST"),
                deployment: binding("COMPUTE_DEPLOYMENT"),
                metadata: metadata.clone(),
                workers_dev: state.scripts.get(*name).is_some_and(|script| script.workers_dev),
            };
            state.scripts.insert((*name).to_owned(), script);
            state.uploads += 1;
            if state.mode == Mode::UploadResponseLost && state.uploads == 1 {
                return (502, b"<html>bad gateway</html>".to_vec());
            }
            (
                200,
                envelope(json!({
                    "id": name,
                    "etag": format!("etag-{}", state.uploads),
                    "modified_on": "2026-10-17T00:00:00Z"
                })),
            )
        }
        ("POST", ["workers", "scripts", name, "subdomain"]) => match state.scripts.get_mut(*name) {
            Some(script) => {
                script.workers_dev = true;
                (200, envelope(json!({"enabled": true})))
            }
            None => (404, failure(10007, "This Worker does not exist on your account.")),
        },
        ("DELETE", ["workers", "scripts", name]) => match state.scripts.remove(*name) {
            Some(_) => (200, envelope(Value::Null)),
            None => (404, failure(10007, "This Worker does not exist on your account.")),
        },
        _ => (404, failure(7000, "No route for that URI")),
    }
}

fn multipart(content_type: &str, body: &[u8]) -> Option<BTreeMap<String, Vec<u8>>> {
    let boundary = content_type.split("boundary=").nth(1)?;
    let delimiter = format!("--{boundary}");
    let mut parts = BTreeMap::new();
    let mut rest = body;
    loop {
        let start = find(rest, delimiter.as_bytes())? + delimiter.len();
        rest = &rest[start..];
        if rest.starts_with(b"--") {
            return Some(parts);
        }
        rest = rest.strip_prefix(b"\r\n")?;
        let header_end = find(rest, b"\r\n\r\n")?;
        let headers = String::from_utf8_lossy(&rest[..header_end]).to_string();
        let name = headers
            .split("name=\"")
            .nth(1)?
            .split('"')
            .next()?
            .to_owned();
        rest = &rest[header_end + 4..];
        let end = find(rest, format!("\r\n{delimiter}").as_bytes())?;
        parts.insert(name, rest[..end].to_vec());
        rest = &rest[end + 2..];
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn invoke(
    stream: &mut TcpStream,
    state: &Arc<Mutex<State>>,
    worker: &str,
    request: &Request,
) -> std::io::Result<()> {
    let (mode, script) = {
        let mut state = state.lock().unwrap();
        state.invocations += 1;
        (state.mode, state.scripts.get(worker).cloned())
    };
    let Some(script) = script else {
        return respond(stream, 404, "text/plain", b"error code: 1042").await;
    };
    match mode {
        Mode::DailyQuota => return respond(stream, 429, "text/plain", b"error code: 1027").await,
        Mode::CpuLimit => return respond(stream, 503, "text/plain", b"error code: 1102").await,
        Mode::Hang => {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            return Ok(());
        }
        _ => {}
    }
    let given = request
        .headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    if script.secret.is_empty() || given != script.secret {
        let body = br#"{"error":"unauthorized","message":"this Worker runs only for the Compute that deployed it"}"#;
        return respond(stream, 401, "application/json", body).await;
    }
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let args = body["args"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let env = body["env"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pair| Some((pair[0].as_str()?.to_owned(), pair[1].as_str()?.to_owned())))
        .collect::<Vec<_>>();
    let stdin = base64::engine::general_purpose::STANDARD
        .decode(body["stdin"].as_str().unwrap_or_default())
        .unwrap_or_default();
    let limit = body["max_output_bytes"].as_u64().unwrap_or(1 << 20) as usize;
    let module = script.module.clone();
    let outcome = tokio::task::spawn_blocking(move || run_wasi(&module, &args, &env, stdin))
        .await
        .unwrap();
    let reported = if mode == Mode::WrongWorkload {
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned()
    } else {
        script.workload.clone()
    };
    let encode = |bytes: &[u8]| {
        base64::engine::general_purpose::STANDARD.encode(&bytes[..bytes.len().min(limit)])
    };
    let response = match outcome {
        Outcome::Invalid(message) => json!({
            "protocol": "compute.cloudflare.invoke@1",
            "harness": "compute-harness/1",
            "workload": reported,
            "execution_id": body["execution_id"],
            "status": "invalid_module",
            "message": message,
        }),
        Outcome::Ran {
            exit_code,
            trap,
            stdout,
            stderr,
        } => json!({
            "protocol": "compute.cloudflare.invoke@1",
            "harness": "compute-harness/1",
            "workload": reported,
            "execution_id": body["execution_id"],
            "status": if trap.is_some() { "trapped" } else { "exited" },
            "exit_code": exit_code,
            "trap": trap,
            "stdout": encode(&stdout),
            "stdout_bytes": stdout.len(),
            "stderr": encode(&stderr),
            "stderr_bytes": stderr.len(),
            "duration_ms": 1,
        }),
    };
    respond(
        stream,
        200,
        "application/json",
        &serde_json::to_vec(&response).unwrap(),
    )
    .await
}

enum Outcome {
    Invalid(String),
    Ran {
        exit_code: Option<i32>,
        trap: Option<String>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
}

fn run_wasi(module: &[u8], args: &[String], env: &[(String, String)], stdin: Vec<u8>) -> Outcome {
    use wasmtime::{Engine, Linker, Module, Store};
    use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
    use wasmtime_wasi::preview1::{self, WasiP1Ctx};
    use wasmtime_wasi::{I32Exit, WasiCtxBuilder};

    let engine = Engine::default();
    let module = match Module::new(&engine, module) {
        Ok(module) => module,
        Err(error) => return Outcome::Invalid(error.to_string()),
    };
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    preview1::add_to_linker_sync(&mut linker, |context| context).unwrap();
    let stdout = MemoryOutputPipe::new(1 << 24);
    let stderr = MemoryOutputPipe::new(1 << 24);
    let mut wasi = WasiCtxBuilder::new();
    wasi.args(args)
        .stdin(MemoryInputPipe::new(stdin))
        .stdout(stdout.clone())
        .stderr(stderr.clone());
    for (name, value) in env {
        wasi.env(name, value);
    }
    let mut store = Store::new(&engine, wasi.build_p1());
    let instance = match linker.instantiate(&mut store, &module) {
        Ok(instance) => instance,
        Err(error) => return Outcome::Invalid(error.to_string()),
    };
    let Ok(start) = instance.get_typed_func::<(), ()>(&mut store, "_start") else {
        return Outcome::Invalid(
            "the module is not a WASI command: it must export memory and _start".into(),
        );
    };
    let (exit_code, trap) = match start.call(&mut store, ()) {
        Ok(()) => (Some(0), None),
        Err(error) => match error.downcast_ref::<I32Exit>() {
            Some(exit) => (Some(exit.0), None),
            None => (None, Some(error.to_string())),
        },
    };
    drop(store);
    Outcome::Ran {
        exit_code,
        trap,
        stdout: stdout.contents().to_vec(),
        stderr: stderr.contents().to_vec(),
    }
}

/// WASI command modules the tests deploy.
pub mod modules {
    fn compile(source: &str) -> Vec<u8> {
        wat::parse_str(source).expect("valid WAT")
    }

    const PRELUDE: &str = r#"
      (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
      (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
      (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
      (import "wasi_snapshot_preview1" "args_sizes_get" (func $args_sizes_get (param i32 i32) (result i32)))
      (import "wasi_snapshot_preview1" "args_get" (func $args_get (param i32 i32) (result i32)))
      (import "wasi_snapshot_preview1" "environ_sizes_get" (func $environ_sizes_get (param i32 i32) (result i32)))
      (import "wasi_snapshot_preview1" "environ_get" (func $environ_get (param i32 i32) (result i32)))
      (memory (export "memory") 1)
      ;; write(fd, ptr, len) through one iovec at 0
      (func $write (param $fd i32) (param $ptr i32) (param $len i32)
        (i32.store (i32.const 0) (local.get $ptr))
        (i32.store (i32.const 4) (local.get $len))
        (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 8))))
    "#;

    /// Prints `hello from cloudflare` and exits 0.
    pub fn hello() -> Vec<u8> {
        compile(&format!(
            r#"(module {PRELUDE}
              (data (i32.const 64) "hello from cloudflare\n")
              (func (export "_start") (call $write (i32.const 1) (i32.const 64) (i32.const 22))))"#
        ))
    }

    /// Writes to stderr and exits 3.
    pub fn exit3() -> Vec<u8> {
        compile(&format!(
            r#"(module {PRELUDE}
              (data (i32.const 64) "failing\n")
              (func (export "_start")
                (call $write (i32.const 2) (i32.const 64) (i32.const 8))
                (call $proc_exit (i32.const 3))))"#
        ))
    }

    /// Traps.
    pub fn trap() -> Vec<u8> {
        compile(&format!(
            r#"(module {PRELUDE} (func (export "_start") unreachable))"#
        ))
    }

    /// Echoes up to 1 KiB of stdin to stdout.
    pub fn echo_stdin() -> Vec<u8> {
        compile(&format!(
            r#"(module {PRELUDE}
              (func (export "_start")
                (i32.store (i32.const 16) (i32.const 1024))
                (i32.store (i32.const 20) (i32.const 1024))
                (drop (call $fd_read (i32.const 0) (i32.const 16) (i32.const 1) (i32.const 24)))
                (call $write (i32.const 1) (i32.const 1024) (i32.load (i32.const 24)))))"#
        ))
    }

    /// Prints its argument buffer, then its environment buffer (each
    /// entry NUL-terminated).
    pub fn show() -> Vec<u8> {
        compile(&format!(
            r#"(module {PRELUDE}
              (func (export "_start")
                (drop (call $args_sizes_get (i32.const 32) (i32.const 36)))
                (drop (call $args_get (i32.const 4096) (i32.const 8192)))
                (call $write (i32.const 1) (i32.const 8192) (i32.load (i32.const 36)))
                (drop (call $environ_sizes_get (i32.const 40) (i32.const 44)))
                (drop (call $environ_get (i32.const 4096) (i32.const 16384)))
                (call $write (i32.const 1) (i32.const 16384) (i32.load (i32.const 44)))))"#
        ))
    }

    /// A valid module that is not a WASI command (no `_start`).
    pub fn library() -> Vec<u8> {
        compile(
            r#"(module (memory (export "memory") 1) (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))"#,
        )
    }
}
