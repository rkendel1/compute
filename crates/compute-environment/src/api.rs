//! The Compute API, `compute.api@1`: one control-plane API for the CLI,
//! the UI, and AppPort. [`ROUTES`] lists every operation; the UI uses no
//! other.
//!
//! Every request is authenticated to an operator and authorized against
//! the scope its route declares ([`crate::auth::required_scope`]); reads
//! included. Every mutation is recorded in the audit trail with its request
//! ID, operator, and credential. Every execution a mutation causes passes
//! through admission. There is no other path into the daemon.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::EnvironmentError;
use crate::auth::{Principal, RequestContext, required_scope};
use crate::daemon::{Daemon, EventFilter};
use crate::model::*;

pub const API_VERSION: &str = "compute.api@1";
const MAX_BODY_BYTES: usize = 512 * 1024 * 1024;

/// Every operation of the Compute API, as `(method, path)`. Parameters are
/// written `{name}`. The UI/API parity test holds the UI to this list.
pub const ROUTES: &[(&str, &str)] = &[
    ("GET", "/health"),
    ("GET", "/info"),
    ("GET", "/status"),
    ("GET", "/auth/whoami"),
    ("GET", "/auth/credentials"),
    ("POST", "/auth/credentials"),
    ("POST", "/auth/credentials/{credential}/revoke"),
    ("POST", "/auth/credentials/{credential}/rotate"),
    ("GET", "/audit"),
    ("POST", "/shutdown"),
    ("GET", "/environments"),
    ("POST", "/environments"),
    ("GET", "/environments/{environment}"),
    ("DELETE", "/environments/{environment}"),
    ("GET", "/environments/{environment}/status"),
    ("POST", "/environments/{environment}/start"),
    ("POST", "/environments/{environment}/stop"),
    ("POST", "/environments/{environment}/restart"),
    ("GET", "/environments/{environment}/projects"),
    ("POST", "/environments/{environment}/projects"),
    ("GET", "/environments/{environment}/projects/{project}"),
    ("DELETE", "/environments/{environment}/projects/{project}"),
    (
        "GET",
        "/environments/{environment}/projects/{project}/status",
    ),
    (
        "POST",
        "/environments/{environment}/projects/{project}/start",
    ),
    (
        "POST",
        "/environments/{environment}/projects/{project}/stop",
    ),
    (
        "POST",
        "/environments/{environment}/projects/{project}/restart",
    ),
    (
        "GET",
        "/environments/{environment}/projects/{project}/executions",
    ),
    (
        "GET",
        "/environments/{environment}/projects/{project}/receipts",
    ),
    (
        "GET",
        "/environments/{environment}/projects/{project}/workloads/{workload}",
    ),
    (
        "POST",
        "/environments/{environment}/projects/{project}/workloads/{workload}/start",
    ),
    (
        "POST",
        "/environments/{environment}/projects/{project}/workloads/{workload}/stop",
    ),
    (
        "POST",
        "/environments/{environment}/projects/{project}/workloads/{workload}/restart",
    ),
    (
        "POST",
        "/environments/{environment}/projects/{project}/workloads/{workload}/run",
    ),
    (
        "GET",
        "/environments/{environment}/projects/{project}/workloads/{workload}/logs",
    ),
    ("GET", "/projects"),
    ("GET", "/projects/{project}"),
    ("GET", "/projects/{project}/status"),
    ("GET", "/projects/{project}/revisions"),
    ("POST", "/projects/{project}/revisions"),
    ("GET", "/deployments"),
    ("POST", "/deployments"),
    ("POST", "/deployments/promote"),
    ("GET", "/deployments/{deployment}"),
    ("GET", "/deployments/{deployment}/receipt"),
    ("POST", "/deployments/{deployment}/rollback"),
    ("GET", "/domains"),
    ("POST", "/domains"),
    ("GET", "/domains/{domain}"),
    ("DELETE", "/domains/{domain}"),
    ("GET", "/dns"),
    ("POST", "/dns/reconcile"),
    ("GET", "/certificates"),
    ("POST", "/certificates/{domain}/renew"),
    ("GET", "/network"),
    ("GET", "/executions/{execution}"),
    ("GET", "/receipts/{receipt}"),
    ("GET", "/events"),
    ("GET", "/events/stream"),
    ("GET", "/providers"),
    ("GET", "/services"),
    ("POST", "/services"),
    ("DELETE", "/services/{service}"),
];

const UI_HTML: &str = include_str!("../ui/index.html");
const UI_SCRIPT: &str = include_str!("../ui/app.js");
const UI_STYLE: &str = include_str!("../ui/app.css");

/// A connection: plaintext or TLS.
trait Connection: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Connection for T {}
type Stream = Box<dyn Connection>;

/// Serve the API until the daemon shuts down. With `tls`, every connection
/// is TLS; a handshake that fails never reaches the API.
pub async fn serve(
    listener: TcpListener,
    daemon: Arc<Daemon>,
    tls: Option<Arc<crate::tls::ApiTls>>,
) -> std::io::Result<()> {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let daemon = daemon.clone();
                let tls = tls.clone();
                tokio::spawn(async move {
                    let stream: Stream = match tls {
                        Some(tls) => match tls.accept(stream).await {
                            Ok(stream) => Box::new(stream),
                            Err(_) => return,
                        },
                        None => Box::new(stream),
                    };
                    let _ = handle(stream, daemon).await;
                });
            }
            () = daemon.wait_for_shutdown() => return Ok(()),
        }
    }
}

struct Request {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    authorization: Option<String>,
    body: Vec<u8>,
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = bytes.get(index + 1..index + 3)
            && let Some(byte) = std::str::from_utf8(hex)
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            decoded.push(byte);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

async fn read_request(stream: &mut Stream) -> Result<Request, EnvironmentError> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Err(EnvironmentError::Invalid("connection closed".into()));
        }
        buffer.extend_from_slice(&chunk[..count]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > 64 * 1024 {
            return Err(EnvironmentError::Invalid(
                "request headers too large".into(),
            ));
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_string();
    let target = request_line.next().unwrap_or_default().to_string();
    let (path, query_string) = target.split_once('?').unwrap_or((&target, ""));
    let query = query_string
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let pair = pair.replace('+', " ");
            let (name, value) = pair.split_once('=').unwrap_or((&pair, ""));
            (percent_decode(name), percent_decode(value))
        })
        .collect();
    let mut content_length = 0_usize;
    let mut authorization = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => {
                    content_length = value
                        .parse()
                        .map_err(|_| EnvironmentError::Invalid("invalid content length".into()))?;
                }
                "authorization" => authorization = Some(value.to_string()),
                _ => {}
            }
        }
    }
    if content_length > MAX_BODY_BYTES {
        return Err(EnvironmentError::Invalid("request body too large".into()));
    }
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..count]);
    }
    body.truncate(content_length);
    Ok(Request {
        method,
        path: path.to_string(),
        query,
        authorization,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

async fn write_response(
    stream: &mut Stream,
    status: u16,
    content_type: &str,
    body: &[u8],
    request_id: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nX-Compute-Api: {API_VERSION}\r\nX-Request-Id: {request_id}\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

async fn write_json(
    stream: &mut Stream,
    status: u16,
    value: &impl Serialize,
    request_id: &str,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    write_response(stream, status, "application/json", &body, request_id).await
}

enum Response {
    Json(u16, Value),
    Static(&'static str, &'static str),
    Redirect(&'static str),
    Stream(EventFilter),
}

async fn handle(mut stream: Stream, daemon: Arc<Daemon>) -> std::io::Result<()> {
    let request_id = crate::auth::request_id();
    let result = match read_request(&mut stream).await {
        Ok(request) => dispatch(&daemon, request, &request_id).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(Response::Json(status, value)) => {
            write_json(&mut stream, status, &value, &request_id).await
        }
        Ok(Response::Static(content_type, body)) => {
            write_response(&mut stream, 200, content_type, body.as_bytes(), &request_id).await
        }
        Ok(Response::Redirect(location)) => {
            let head = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(head.as_bytes()).await?;
            stream.shutdown().await
        }
        Ok(Response::Stream(filter)) => stream_events(stream, daemon, filter).await,
        Err(error) => {
            write_json(
                &mut stream,
                error.status(),
                &serde_json::json!({
                    "kind": error.kind(),
                    "message": error.message(),
                    "request_id": request_id,
                }),
                &request_id,
            )
            .await
        }
    }
}

/// Authenticate, authorize, run, and audit one request.
async fn dispatch(
    daemon: &Arc<Daemon>,
    request: Request,
    request_id: &str,
) -> Result<Response, EnvironmentError> {
    let segments = request
        .path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(percent_decode)
        .collect::<Vec<_>>();
    let segments = segments.iter().map(String::as_str).collect::<Vec<_>>();
    let method = request.method.clone();
    // The UI's static assets and liveness carry no state.
    match (method.as_str(), segments.as_slice()) {
        ("GET", []) => return Ok(Response::Redirect("/ui/")),
        ("GET", ["ui"]) => return Ok(Response::Static("text/html; charset=utf-8", UI_HTML)),
        ("GET", ["ui", "app.js"]) => {
            return Ok(Response::Static(
                "text/javascript; charset=utf-8",
                UI_SCRIPT,
            ));
        }
        ("GET", ["ui", "app.css"]) => {
            return Ok(Response::Static("text/css; charset=utf-8", UI_STYLE));
        }
        ("GET", ["health"]) => return Ok(Response::Json(200, daemon.health().await)),
        _ => {}
    }
    let mutation = method != "GET";
    let operation = operation_name(&method, &segments);
    let (resource, resource_id) = resource_of(&segments);
    let audit = |operator: &str,
                 credential: Option<String>,
                 result: &str,
                 status: u16,
                 error: Option<&EnvironmentError>,
                 detail: serde_json::Map<String, Value>| {
        compute_state::AuditRecord {
            request_id: request_id.to_string(),
            operator_id: operator.to_string(),
            credential_id: credential,
            operation: operation.clone(),
            resource: resource.clone(),
            resource_id: resource_id.clone(),
            result: result.into(),
            status,
            error_kind: error.map(|error| error.kind().to_string()),
            detail,
            at: chrono::Utc::now(),
        }
    };
    let principal = match daemon.authenticate(request.authorization.as_deref()) {
        Ok(principal) => principal,
        Err(error) => {
            daemon
                .refused(
                    compute_state::events::AUTHENTICATION_FAILED,
                    format!("{operation}: {}", error.message()),
                    serde_json::json!({ "request_id": request_id, "operation": operation }),
                )
                .await;
            if mutation {
                daemon
                    .audit(audit(
                        "anonymous",
                        None,
                        "rejected",
                        error.status(),
                        Some(&error),
                        Default::default(),
                    ))
                    .await;
            }
            return Err(error);
        }
    };
    let scope = required_scope(&method, &segments);
    if !principal.allows(scope) {
        let error = EnvironmentError::Forbidden(format!(
            "{operation} needs {}; {} has {}",
            scope.as_str(),
            principal.operator_id,
            principal
                .scopes
                .iter()
                .map(|scope| scope.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        daemon
            .refused(
                compute_state::events::AUTHORIZATION_DENIED,
                format!("{} was denied {operation}", principal.operator_id),
                serde_json::json!({
                    "request_id": request_id,
                    "operator_id": principal.operator_id,
                    "credential_id": principal.credential_id,
                    "operation": operation,
                    "scope": scope.as_str(),
                }),
            )
            .await;
        if mutation {
            daemon
                .audit(audit(
                    &principal.operator_id,
                    principal.credential_id.clone(),
                    "rejected",
                    error.status(),
                    Some(&error),
                    Default::default(),
                ))
                .await;
        }
        return Err(error);
    }
    let context = RequestContext {
        request_id: request_id.to_string(),
        operator_id: principal.operator_id.clone(),
        credential_id: principal.credential_id.clone(),
    };
    let result = crate::auth::REQUEST
        .scope(context, route(daemon, &principal, request, &segments))
        .await;
    if mutation {
        let record = match &result {
            Ok(Response::Json(status, value)) => audit(
                &principal.operator_id,
                principal.credential_id.clone(),
                "accepted",
                *status,
                None,
                detail_of(value),
            ),
            Ok(_) => audit(
                &principal.operator_id,
                principal.credential_id.clone(),
                "accepted",
                200,
                None,
                Default::default(),
            ),
            Err(error) => audit(
                &principal.operator_id,
                principal.credential_id.clone(),
                "failed",
                error.status(),
                Some(error),
                Default::default(),
            ),
        };
        daemon.audit(record).await;
    }
    result
}

/// `METHOD /route/{template}`: the route with identifiers replaced by the
/// names of their parameters, as [`ROUTES`] lists it.
fn operation_name(method: &str, segments: &[&str]) -> String {
    for (route_method, route) in ROUTES {
        if *route_method != method {
            continue;
        }
        let template = route
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if template.len() == segments.len()
            && template
                .iter()
                .zip(segments)
                .all(|(expected, actual)| expected.starts_with('{') || expected == actual)
        {
            return format!("{method} {route}");
        }
    }
    format!("{method} /{}", segments.join("/"))
}

/// The resource a route acts on and its identifier: the innermost named
/// collection with an identifier after it.
fn resource_of(segments: &[&str]) -> (String, Option<String>) {
    let singular = |name: &str| {
        match name {
            "environments" => "environment",
            "projects" => "project",
            "workloads" => "workload",
            "deployments" => "deployment",
            "domains" => "domain",
            "certificates" => "certificate",
            "credentials" => "credential",
            "services" => "service",
            "executions" => "execution",
            "receipts" => "receipt",
            other => other,
        }
        .to_string()
    };
    let mut resource = (
        segments
            .first()
            .map(|name| singular(name))
            .unwrap_or_default(),
        None,
    );
    let mut path = vec![];
    let mut index = 0;
    while index < segments.len() {
        let name = segments[index];
        if let Some(id) = segments.get(index + 1)
            && [
                "environments",
                "projects",
                "workloads",
                "deployments",
                "domains",
                "certificates",
                "credentials",
                "services",
            ]
            .contains(&name)
            && !matches!(*id, "promote" | "reconcile")
        {
            path.push(id.to_string());
            resource = (singular(name), Some(path.join("/")));
            index += 2;
        } else {
            index += 1;
        }
    }
    resource
}

/// Identifiers a mutation's response names, for the audit record. Never
/// a token: credential responses carry one, and it is left out.
fn detail_of(value: &Value) -> serde_json::Map<String, Value> {
    const KEYS: &[&str] = &[
        "deployment_id",
        "revision",
        "revision_id",
        "project",
        "environment",
        "execution_id",
        "credential_id",
        "status",
    ];
    let mut detail = serde_json::Map::new();
    let mut collect = |object: &serde_json::Map<String, Value>| {
        for key in KEYS {
            if let Some(value) = object.get(*key)
                && (value.is_string() || value.is_number())
                && !detail.contains_key(*key)
            {
                detail.insert((*key).into(), value.clone());
            }
        }
    };
    if let Some(object) = value.as_object() {
        collect(object);
        for nested in ["credential", "record", "deployment"] {
            if let Some(object) = object.get(nested).and_then(Value::as_object) {
                collect(object);
            }
        }
    }
    detail.remove("token");
    detail
}

/// Server-sent events: the events after `after`, then new ones as they are
/// recorded. The UI refreshes what an event names.
async fn stream_events(
    mut stream: Stream,
    daemon: Arc<Daemon>,
    filter: EventFilter,
) -> std::io::Result<()> {
    let mut receiver = daemon.subscribe();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Compute-Api: {API_VERSION}\r\nConnection: keep-alive\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await?;
    let mut last = filter.after.unwrap_or_default();
    if filter.after.is_some()
        && let Ok(events) = daemon.events(filter.clone()).await
    {
        for event in events {
            last = last.max(event.sequence);
            write_event(&mut stream, &event).await?;
        }
    }
    let matches = |event: &compute_state::EventRecord| {
        filter
            .environment
            .as_ref()
            .is_none_or(|environment| event.environment.as_ref() == Some(environment))
            && filter
                .project
                .as_ref()
                .is_none_or(|project| event.project.as_ref() == Some(project))
    };
    loop {
        tokio::select! {
            received = receiver.recv() => match received {
                Ok(event) if event.sequence > last && matches(&event) => {
                    last = event.sequence;
                    write_event(&mut stream, &event).await?;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    stream.write_all(b"event: lagged\ndata: {}\n\n").await?;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            },
            () = tokio::time::sleep(Duration::from_secs(15)) => {
                stream.write_all(b": keepalive\n\n").await?;
            }
            () = daemon.wait_for_shutdown() => return Ok(()),
        }
    }
}

async fn write_event(
    stream: &mut Stream,
    event: &compute_state::EventRecord,
) -> std::io::Result<()> {
    let data = serde_json::to_string(event).unwrap_or_default();
    stream
        .write_all(format!("id: {}\ndata: {data}\n\n", event.sequence).as_bytes())
        .await?;
    stream.flush().await
}

fn to_value(value: impl Serialize) -> Result<Value, EnvironmentError> {
    Ok(serde_json::to_value(value)?)
}

fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, EnvironmentError> {
    let value = if body.is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_slice(body)?
    };
    serde_json::from_value(value).map_err(|error| EnvironmentError::Invalid(error.to_string()))
}

async fn route(
    daemon: &Arc<Daemon>,
    principal: &Principal,
    request: Request,
    segments: &[&str],
) -> Result<Response, EnvironmentError> {
    let method = request.method.as_str();
    let body = request.body.as_slice();
    let query = &request.query;
    let ok = |value: Value| Ok(Response::Json(200, value));
    let created = |value: Value| Ok(Response::Json(201, value));
    let limit = |default: usize| {
        query
            .get("limit")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(default)
    };
    match (method, segments) {
        ("GET", ["info"]) => ok(to_value(daemon.info().await)?),
        ("GET", ["status"]) => ok(to_value(daemon.status().await)?),

        // Operators.
        ("GET", ["auth", "whoami"]) => ok(to_value(principal)?),
        ("GET", ["auth", "credentials"]) => ok(to_value(daemon.credentials().await?)?),
        ("POST", ["auth", "credentials"]) => created(to_value(
            daemon.create_credential(principal, parse(body)?).await?,
        )?),
        ("POST", ["auth", "credentials", id, "revoke"]) => {
            ok(to_value(daemon.revoke_credential(id).await?)?)
        }
        ("POST", ["auth", "credentials", id, "rotate"]) => created(to_value(
            daemon
                .rotate_credential(principal, id, parse(body)?)
                .await?,
        )?),
        ("GET", ["audit"]) => ok(to_value(
            daemon
                .audit_records(query.get("operator").map(String::as_str), limit(100))
                .await?,
        )?),
        ("POST", ["shutdown"]) => {
            let daemon = daemon.clone();
            tokio::spawn(async move { daemon.shutdown().await });
            ok(serde_json::json!({ "shutdown": true }))
        }

        // Environments.
        ("GET", ["environments"]) => ok(to_value(daemon.environments().await?)?),
        ("POST", ["environments"]) => {
            created(to_value(daemon.create_environment(parse(body)?).await?)?)
        }
        ("GET", ["environments", id]) | ("GET", ["environments", id, "status"]) => {
            ok(to_value(daemon.environment(id).await?)?)
        }
        ("DELETE", ["environments", id]) => {
            daemon.destroy_environment(id).await?;
            ok(serde_json::json!({ "destroyed": id }))
        }
        ("POST", ["environments", id, action @ ("start" | "stop" | "restart")]) => {
            let (desired, restart) = lifecycle(action);
            ok(to_value(
                daemon.set_environment_state(id, desired, restart).await?,
            )?)
        }

        // Projects in an environment.
        ("GET", ["environments", id, "projects"]) => {
            ok(to_value(daemon.environment(id).await?.projects)?)
        }
        ("POST", ["environments", id, "projects"]) => {
            created(to_value(daemon.add_project(id, parse(body)?).await?)?)
        }
        ("GET", ["environments", id, "projects", project])
        | ("GET", ["environments", id, "projects", project, "status"]) => {
            ok(to_value(daemon.project(id, project).await?)?)
        }
        ("DELETE", ["environments", id, "projects", project]) => {
            daemon.remove_project(id, project).await?;
            ok(serde_json::json!({ "removed": project }))
        }
        (
            "POST",
            [
                "environments",
                id,
                "projects",
                project,
                action @ ("start" | "stop" | "restart"),
            ],
        ) => {
            let (desired, restart) = lifecycle(action);
            ok(to_value(
                daemon
                    .set_project_state(id, project, desired, restart)
                    .await?,
            )?)
        }
        ("GET", ["environments", id, "projects", project, "executions"]) => {
            ok(to_value(daemon.executions(id, project, limit(50)).await?)?)
        }
        ("GET", ["environments", id, "projects", project, "receipts"]) => {
            ok(to_value(daemon.receipts(id, project, limit(50)).await?)?)
        }

        // Workloads.
        (
            "GET",
            [
                "environments",
                id,
                "projects",
                project,
                "workloads",
                workload,
            ],
        ) => ok(to_value(daemon.workload(id, project, workload).await?)?),
        (
            "POST",
            [
                "environments",
                id,
                "projects",
                project,
                "workloads",
                workload,
                action @ ("start" | "stop" | "restart"),
            ],
        ) => {
            let (desired, restart) = lifecycle(action);
            ok(to_value(
                daemon
                    .set_workload_state(id, project, workload, desired, restart)
                    .await?,
            )?)
        }
        (
            "POST",
            [
                "environments",
                id,
                "projects",
                project,
                "workloads",
                workload,
                "run",
            ],
        ) => ok(to_value(daemon.run_task(id, project, workload).await?)?),
        (
            "GET",
            [
                "environments",
                id,
                "projects",
                project,
                "workloads",
                workload,
                "logs",
            ],
        ) => {
            let (stdout, stderr) = daemon.logs(id, project, workload).await?;
            ok(serde_json::json!({ "stdout": stdout, "stderr": stderr }))
        }

        // Projects across environments.
        ("GET", ["projects"]) => ok(to_value(daemon.projects().await?)?),
        ("GET", ["projects", project]) => ok(to_value(daemon.project_detail(project).await?)?),
        ("GET", ["projects", project, "status"]) => {
            ok(to_value(daemon.project_detail(project).await?.summary)?)
        }
        ("GET", ["projects", project, "revisions"]) => {
            ok(to_value(daemon.revisions(project).await?)?)
        }
        ("POST", ["projects", project, "revisions"]) => created(to_value(
            daemon.register_revision(project, parse(body)?).await?,
        )?),

        // Deployments.
        ("GET", ["deployments"]) => ok(to_value(
            daemon
                .deployments(
                    query.get("environment").cloned(),
                    query.get("project").cloned(),
                    Some(limit(50)),
                )
                .await?,
        )?),
        ("POST", ["deployments"]) => created(to_value(daemon.deploy(parse(body)?).await?)?),
        ("POST", ["deployments", "promote"]) => {
            created(to_value(daemon.promote(parse(body)?).await?)?)
        }
        ("GET", ["deployments", id]) => ok(to_value(daemon.deployment(id).await?)?),
        ("GET", ["deployments", id, "receipt"]) => {
            ok(daemon.deployment_receipt_document(id).await?)
        }
        ("POST", ["deployments", id, "rollback"]) => ok(to_value(daemon.rollback(id).await?)?),

        // Network.
        ("GET", ["domains"]) => ok(to_value(daemon.domains().await?)?),
        ("POST", ["domains"]) => created(to_value(daemon.add_domain(parse(body)?).await?)?),
        ("GET", ["domains", name]) => ok(to_value(daemon.domain(name).await?)?),
        ("DELETE", ["domains", name]) => {
            daemon.remove_domain(name).await?;
            ok(serde_json::json!({ "removed": name }))
        }
        ("GET", ["dns"]) => ok(to_value(daemon.dns_status().await?)?),
        ("POST", ["dns", "reconcile"]) => ok(to_value(daemon.reconcile_dns().await?)?),
        ("GET", ["certificates"]) => ok(to_value(daemon.certificates().await?)?),
        ("POST", ["certificates", domain, "renew"]) => {
            ok(to_value(daemon.renew_certificate(domain).await?)?)
        }
        ("GET", ["network"]) => ok(to_value(daemon.network_status().await)?),

        // Evidence.
        ("GET", ["executions", execution]) => ok(to_value(daemon.execution(execution).await?)?),
        ("GET", ["receipts", receipt]) => ok(daemon.receipt(receipt).await?),
        ("GET", ["events"]) => ok(to_value(daemon.events(event_filter(query)).await?)?),
        ("GET", ["events", "stream"]) => Ok(Response::Stream(event_filter(query))),

        // Pool and shared services.
        ("GET", ["providers"]) => ok(to_value(daemon.providers().await?)?),
        ("GET", ["services"]) => ok(to_value(daemon.services().await?)?),
        ("POST", ["services"]) => created(to_value(daemon.register_service(parse(body)?).await?)?),
        ("DELETE", ["services", name]) => {
            daemon.remove_service(name).await?;
            ok(serde_json::json!({ "removed": name }))
        }
        _ => Err(EnvironmentError::NoRoute(format!(
            "{method} {}",
            request.path
        ))),
    }
}

fn event_filter(query: &BTreeMap<String, String>) -> EventFilter {
    EventFilter {
        after: query.get("after").and_then(|value| value.parse().ok()),
        limit: query.get("limit").and_then(|value| value.parse().ok()),
        environment: query.get("environment").cloned(),
        project: query.get("project").cloned(),
        deployment_id: query.get("deployment").cloned(),
    }
}

fn lifecycle(action: &str) -> (DesiredState, bool) {
    match action {
        "stop" => (DesiredState::Stopped, false),
        "restart" => (DesiredState::Running, true),
        _ => (DesiredState::Running, false),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn percent_decoding() {
        assert_eq!(super::percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(super::percent_decode("100%"), "100%");
        assert_eq!(super::percent_decode("%zz"), "%zz");
    }
}
