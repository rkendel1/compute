//! The REST adapter against a local server: the request it sends and how it
//! reports each answer. The credential must appear only in the
//! `Authorization` header, never in an error.

use std::sync::{Arc, Mutex};

use compute_worker_github::{GitHubApi, Repository, RestGitHubApi, Secret};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CREDENTIAL: &str = "ghp_CREDENTIAL0123456789-do-not-leak";

/// Serve one request with `status`/`body`; return the base URL and what was
/// received.
async fn server(status: u16, body: &'static str) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let received = Arc::new(Mutex::new(String::new()));
    let sink = received.clone();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0u8; 8192];
        let read = stream.read(&mut buffer).await.unwrap();
        *sink.lock().unwrap() = String::from_utf8_lossy(&buffer[..read]).into_owned();
        let response = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    (url, received)
}

#[tokio::test]
async fn a_registration_token_is_requested_for_the_repository() {
    let (url, received) = server(
        201,
        r#"{"token":"AREG123","expires_at":"2030-01-01T00:00:00Z"}"#,
    )
    .await;
    let api = RestGitHubApi::new(url).unwrap();
    let token = api
        .registration_token(
            &Repository::parse("rkendel1/compute").unwrap(),
            &Secret::new(CREDENTIAL),
        )
        .await
        .unwrap();
    assert_eq!(token.expose(), "AREG123");
    let request = received.lock().unwrap().to_lowercase();
    assert!(
        request.starts_with("post /repos/rkendel1/compute/actions/runners/registration-token ")
    );
    assert!(request.contains(&format!(
        "authorization: bearer {}",
        CREDENTIAL.to_lowercase()
    )));
}

#[tokio::test]
async fn an_unknown_repository_is_reported_as_such() {
    let (url, _) = server(404, r#"{"message":"Not Found"}"#).await;
    let error = RestGitHubApi::new(url)
        .unwrap()
        .registration_token(
            &Repository::parse("nobody/nothing").unwrap(),
            &Secret::new(CREDENTIAL),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "repository_not_found");
    assert!(!error.to_string().contains(CREDENTIAL));
}

#[tokio::test]
async fn a_rejected_credential_is_reported_without_echoing_it() {
    let body: &'static str =
        Box::leak(format!(r#"{{"message":"Bad credentials {CREDENTIAL}"}}"#).into_boxed_str());
    let (url, _) = server(401, body).await;
    let error = RestGitHubApi::new(url)
        .unwrap()
        .registration_token(&Repository::parse("o/r").unwrap(), &Secret::new(CREDENTIAL))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "github_unauthorized");
    assert!(!error.to_string().contains(CREDENTIAL), "{error}");
}

#[tokio::test]
async fn an_answer_without_a_token_is_malformed() {
    let (url, _) = server(201, r#"{"expires_at":"x"}"#).await;
    let error = RestGitHubApi::new(url)
        .unwrap()
        .registration_token(&Repository::parse("o/r").unwrap(), &Secret::new(CREDENTIAL))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "github_malformed_response");
}

#[tokio::test]
async fn an_unreachable_server_is_reported_without_the_credential() {
    let error = RestGitHubApi::new("http://127.0.0.1:1")
        .unwrap()
        .registration_token(&Repository::parse("o/r").unwrap(), &Secret::new(CREDENTIAL))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "github_unreachable");
    assert!(!error.to_string().contains(CREDENTIAL));
}
