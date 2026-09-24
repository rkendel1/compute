//! The Compute API: one lifecycle API for the CLI, the UI, and AppPort.
//!
//! ```text
//! GET    /status                                   daemon status
//! POST   /shutdown
//! GET    /environments                             POST /environments
//! GET    /environments/:id                         DELETE /environments/:id
//! POST   /environments/:id/start | stop | restart
//! GET    /environments/:id/status
//! GET    /environments/:id/projects                POST /environments/:id/projects
//! GET    /environments/:id/projects/:project       DELETE /environments/:id/projects/:project
//! POST   /environments/:id/projects/:project/start | stop | restart
//! GET    /environments/:id/projects/:project/status
//! GET    /environments/:id/projects/:project/workloads/:workload
//! POST   /environments/:id/projects/:project/workloads/:workload/start | stop | restart | run
//! GET    /environments/:id/projects/:project/workloads/:workload/logs
//! GET    /executions/:execution_id
//! ```
//!
//! `:id` is an environment name or environment ID. Every request passes
//! through the provider authorization boundary: reads as
//! `EnvironmentRead`, every mutation as `EnvironmentMutate`. There is no
//! other path into the daemon.

use std::sync::Arc;

use compute_provider::{ProviderAuthorizer, ProviderOperation};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::EnvironmentError;
use crate::daemon::Daemon;
use crate::model::{DesiredState, EnvironmentDefinition, ProjectDefinition};

pub const API_VERSION: &str = "compute.api@1";
const MAX_BODY_BYTES: usize = 512 * 1024 * 1024;

/// Serve the API until the daemon shuts down.
pub async fn serve(
    listener: TcpListener,
    daemon: Arc<Daemon>,
    authorizer: Arc<dyn ProviderAuthorizer>,
) -> std::io::Result<()> {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let daemon = daemon.clone();
                let authorizer = authorizer.clone();
                tokio::spawn(async move {
                    let _ = handle(stream, daemon, authorizer).await;
                });
            }
            () = daemon.wait_for_shutdown() => return Ok(()),
        }
    }
}

struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> Result<Request, EnvironmentError> {
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
    let path = request_line.next().unwrap_or_default().to_string();
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
        path,
        authorization,
        body,
    })
}

async fn write_json(
    stream: &mut TcpStream,
    status: u16,
    value: &impl Serialize,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nX-Compute-Api: {API_VERSION}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.shutdown().await
}

async fn handle(
    mut stream: TcpStream,
    daemon: Arc<Daemon>,
    authorizer: Arc<dyn ProviderAuthorizer>,
) -> std::io::Result<()> {
    let result = match read_request(&mut stream).await {
        Ok(request) => route(&daemon, authorizer.as_ref(), request).await,
        Err(error) => Err(error),
    };
    match result {
        Ok((status, value)) => write_json(&mut stream, status, &value).await,
        Err(error) => {
            write_json(
                &mut stream,
                error.status(),
                &serde_json::json!({ "kind": error.kind(), "message": error.message() }),
            )
            .await
        }
    }
}

fn to_value(value: impl Serialize) -> Result<Value, EnvironmentError> {
    Ok(serde_json::to_value(value)?)
}

async fn route(
    daemon: &Arc<Daemon>,
    authorizer: &dyn ProviderAuthorizer,
    request: Request,
) -> Result<(u16, Value), EnvironmentError> {
    let path = request.path.split('?').next().unwrap_or_default();
    let segments = path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let method = request.method.as_str();
    let operation = if method == "GET" {
        ProviderOperation::EnvironmentRead
    } else {
        ProviderOperation::EnvironmentMutate
    };
    authorizer
        .authorize(operation, request.authorization.as_deref())
        .await
        .map_err(|error| EnvironmentError::Unauthorized(error.message))?;
    let body = || -> Result<Value, EnvironmentError> {
        if request.body.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_slice(&request.body)?)
        }
    };
    let ok = |value: Value| Ok((200, value));
    match (method, segments.as_slice()) {
        ("GET", ["status"]) => ok(to_value(daemon.status().await)?),
        ("POST", ["shutdown"]) => {
            let daemon = daemon.clone();
            tokio::spawn(async move { daemon.shutdown().await });
            ok(serde_json::json!({ "shutdown": true }))
        }
        ("GET", ["environments"]) => ok(to_value(daemon.environments().await)?),
        ("POST", ["environments"]) => {
            let definition: EnvironmentDefinition = serde_json::from_value(body()?)?;
            Ok((201, to_value(daemon.create_environment(definition).await?)?))
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
        ("GET", ["environments", id, "projects"]) => {
            ok(to_value(daemon.environment(id).await?.projects)?)
        }
        ("POST", ["environments", id, "projects"]) => {
            let definition: ProjectDefinition = serde_json::from_value(body()?)?;
            Ok((201, to_value(daemon.add_project(id, definition).await?)?))
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
        ("GET", ["executions", execution]) => ok(to_value(daemon.execution(execution).await?)?),
        _ => Err(EnvironmentError::NotFound(format!("{method} {path}"))),
    }
}

fn lifecycle(action: &str) -> (DesiredState, bool) {
    match action {
        "stop" => (DesiredState::Stopped, false),
        "restart" => (DesiredState::Running, true),
        _ => (DesiredState::Running, false),
    }
}
