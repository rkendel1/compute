//! Operator credentials and the audit trail: durable control state, with
//! the node's cache of verifiers kept in step.

use std::io::Write;
use std::sync::Arc;

use chrono::Utc;
use compute_state::events;
use compute_state::{AuditRecord, OperatorCredentialRecord, Query, Stored, ids};
use serde_json::json;

use super::{Change, Daemon};
use crate::EnvironmentError;
use crate::auth::{
    CredentialRequest, CredentialView, IssuedCredential, Principal, RotateRequest,
    Scope as AuthScope, SecurityMode, mint,
};

/// The local audit log rolls over at this size.
const AUDIT_LOG_BYTES: u64 = 64 * 1024 * 1024;
/// Failed authentication is recorded as an event at most this often per
/// minute, so anonymous traffic cannot flood durable state.
const AUTH_EVENTS_PER_MINUTE: u32 = 60;

impl Daemon {
    /// Authenticate a request: the node's credential cache, never a round
    /// trip to control state.
    pub fn authenticate(&self, authorization: Option<&str>) -> Result<Principal, EnvironmentError> {
        self.authority.authenticate(authorization)
    }

    pub fn security_mode(&self) -> SecurityMode {
        self.authority.config.mode
    }

    /// Load credentials from control state; fall back to the node's
    /// snapshot of verifiers when control state is unreachable.
    pub(crate) async fn load_credentials(&self) -> Result<(), EnvironmentError> {
        match self.control().list::<OperatorCredentialRecord>().await {
            Ok(records) => {
                self.authority
                    .replace(records.into_iter().map(|stored| stored.value).collect());
                Ok(())
            }
            Err(error) => {
                self.authority.load_snapshot();
                Err(error.into())
            }
        }
    }

    /// In production, a node with no active admin credential issues one
    /// and writes its token, readable only by this user, to
    /// `<state_dir>/bootstrap-admin.token`. The operator creates real
    /// credentials with it and revokes it.
    pub(crate) async fn bootstrap_admin(&self) -> Result<(), EnvironmentError> {
        if self.authority.config.mode != SecurityMode::Production
            || self.authority.has_active_admin()
        {
            return Ok(());
        }
        let (record, token) = mint(
            &CredentialRequest {
                operator_id: "bootstrap-admin".into(),
                scopes: vec![AuthScope::Admin.as_str().into()],
                description: Some("issued at first production start; revoke after use".into()),
                expires_in_seconds: None,
            },
            Some("compute"),
            None,
        )?;
        let path = self.config.state_dir.join("bootstrap-admin.token");
        write_private(&path, format!("{token}\n").as_bytes())?;
        let change = Change::new()
            .with(|batch| batch.create(&ids::credential(&record.credential_id), &record));
        let change = self.event(
            change,
            events::CREDENTIAL_BOOTSTRAPPED,
            super::Scope::default(),
            format!(
                "issued bootstrap admin credential {}; its token is in {}",
                record.credential_id,
                path.display()
            ),
            json!({ "credential_id": record.credential_id }),
        );
        self.apply(change).await?;
        self.authority.upsert(record);
        Ok(())
    }

    pub async fn credentials(&self) -> Result<Vec<CredentialView>, EnvironmentError> {
        // Fresh from control state when it answers.
        let _ = self.load_credentials().await;
        let now = Utc::now();
        Ok(self
            .authority
            .records()
            .iter()
            .map(|record| CredentialView::of(record, now))
            .collect())
    }

    pub async fn create_credential(
        &self,
        principal: &Principal,
        request: CredentialRequest,
    ) -> Result<IssuedCredential, EnvironmentError> {
        let (record, token) = mint(&request, Some(&principal.operator_id), None)?;
        let change = Change::new()
            .with(|batch| batch.create(&ids::credential(&record.credential_id), &record));
        let change = self.event(
            change,
            events::CREDENTIAL_CREATED,
            super::Scope::default(),
            format!(
                "credential {} created for {} ({})",
                record.credential_id,
                record.operator_id,
                record.scopes.join(", ")
            ),
            json!({ "credential_id": record.credential_id, "operator": record.operator_id, "scopes": record.scopes }),
        );
        self.apply(change).await?;
        self.authority.upsert(record.clone());
        Ok(IssuedCredential {
            credential: CredentialView::of(&record, Utc::now()),
            token,
        })
    }

    async fn credential(
        &self,
        credential_id: &str,
    ) -> Result<Stored<OperatorCredentialRecord>, EnvironmentError> {
        self.control()
            .get::<OperatorCredentialRecord>(&ids::credential(credential_id))
            .await?
            .ok_or_else(|| EnvironmentError::NotFound(format!("credential {credential_id}")))
    }

    pub async fn revoke_credential(
        &self,
        credential_id: &str,
    ) -> Result<CredentialView, EnvironmentError> {
        let stored = self.credential(credential_id).await?;
        if stored.value.revoked_at.is_some() {
            return Ok(CredentialView::of(&stored.value, Utc::now()));
        }
        let mut record = stored.value.clone();
        record.revoked_at = Some(Utc::now());
        let change = Change::new().with(|batch| batch.replace(&stored, &record));
        let change = self.event(
            change,
            events::CREDENTIAL_REVOKED,
            super::Scope::default(),
            format!(
                "credential {credential_id} of {} revoked",
                record.operator_id
            ),
            json!({ "credential_id": credential_id, "operator": record.operator_id }),
        );
        self.apply(change).await?;
        self.authority.upsert(record.clone());
        Ok(CredentialView::of(&record, Utc::now()))
    }

    /// Issue a replacement with the same operator and scopes, and retire
    /// the old credential now or after a grace period.
    pub async fn rotate_credential(
        &self,
        principal: &Principal,
        credential_id: &str,
        request: RotateRequest,
    ) -> Result<IssuedCredential, EnvironmentError> {
        let stored = self.credential(credential_id).await?;
        if stored.value.revoked_at.is_some() {
            return Err(EnvironmentError::Conflict(format!(
                "credential {credential_id} is revoked; create a new one"
            )));
        }
        let now = Utc::now();
        let (record, token) = mint(
            &CredentialRequest {
                operator_id: stored.value.operator_id.clone(),
                scopes: stored.value.scopes.clone(),
                description: stored.value.description.clone(),
                expires_in_seconds: stored
                    .value
                    .expires_at
                    .map(|at| (at - stored.value.created_at).num_seconds().max(1) as u64),
            },
            Some(&principal.operator_id),
            Some(credential_id.to_string()),
        )?;
        let mut old = stored.value.clone();
        old.revoked_at = Some(now + chrono::TimeDelta::seconds(request.grace_seconds as i64));
        let change = Change::new()
            .with(|batch| batch.create(&ids::credential(&record.credential_id), &record))
            .with(|batch| batch.replace(&stored, &old));
        let change = self.event(
            change,
            events::CREDENTIAL_ROTATED,
            super::Scope::default(),
            format!(
                "credential {credential_id} of {} rotated to {}",
                record.operator_id, record.credential_id
            ),
            json!({
                "credential_id": record.credential_id,
                "rotated_from": credential_id,
                "grace_seconds": request.grace_seconds,
            }),
        );
        self.apply(change).await?;
        self.authority.upsert(old);
        self.authority.upsert(record.clone());
        Ok(IssuedCredential {
            credential: CredentialView::of(&record, Utc::now()),
            token,
        })
    }

    /// Record one remote operation. It is appended to the node's audit log
    /// at once and written to control state; if control state is
    /// unreachable it is written once it is.
    pub(crate) async fn audit(&self, record: AuditRecord) {
        self.append_audit_log(&record);
        let change =
            Change::new().with(|batch| batch.create(&ids::audit(&record.request_id), &record));
        if self.apply(change).await.is_err() {
            self.inner.lock().await.pending_audit.push(record);
        }
    }

    pub(crate) async fn flush_pending_audit(&self) {
        let pending = std::mem::take(&mut self.inner.lock().await.pending_audit);
        let mut failed = vec![];
        for record in pending {
            let change =
                Change::new().with(|batch| batch.create(&ids::audit(&record.request_id), &record));
            match self.apply(change).await {
                Ok(()) | Err(EnvironmentError::Conflict(_)) => {}
                Err(_) => failed.push(record),
            }
        }
        if !failed.is_empty() {
            let mut inner = self.inner.lock().await;
            failed.append(&mut inner.pending_audit);
            inner.pending_audit = failed;
        }
    }

    /// Recent audit records, newest first.
    pub async fn audit_records(
        &self,
        operator: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AuditRecord>, EnvironmentError> {
        // Ordered and limited by FeltDB; one operator's records through its
        // index.
        let mut query = Query::all(compute_state::Collection::Audit);
        if let Some(operator) = operator {
            query = query.eq("operator_id", operator.to_string());
        }
        Ok(self
            .control()
            .query::<AuditRecord>(query.descending("at").limit(limit))
            .await?
            .into_iter()
            .map(|stored| stored.value)
            .collect())
    }

    fn append_audit_log(&self, record: &AuditRecord) {
        let path = self.config.state_dir.join("audit.log");
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > AUDIT_LOG_BYTES) {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
        let Ok(mut line) = serde_json::to_vec(record) else {
            return;
        };
        line.push(b'\n');
        let file = {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&path)
        };
        if let Ok(mut file) = file {
            let _ = file.write_all(&line);
        }
    }

    /// Record a refused request as an event, rate-limited.
    pub(crate) async fn refused(
        self: &Arc<Self>,
        kind: &'static str,
        message: String,
        data: serde_json::Value,
    ) {
        {
            let mut inner = self.inner.lock().await;
            let minute = Utc::now().timestamp() / 60;
            if inner.refusals.0 != minute {
                inner.refusals = (minute, 0);
            }
            inner.refusals.1 += 1;
            if inner.refusals.1 > AUTH_EVENTS_PER_MINUTE {
                return;
            }
        }
        let change = self.event(Change::new(), kind, super::Scope::default(), message, data);
        let _ = self.apply(change).await;
    }
}

/// Write a file only this user can read.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&temporary)?.write_all(bytes)?;
    }
    std::fs::rename(&temporary, path)
}
