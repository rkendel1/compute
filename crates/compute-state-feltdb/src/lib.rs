//! Compute control state in Managed FeltDB.
//!
//! Managed FeltDB is the durable authority for Compute control state. This
//! adapter speaks FeltDB's application-scoped Service API only:
//!
//! - `GET  /v1/application`  discovers the active revision of the Compute
//!   application in a FeltDB environment
//! - `POST /v1/query`        reads (bounded, paginated)
//! - `POST /v1/transactions` writes, atomically, fenced on record versions
//!
//! The Compute application's schema is `model/compute.flow`, compiled to
//! `model/compute.manifest.json` and installed by [`provision`]. Nothing in
//! the execution engine depends on this crate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use compute_state::{
    BackendInfo, Collection, Comparison, Query, Record, StateError, StateStore, Write,
};
use serde_json::{Map, Value, json};
use tokio::sync::RwLock;

mod provision;

pub use provision::{ProvisionRequest, Provisioned, provision, provision_manifest};

/// `compute.flow`, the Compute control model.
pub const COMPUTE_FLOW: &str = include_str!("../model/compute.flow");
/// `compute.flow` compiled to a FeltDB application manifest.
pub const COMPUTE_MANIFEST: &str = include_str!("../model/compute.manifest.json");

const PAGE: usize = 200;

#[derive(Debug, Clone)]
pub struct FeltDbConfig {
    /// The FeltDB authority, such as `https://feltdb.example.com`.
    pub url: String,
    /// An application-scoped FeltDB API key.
    pub token: String,
    /// The Compute application in FeltDB (from [`provision`]).
    pub application_id: String,
    /// The FeltDB environment that holds this control plane's state.
    pub environment: String,
}

pub struct FeltDbState {
    http: reqwest::Client,
    config: FeltDbConfig,
    revision: RwLock<Option<String>>,
    transactions: AtomicU64,
}

/// A refusal from FeltDB, with its error code.
#[derive(Debug)]
struct Refusal {
    status: u16,
    code: String,
    message: String,
    body: Value,
}

impl FeltDbState {
    pub fn new(config: FeltDbConfig) -> Result<Self, StateError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| StateError::Unavailable(error.to_string()))?;
        Ok(Self {
            http,
            config: FeltDbConfig {
                url: config.url.trim_end_matches('/').to_string(),
                ..config
            },
            revision: RwLock::new(None),
            transactions: AtomicU64::new(0),
        })
    }

    /// Connect and discover the Compute application's active revision.
    pub async fn connect(config: FeltDbConfig) -> Result<Self, StateError> {
        let state = Self::new(config)?;
        state.revision(true).await?;
        Ok(state)
    }

    pub fn config(&self) -> &FeltDbConfig {
        &self.config
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, Result<Refusal, StateError>> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.config.url))
            .bearer_auth(&self.config.token)
            .header("FeltDB-Protocol", "1");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|error| {
            Err(StateError::Unavailable(format!(
                "FeltDB at {} is unreachable: {error}",
                self.config.url
            )))
        })?;
        let status = response.status().as_u16();
        let text = response.text().await.map_err(|error| {
            Err(StateError::Unavailable(format!(
                "FeltDB response was interrupted: {error}"
            )))
        })?;
        let value = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
        if (200..300).contains(&status) {
            return Ok(value);
        }
        let code = value["code"]
            .as_str()
            .or_else(|| value["error"].as_str())
            .unwrap_or("REQUEST_FAILED")
            .to_string();
        let message = value["message"]
            .as_str()
            .or_else(|| value["error"].as_str())
            .or_else(|| value.as_str())
            .unwrap_or("FeltDB refused the request")
            .to_string();
        Err(Ok(Refusal {
            status,
            code,
            message,
            body: value,
        }))
    }

    async fn revision(&self, refresh: bool) -> Result<String, StateError> {
        if !refresh && let Some(revision) = self.revision.read().await.clone() {
            return Ok(revision);
        }
        let path = format!(
            "/v1/application?application_id={}&environment={}",
            encode(&self.config.application_id),
            encode(&self.config.environment)
        );
        let application = self
            .send(reqwest::Method::GET, &path, None)
            .await
            .map_err(|refusal| match refusal {
                Ok(refusal) => StateError::Unavailable(format!(
                    "the Compute application {} is not available in FeltDB environment {} ({} {}): {}; run `compute state provision`",
                    self.config.application_id,
                    self.config.environment,
                    refusal.status,
                    refusal.code,
                    refusal.message
                )),
                Err(error) => error,
            })?;
        let revision = application["revision_id"]
            .as_str()
            .ok_or_else(|| {
                StateError::Unavailable("FeltDB did not report an active revision".into())
            })?
            .to_string();
        if application["application_name"].as_str() != Some("compute") {
            return Err(StateError::Invalid(format!(
                "FeltDB application {} is not a Compute control plane",
                self.config.application_id
            )));
        }
        *self.revision.write().await = Some(revision.clone());
        Ok(revision)
    }

    /// POST an application-scoped request, rediscovering the revision once
    /// if the schema moved underneath us.
    async fn scoped(&self, path: &str, key: &str, payload: Value) -> Result<Value, Refusal> {
        let mut refreshed = false;
        loop {
            let revision = self.revision(false).await.map_err(unavailable)?;
            let body = json!({
                "application_id": self.config.application_id,
                "environment": self.config.environment,
                "revision_id": revision,
                key: payload,
            });
            match self.send(reqwest::Method::POST, path, Some(&body)).await {
                Ok(value) => return Ok(value),
                Err(Ok(refusal))
                    if !refreshed
                        && matches!(
                            refusal.code.as_str(),
                            "SCHEMA_MISMATCH" | "UNKNOWN_FIELD" | "UNKNOWN_COLLECTION"
                        ) =>
                {
                    refreshed = true;
                    self.revision(true).await.map_err(unavailable)?;
                }
                Err(Ok(refusal)) => return Err(refusal),
                Err(Err(error)) => return Err(unavailable(error)),
            }
        }
    }

    async fn query_page(
        &self,
        query: &Query,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<Record>, Option<String>), StateError> {
        let mut canonical = json!({
            "collection": query.collection.name(),
            "limit": limit,
        });
        let filters = query
            .filters
            .iter()
            .map(|filter| {
                json!({
                    "operator": match filter.comparison {
                        Comparison::Eq => "eq",
                        Comparison::Gt => "gt",
                        Comparison::Gte => "gte",
                        Comparison::Lt => "lt",
                        Comparison::Lte => "lte",
                    },
                    "field": filter.field,
                    "value": filter.value,
                })
            })
            .collect::<Vec<_>>();
        match filters.len() {
            0 => {}
            1 => canonical["filter"] = filters.into_iter().next().expect("one"),
            _ => canonical["filter"] = json!({ "operator": "and", "filters": filters }),
        }
        if let Some((field, descending)) = &query.order_by {
            canonical["order_by"] = json!([{
                "field": field,
                "direction": if *descending { "desc" } else { "asc" },
            }]);
        }
        if let Some(cursor) = cursor {
            canonical["cursor"] = json!(cursor);
        }
        let response = self
            .scoped("/v1/query", "query", canonical)
            .await
            .map_err(|refusal| refused(refusal, &[]))?;
        let records = response["records"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(record)
            .collect::<Result<Vec<_>, _>>()?;
        let next = response["next_cursor"].as_str().map(str::to_owned);
        Ok((records, next))
    }
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn unavailable(error: StateError) -> Refusal {
    Refusal {
        status: 0,
        code: "STATE_UNAVAILABLE".into(),
        message: error.to_string(),
        body: Value::Null,
    }
}

/// A FeltDB query record: the document plus `_id` and `_version`, the
/// storage version that transaction fences compare.
fn record(value: Value) -> Result<Record, StateError> {
    let Value::Object(mut map) = value else {
        return Err(StateError::Invalid(
            "FeltDB returned a non-object record".into(),
        ));
    };
    let id = map
        .get("_id")
        .or_else(|| map.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| StateError::Invalid("FeltDB record has no identity".into()))?
        .to_string();
    let version = map
        .get("_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| StateError::Invalid(format!("FeltDB record {id} has no version")))?;
    // Compute fields never begin with `_`; FeltDB's metadata does.
    map.retain(|key, _| !key.starts_with('_') && key != "id");
    let value: Map<String, Value> = map;
    Ok(Record { id, version, value })
}

/// Map a FeltDB refusal onto the state semantics.
fn refused(refusal: Refusal, writes: &[Write]) -> StateError {
    let resource = refusal.body["resource"].as_str().unwrap_or_default();
    let mut parts = resource.rsplitn(2, ':');
    let id = parts.next().unwrap_or_default().to_string();
    let collection = parts
        .next()
        .and_then(|rest| rest.rsplit(':').next())
        .and_then(Collection::from_name);
    let located = |kind: fn(&Write) -> bool| {
        collection
            .map(|collection| (collection, id.clone()))
            .filter(|(_, id)| !id.is_empty())
            .or_else(|| {
                writes
                    .iter()
                    .find(|write| kind(write))
                    .map(|write| (write.collection(), write.id().to_string()))
            })
    };
    match refusal.code.as_str() {
        "CONFLICT" => match located(|write| matches!(write, Write::Create { .. })) {
            Some((collection, id)) => StateError::Conflict { collection, id },
            None => StateError::Invalid(refusal.message),
        },
        "PRECONDITION_FAILED" => match located(|write| !matches!(write, Write::Create { .. })) {
            Some((collection, id)) => StateError::Precondition {
                collection,
                id,
                expected: refusal.body["expected"].as_u64().unwrap_or_default(),
                actual: refusal.body["actual"].as_u64(),
            },
            None => StateError::Invalid(refusal.message),
        },
        "NOT_FOUND" | "RECORD_NOT_FOUND" => {
            match located(|write| !matches!(write, Write::Create { .. })) {
                Some((collection, id)) => StateError::NotFound { collection, id },
                None => StateError::Invalid(refusal.message),
            }
        }
        "STATE_UNAVAILABLE" | "STORAGE_FAILURE" => StateError::Unavailable(refusal.message),
        _ if refusal.status == 401 || refusal.status == 403 => StateError::Unavailable(format!(
            "FeltDB refused Compute's credentials ({}): {}",
            refusal.code, refusal.message
        )),
        _ if refusal.status >= 500 || refusal.status == 0 => {
            StateError::Unavailable(format!("{}: {}", refusal.code, refusal.message))
        }
        _ => StateError::Invalid(format!("{}: {}", refusal.code, refusal.message)),
    }
}

#[async_trait]
impl StateStore for FeltDbState {
    fn backend(&self) -> BackendInfo {
        BackendInfo {
            kind: "feltdb".into(),
            location: format!(
                "{} application {} ({})",
                self.config.url, self.config.application_id, self.config.environment
            ),
            durable: true,
        }
    }

    async fn get(&self, collection: Collection, id: &str) -> Result<Option<Record>, StateError> {
        // `_id` is FeltDB's record identity; it is filterable and indexed.
        let query = Query::all(collection).eq("_id", id).limit(1);
        Ok(self.query_page(&query, 1, None).await?.0.into_iter().next())
    }

    async fn query(&self, query: &Query) -> Result<Vec<Record>, StateError> {
        // Without an ordering, results are ordered by ID: read every page,
        // then order and limit, exactly as the in-memory semantics do.
        let server_limit = query.order_by.as_ref().and(query.limit);
        let mut records = vec![];
        let mut cursor = None;
        loop {
            let page = server_limit.map_or(PAGE, |limit| (limit - records.len()).min(PAGE));
            let (mut batch, next) = self.query_page(query, page, cursor.as_deref()).await?;
            records.append(&mut batch);
            if next.is_none() || server_limit.is_some_and(|limit| records.len() >= limit) {
                break;
            }
            cursor = next;
        }
        if query.order_by.is_none() {
            records.sort_by(|left, right| left.id.cmp(&right.id));
            if let Some(limit) = query.limit {
                records.truncate(limit);
            }
        }
        Ok(records)
    }

    async fn commit(&self, writes: Vec<Write>) -> Result<(), StateError> {
        let mut operations = vec![];
        for write in &writes {
            match write {
                Write::Create {
                    collection,
                    id,
                    value,
                } => operations.push(json!({
                    "kind": "insert", "collection": collection.name(), "id": id, "value": value,
                })),
                // FeltDB updates merge fields; Compute replaces documents.
                // A fenced delete and an insert in one transaction is an
                // atomic replace.
                Write::Replace {
                    collection,
                    id,
                    value,
                    expected,
                } => {
                    operations.push(json!({
                        "kind": "delete", "collection": collection.name(), "id": id, "if_version": expected,
                    }));
                    operations.push(json!({
                        "kind": "insert", "collection": collection.name(), "id": id, "value": value,
                    }));
                }
                // FeltDB's native update merges top-level fields.
                Write::Update {
                    collection,
                    id,
                    fields,
                    expected,
                } => operations.push(json!({
                    "kind": "update", "collection": collection.name(), "id": id, "value": fields, "if_version": expected,
                })),
                Write::Delete {
                    collection,
                    id,
                    expected,
                } => operations.push(json!({
                    "kind": "delete", "collection": collection.name(), "id": id, "if_version": expected,
                })),
            }
        }
        let transaction_id = format!(
            "compute-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default(),
            self.transactions.fetch_add(1, Ordering::Relaxed)
        );
        // The server fills tenant, revision, schema, and authorization from
        // the authenticated scope; empty values defer to it.
        let transaction = json!({
            "transaction_id": transaction_id,
            "tenant_id": "",
            "application_id": "",
            "revision_id": "",
            "schema_version": 0,
            "authorization": {
                "subject": "", "tenant_id": "", "application_id": "", "revision_id": "", "capabilities": [],
            },
            "operations": operations,
        });
        // A transport failure is retried once with the same transaction ID;
        // FeltDB answers a replayed ID with the original result.
        let mut attempts = 0;
        loop {
            attempts += 1;
            match self
                .scoped("/v1/transactions", "transaction", transaction.clone())
                .await
            {
                Ok(_) => return Ok(()),
                Err(refusal) if refusal.status == 0 && attempts == 1 => continue,
                Err(refusal) => return Err(refused(refusal, &writes)),
            }
        }
    }
}
