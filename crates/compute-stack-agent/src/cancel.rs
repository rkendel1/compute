//! Compute's side of cancelling a stack-agent work (compute#90, G3).
//!
//! Cancellation is a request to stop work, and what is reported is what actually stopped:
//!
//! - **Chip's loop.** [`request`] calls stack-agent's `POST /v1/work/{id}/cancel`. A queued work is
//!   removed before it starts (`cancelled`: nothing ran). A running work is asked to stop
//!   (`cancellation_requested`): Chip makes no further model call, but what is in flight is not
//!   interrupted and the work ends however Chip establishes. Interrupting an FX call in progress
//!   is Chip's and FX's (FX's cancellation is cooperative and checked before and between polls).
//! - **Compute's commands.** [`crate::ComputeSessionEnvironments::cancel_work`] cancels the
//!   work's running Compute commands with Compute's confirmed, idempotent job cancellation; a
//!   command interrupted while it ran is never reported as not having run.
//!
//! Nothing here retries, replays, or claims an effect did not happen.

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What stack-agent established about a cancellation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChipCancellation {
    /// Removed from the queue before it started: no model call, execution or observation.
    Cancelled,
    /// The work is running: no further model call will be made; work in flight is not
    /// interrupted, and the outcome is whatever Chip establishes.
    Requested,
    /// The work had already ended; nothing changed.
    AlreadyFinished,
    /// Chip does not know the work.
    Unknown,
    /// Anything else Chip answered, as it answered it.
    Other { status: u16, body: String },
}

/// Asks the stack-agent service at `service` (`host:port`) to cancel `work`. Idempotent: asking
/// again reports the work's current state and changes nothing that already ended.
pub async fn request(service: &str, work: &str) -> Result<ChipCancellation, String> {
    if work.is_empty()
        || !work
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!("{work:?} is not a work id"));
    }
    let exchange = async {
        let mut stream = tokio::net::TcpStream::connect(service)
            .await
            .map_err(|e| format!("cannot reach the Chip service at {service}: {e}"))?;
        let request = format!(
            "POST /v1/work/{work}/cancel HTTP/1.1\r\nHost: {service}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| format!("the cancellation request was not sent: {e}"))?;
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .map_err(|e| format!("the Chip service's answer was cut off: {e}"))?;
        Ok::<_, String>(response)
    };
    let response = tokio::time::timeout(Duration::from_secs(30), exchange)
        .await
        .map_err(|_| "the Chip service did not answer the cancellation in time".to_string())??;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "the Chip service answered with no HTTP response".to_string())?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| "the Chip service answered with no status".to_string())?;
    let json = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let error = json.pointer("/error/code").and_then(Value::as_str);
    Ok(match (status, json["status"].as_str(), error) {
        (200, Some("cancelled"), _) => ChipCancellation::Cancelled,
        (200, Some("cancellation_requested"), _) => ChipCancellation::Requested,
        (409, _, Some("work_already_finished")) => ChipCancellation::AlreadyFinished,
        (404, _, _) => ChipCancellation::Unknown,
        _ => ChipCancellation::Other {
            status,
            body: body.chars().take(500).collect(),
        },
    })
}
