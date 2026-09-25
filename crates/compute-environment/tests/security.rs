//! The Compute API as a security boundary: TLS, operator credentials,
//! scopes, rotation, revocation, expiry, and the audit trail, against a
//! real daemon serving real TLS.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use compute_environment::auth::{
    CredentialRequest, CredentialView, IssuedCredential, Principal, RotateRequest, SecurityConfig,
    SecurityMode,
};
use compute_environment::client::DaemonClient;
use compute_environment::tls::ApiTls;
use compute_environment::*;

struct Node {
    daemon: Arc<Daemon>,
    endpoint: String,
    ca: Vec<u8>,
    dir: tempfile::TempDir,
    certificate: std::path::PathBuf,
    key: std::path::PathBuf,
}

fn self_signed(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf, Vec<u8>) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let certificate = dir.join("api.crt");
    let key = dir.join("api.key");
    std::fs::write(&certificate, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.signing_key.serialize_pem()).unwrap();
    (certificate, key, certified.cert.pem().into_bytes())
}

async fn production_node() -> Node {
    let dir = tempfile::tempdir().unwrap();
    let (certificate, key, ca) = self_signed(dir.path());
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(dir.path().join("node"), store, artifacts);
    config.provider = std::sync::Arc::new(common::provider());
    config.port_range = (27000, 27099);
    config.instance_port_range = (47000, 47099);
    let tls = ApiTls::load(certificate.clone(), key.clone()).unwrap();
    config.security = SecurityConfig {
        mode: SecurityMode::Production,
        reason: "test".into(),
        tls: true,
        legacy_token: None,
    };
    config.api_tls = Some(tls.clone());
    let daemon = Daemon::start(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    tokio::spawn(api::serve(listener, daemon.clone(), Some(tls)));
    Node {
        daemon,
        endpoint,
        ca,
        dir,
        certificate,
        key,
    }
}

impl Node {
    fn client(&self) -> DaemonClient {
        DaemonClient::new(&self.endpoint)
            .unwrap()
            .trusting_pem(&self.ca)
            .unwrap()
    }

    fn with(&self, token: &str) -> DaemonClient {
        self.client().with_bearer_token(token)
    }

    fn bootstrap(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("node/bootstrap-admin.token"))
            .unwrap()
            .trim()
            .to_string()
    }

    async fn issue(
        &self,
        admin: &str,
        operator: &str,
        scopes: &[&str],
        expires: Option<u64>,
    ) -> IssuedCredential {
        self.with(admin)
            .post(
                "/auth/credentials",
                Some(&CredentialRequest {
                    operator_id: operator.into(),
                    scopes: scopes.iter().map(|scope| scope.to_string()).collect(),
                    description: None,
                    expires_in_seconds: expires,
                }),
            )
            .await
            .unwrap()
    }
}

fn environment(name: &str) -> EnvironmentDefinition {
    EnvironmentDefinition {
        name: name.into(),
        desired_state: DesiredState::Running,
        env: BTreeMap::new(),
        policy: None,
        provider: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_requires_tls_and_a_credential_for_every_request() {
    let node = production_node().await;
    // Plaintext to the TLS listener gets nothing.
    let plaintext = DaemonClient::new(&node.endpoint.replace("https://", "http://")).unwrap();
    assert!(plaintext.get::<serde_json::Value>("/health").await.is_err());
    // A client that does not trust the certificate refuses it.
    let untrusting = DaemonClient::new(&node.endpoint).unwrap();
    assert!(matches!(
        untrusting.get::<serde_json::Value>("/health").await,
        Err(EnvironmentError::ControllerUnavailable(_))
    ));
    // Liveness needs no credential; nothing else is readable without one.
    let health: serde_json::Value = node.client().get("/health").await.unwrap();
    assert_eq!(health["status"], "ok");
    for path in [
        "/status",
        "/environments",
        "/info",
        "/events",
        "/deployments",
    ] {
        assert!(
            matches!(
                node.client().get::<serde_json::Value>(path).await,
                Err(EnvironmentError::Unauthorized(_))
            ),
            "{path} is readable without a credential"
        );
    }
    // Invalid credentials are refused the same way.
    for token in ["nonsense", "cmpt_cred_0000000000000000_deadbeef"] {
        assert!(matches!(
            node.with(token).get::<serde_json::Value>("/status").await,
            Err(EnvironmentError::Unauthorized(_))
        ));
    }
    // The bootstrap admin token is private to this user.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(node.dir.path().join("node/bootstrap-admin.token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }
    let admin = node.bootstrap();
    let info: ControllerInfo = node.with(&admin).get("/info").await.unwrap();
    assert_eq!(info.security.mode, SecurityMode::Production);
    assert!(info.security.authentication_required);
    assert!(info.security.tls.enabled);
    assert!(!info.controller.build_id.is_empty());
    assert!(!info.controller.git_commit.is_empty());
    node.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scopes_are_enforced_and_every_mutation_is_audited() {
    let node = production_node().await;
    let admin = node.bootstrap();
    let reader = node
        .issue(&admin, "auditor-7", &["compute.read"], None)
        .await;
    let operator = node
        .issue(
            &admin,
            "developer-42",
            &["compute.read", "compute.operate"],
            None,
        )
        .await;
    assert!(reader.token.starts_with("cmpt_"));

    // A reader reads, but may not change anything.
    let _: Vec<EnvironmentSummary> = node.with(&reader.token).get("/environments").await.unwrap();
    let denied = node
        .with(&reader.token)
        .post::<_, EnvironmentView>("/environments", Some(&environment("prod")))
        .await
        .unwrap_err();
    assert_eq!(denied.kind(), "authorization_denied");
    assert!(denied.message().contains("compute.operate"));
    // Nor manage credentials or read the audit trail.
    assert_eq!(
        node.with(&operator.token)
            .get::<Vec<CredentialView>>("/auth/credentials")
            .await
            .unwrap_err()
            .kind(),
        "authorization_denied"
    );
    // A scoped operation is accepted.
    let created: EnvironmentView = node
        .with(&operator.token)
        .post("/environments", Some(&environment("prod")))
        .await
        .unwrap();
    assert_eq!(created.name, "prod");
    let whoami: Principal = node
        .with(&operator.token)
        .get("/auth/whoami")
        .await
        .unwrap();
    assert_eq!(whoami.operator_id, "developer-42");

    // The audit trail: who, what, which credential, what happened.
    let audit: Vec<compute_state::AuditRecord> =
        node.with(&admin).get("/audit?limit=100").await.unwrap();
    let accepted = audit
        .iter()
        .find(|record| record.operation == "POST /environments" && record.result == "accepted")
        .expect("the accepted creation is audited");
    assert_eq!(accepted.operator_id, "developer-42");
    assert_eq!(
        accepted.credential_id.as_deref(),
        Some(operator.credential.credential_id.as_str())
    );
    assert!(accepted.request_id.starts_with("req_"));
    assert_eq!(accepted.resource, "environment");
    let rejected = audit
        .iter()
        .find(|record| record.operator_id == "auditor-7" && record.result == "rejected")
        .expect("the refused attempt is audited");
    assert_eq!(rejected.error_kind.as_deref(), Some("authorization_denied"));
    // Credential creation is audited without its token.
    let issued = audit
        .iter()
        .find(|record| record.operation == "POST /auth/credentials")
        .unwrap();
    assert!(issued.detail.contains_key("credential_id"));

    // The event names who asked for it.
    let events: Vec<compute_state::EventRecord> =
        node.with(&admin).get("/events?limit=200").await.unwrap();
    let event = events
        .iter()
        .find(|event| event.kind == compute_state::events::ENVIRONMENT_CREATED)
        .unwrap();
    assert_eq!(event.data["operator_id"], "developer-42");
    assert!(
        event.data["request_id"]
            .as_str()
            .unwrap()
            .starts_with("req_")
    );
    let denials = events
        .iter()
        .filter(|event| event.kind == compute_state::events::AUTHORIZATION_DENIED)
        .count();
    assert!(denials >= 1);

    // No secret anywhere: not in events, audit records, the local audit
    // log, or the credential snapshot.
    let everything = format!(
        "{}{}{}{}",
        serde_json::to_string(&events).unwrap(),
        serde_json::to_string(&audit).unwrap(),
        std::fs::read_to_string(node.dir.path().join("node/audit.log")).unwrap(),
        std::fs::read_to_string(node.dir.path().join("node/credentials.json")).unwrap(),
    );
    for token in [&admin, &reader.token, &operator.token] {
        let secret = token.rsplit_once('_').unwrap().1;
        assert!(!everything.contains(secret), "a secret leaked");
    }
    let listed: Vec<CredentialView> = node.with(&admin).get("/auth/credentials").await.unwrap();
    assert!(!serde_json::to_string(&listed).unwrap().contains("verifier"));
    node.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credentials_expire_revoke_and_rotate() {
    let node = production_node().await;
    let admin = node.bootstrap();
    // Expired.
    let brief = node.issue(&admin, "temp", &["compute.read"], Some(1)).await;
    let _: DaemonStatus = node.with(&brief.token).get("/status").await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let expired = node
        .with(&brief.token)
        .get::<DaemonStatus>("/status")
        .await
        .unwrap_err();
    assert_eq!(expired.kind(), "authentication_failed");
    assert!(expired.message().contains("expired"));

    // Revoked.
    let revocable = node.issue(&admin, "leaver", &["compute.read"], None).await;
    let _: DaemonStatus = node.with(&revocable.token).get("/status").await.unwrap();
    let view: CredentialView = node
        .with(&admin)
        .post::<(), _>(
            &format!(
                "/auth/credentials/{}/revoke",
                revocable.credential.credential_id
            ),
            None,
        )
        .await
        .unwrap();
    assert_eq!(view.status, "revoked");
    let revoked = node
        .with(&revocable.token)
        .get::<DaemonStatus>("/status")
        .await
        .unwrap_err();
    assert!(revoked.message().contains("revoked"));

    // Rotated: the new token works, the old one stops.
    let rotating = node
        .issue(&admin, "ci", &["compute.deploy", "compute.read"], None)
        .await;
    let rotated: IssuedCredential = node
        .with(&admin)
        .post(
            &format!(
                "/auth/credentials/{}/rotate",
                rotating.credential.credential_id
            ),
            Some(&RotateRequest { grace_seconds: 0 }),
        )
        .await
        .unwrap();
    assert_ne!(rotated.token, rotating.token);
    assert_eq!(rotated.credential.operator_id, "ci");
    assert_eq!(rotated.credential.scopes, rotating.credential.scopes);
    assert_eq!(
        rotated.credential.rotated_from.as_deref(),
        Some(rotating.credential.credential_id.as_str())
    );
    let _: DaemonStatus = node.with(&rotated.token).get("/status").await.unwrap();
    assert!(
        node.with(&rotating.token)
            .get::<DaemonStatus>("/status")
            .await
            .is_err()
    );

    // With a grace period, both work until it ends.
    let graceful: IssuedCredential = node
        .with(&admin)
        .post(
            &format!(
                "/auth/credentials/{}/rotate",
                rotated.credential.credential_id
            ),
            Some(&RotateRequest { grace_seconds: 60 }),
        )
        .await
        .unwrap();
    let _: DaemonStatus = node.with(&rotated.token).get("/status").await.unwrap();
    let _: DaemonStatus = node.with(&graceful.token).get("/status").await.unwrap();
    node.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_certificate_reloads_without_a_restart() {
    let node = production_node().await;
    let admin = node.bootstrap();
    let before: ControllerInfo = node.with(&admin).get("/info").await.unwrap();
    // Replace the pair on disk.
    let replacement = tempfile::tempdir().unwrap();
    let (certificate, key, ca) = self_signed(replacement.path());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    std::fs::copy(&key, &node.key).unwrap();
    std::fs::copy(&certificate, &node.certificate).unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let client = DaemonClient::new(&node.endpoint)
        .unwrap()
        .trusting_pem(&ca)
        .unwrap()
        .with_bearer_token(&admin);
    let after: ControllerInfo = client.get("/info").await.unwrap();
    assert_ne!(
        before.security.tls.fingerprint,
        after.security.tls.fingerprint
    );
    assert_eq!(after.security.tls.reloads, 1);
    assert_eq!(before.instance_id, after.instance_id, "the same controller");
    // The old certificate is no longer served.
    assert!(
        node.with(&admin)
            .get::<ControllerInfo>("/info")
            .await
            .is_err()
    );
    node.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn development_mode_is_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(dir.path(), store, artifacts);
    config.provider = std::sync::Arc::new(common::provider());
    config.port_range = (27100, 27199);
    config.instance_port_range = (47100, 47199);
    let daemon = Daemon::start(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(api::serve(listener, daemon.clone(), None));
    let client = DaemonClient::new(&endpoint).unwrap();
    let whoami: Principal = client.get("/auth/whoami").await.unwrap();
    assert!(whoami.development);
    assert_eq!(whoami.operator_id, "development");
    let info: ControllerInfo = client.get("/info").await.unwrap();
    assert_eq!(info.security.mode, SecurityMode::Development);
    assert!(!info.security.authentication_required);
    assert!(!info.security.tls.enabled);
    // A credential presented in development mode is still checked.
    assert!(
        client
            .clone()
            .with_bearer_token("cmpt_cred_0000000000000000_00")
            .get::<DaemonStatus>("/status")
            .await
            .is_err()
    );
    // Production refuses a shared token outright.
    let other = tempfile::tempdir().unwrap();
    let store = Arc::new(compute_state_memory::MemoryState::new());
    let artifacts = Arc::new(compute_state::StateArtifacts::new(
        compute_state::ControlState::new(store.clone()),
    ));
    let mut config = DaemonConfig::new(other.path(), store, artifacts);
    config.provider = std::sync::Arc::new(common::provider());
    config.security = SecurityConfig {
        mode: SecurityMode::Production,
        reason: "test".into(),
        tls: true,
        legacy_token: Some("shared".into()),
    };
    assert!(Daemon::start(config).await.is_err());
    daemon.shutdown().await;
}
