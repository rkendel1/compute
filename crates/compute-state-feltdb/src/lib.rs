//! Compute control state in Managed FeltDB.
//!
//! Managed FeltDB is the durable authority for Compute control state. This
//! adapter speaks FeltDB's application-scoped Service API only:
//!
//! - `GET  /v1/application`: discovers the active revision of the Compute
//!   application in a FeltDB environment
//! - `POST /v1/query`: reads, bounded and paginated; indexed where the model
//!   declares an index, and the plan FeltDB reports is counted in
//!   [`AccessReport`]
//! - `POST /v1/transactions`: writes, atomically, fenced on record versions
//! - `GET  /v1/state/version`: the authoritative committed revision, which
//!   proves snapshots coherent and caches current
//! - `GET  /health`: the server's own health and version
//!
//! Every query is ordered and limited by FeltDB, never by reading a
//! collection whole and trimming it here.
//!
//! The Compute application's schema is `model/compute.flow`, compiled to
//! `model/compute.manifest.json` and installed by [`provision`]. Nothing in
//! the execution engine depends on this crate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use compute_state::{
    AccessReport, BackendInfo, Collection, Comparison, ID_FIELD, Query, Record, Revision,
    StateError, StateStore, Write,
};
use serde_json::{Map, Value, json};
use tokio::sync::RwLock;

mod provision;
mod upgrade;

pub use provision::{
    ModelComparison, ModelInspection, ModelRelation, ProvisionRequest, Provisioned, compare_models,
    inspect_model, provision, provision_manifest,
};
pub use upgrade::{BackupPlan, UpgradeReport, UpgradeRequest, UpgradeStep, upgrade_control_plane};

/// `compute.flow`, the Compute control model.
pub const COMPUTE_FLOW: &str = include_str!("../model/compute.flow");
/// `compute.flow` compiled to a FeltDB application manifest.
pub const COMPUTE_MANIFEST: &str = include_str!("../model/compute.manifest.json");

/// The `@feltdb/core` release Compute is certified against: the model is
/// compiled with it (`packages/compute-state-model`), CI builds the
/// `feltdb-server` its package ships, and `scripts/feltdb/verify-version.mjs`
/// fails if anything else is resolved.
pub const CERTIFIED_FELTDB_VERSION: &str = "0.11.8";

/// FeltDB caps a query page at 1000 records, and re-executes a query for
/// every page, so pages are as large as it allows.
const PAGE: usize = 1000;

/// The identity field every Compute record carries in FeltDB (see
/// `compute.flow`): indexed, unlike `_id`.
pub const IDENTITY_FIELD: &str = "record_id";

/// Indexed lookups in flight at once for one logical read.
const CONCURRENT_LOOKUPS: usize = 32;

type IndexedFields = std::collections::BTreeMap<String, std::collections::BTreeSet<String>>;

/// The fields each collection has an equality index on, from the model.
fn indexed() -> &'static IndexedFields {
    static INDEXED: std::sync::OnceLock<IndexedFields> = std::sync::OnceLock::new();
    INDEXED.get_or_init(|| {
        let manifest: Value =
            serde_json::from_str(COMPUTE_MANIFEST).expect("the embedded manifest");
        let mut indexed = IndexedFields::new();
        for index in manifest["indexes"].as_array().into_iter().flatten() {
            if let (Some(collection), Some([field])) = (
                index["collection"].as_str(),
                index["fields"].as_array().map(Vec::as_slice),
            ) && let Some(field) = field.as_str()
            {
                indexed
                    .entry(collection.to_string())
                    .or_default()
                    .insert(field.to_string());
            }
        }
        indexed
    })
}

/// Whether FeltDB answers an equality on `field` from an index.
pub fn is_indexed(collection: Collection, field: &str) -> bool {
    indexed()
        .get(collection.name())
        .is_some_and(|fields| fields.contains(field))
}

/// Plan a query for FeltDB's planner, which uses an index only for the
/// first equality of a conjunction, and only on a declared field:
///
/// - identity (`_id`) is looked up through the indexed `record_id`;
/// - an indexed equality goes first;
/// - `In` over an indexed field, with no indexed equality beside it,
///   becomes one indexed equality per value, merged by the caller.
///
/// More than one query means merge.
pub fn plan(query: &Query) -> Vec<Query> {
    let mut query = query.clone();
    for filter in &mut query.filters {
        if filter.field == ID_FIELD {
            filter.field = IDENTITY_FIELD.into();
        }
    }
    let collection = query.collection;
    let indexed_eq = |filter: &compute_state::Filter| {
        filter.comparison == Comparison::Eq && is_indexed(collection, &filter.field)
    };
    if !query.filters.iter().any(indexed_eq)
        && let Some(position) = query.filters.iter().position(|filter| {
            filter.comparison == Comparison::In && is_indexed(collection, &filter.field)
        })
    {
        let split = query.filters.remove(position);
        let values = split
            .value
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|value| serde_json::to_string(&value).unwrap_or_default())
            .collect::<std::collections::BTreeSet<_>>();
        return values
            .into_iter()
            .map(|value| {
                let mut single = query.clone();
                single.filters.insert(
                    0,
                    compute_state::Filter {
                        field: split.field.clone(),
                        comparison: Comparison::Eq,
                        value: serde_json::from_str(&value).unwrap_or_default(),
                    },
                );
                single
            })
            .collect();
    }
    if let Some(position) = query.filters.iter().position(indexed_eq) {
        let first = query.filters.remove(position);
        query.filters.insert(0, first);
    }
    vec![query]
}

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
    /// A PEM certificate authority to trust in addition to the public
    /// roots, for a FeltDB served under a private CA. Verification is never
    /// disabled.
    pub ca_certificate: Option<Vec<u8>>,
}

/// Cheap to clone: clones share the connection, the discovered revision,
/// and the boundary counters.
#[derive(Clone)]
pub struct FeltDbState {
    http: reqwest::Client,
    config: FeltDbConfig,
    revision: std::sync::Arc<RwLock<Option<String>>>,
    transactions: std::sync::Arc<AtomicU64>,
    access: std::sync::Arc<std::sync::Mutex<AccessReport>>,
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
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
        if let Some(pem) = &config.ca_certificate {
            let certificate = reqwest::Certificate::from_pem(pem).map_err(|error| {
                StateError::Invalid(format!(
                    "the FeltDB CA certificate is not valid PEM: {error}"
                ))
            })?;
            builder = builder.add_root_certificate(certificate);
        }
        let http = builder
            .build()
            .map_err(|error| StateError::Unavailable(error.to_string()))?;
        Ok(Self {
            http,
            config: FeltDbConfig {
                url: config.url.trim_end_matches('/').to_string(),
                ..config
            },
            revision: std::sync::Arc::new(RwLock::new(None)),
            transactions: std::sync::Arc::new(AtomicU64::new(0)),
            access: std::sync::Arc::new(std::sync::Mutex::new(AccessReport {
                connection: "unknown".into(),
                certified_version: Some(CERTIFIED_FELTDB_VERSION.into()),
                ..AccessReport::default()
            })),
        })
    }

    fn account(&self, update: impl FnOnce(&mut AccessReport)) {
        update(&mut self.access.lock().expect("access"));
    }

    fn failed(&self, connection: &str, error: &str) {
        self.account(|access| {
            access.connection = connection.into();
            access.last_error = Some(error.to_string());
            access.last_error_at = Some(chrono::Utc::now());
        });
    }

    /// FeltDB's own health: status, server version, and contract. Also
    /// records the server version for diagnostics.
    pub async fn health(&self) -> Result<Value, StateError> {
        let health = self
            .send(reqwest::Method::GET, "/health", None)
            .await
            .map_err(|refusal| match refusal {
                Ok(refusal) => StateError::Unavailable(format!(
                    "FeltDB health refused ({} {}): {}",
                    refusal.status, refusal.code, refusal.message
                )),
                Err(error) => error,
            })?;
        let version = health["version"].as_str().map(str::to_owned);
        self.account(|access| access.server_version = version);
        Ok(health)
    }

    /// Connect, discover the Compute application's active revision, and
    /// check that its model is one this build can use.
    pub async fn connect(config: FeltDbConfig) -> Result<Self, StateError> {
        let state = Self::new(config)?;
        state.revision(true).await?;
        state.require_compatible_model().await?;
        Ok(state)
    }

    /// Refuse a model this build would silently misuse. An older model
    /// lacks what this build writes (FeltDB rejects unknown fields), so
    /// `compute control-plane upgrade` must run first; a divergent one is
    /// not Compute's to use. A newer model is fine: every generation only
    /// adds, so this build's writes remain valid in it.
    pub async fn require_compatible_model(&self) -> Result<(), StateError> {
        let inspection = inspect_model(&self.config).await?;
        match inspection.comparison.relation {
            ModelRelation::Current | ModelRelation::Newer => Ok(()),
            ModelRelation::Older => Err(StateError::Invalid(format!(
                "the Compute model in FeltDB (revision {}) predates this build's (generation {}): it lacks {}; run `compute control-plane upgrade` before starting this controller",
                inspection.active_revision.unwrap_or_default(),
                inspection.required_generation,
                inspection.comparison.additions.join(", ")
            ))),
            ModelRelation::Divergent => Err(StateError::Invalid(format!(
                "the Compute model in FeltDB diverges from this build's (it lacks {}; this build lacks {})",
                inspection.comparison.additions.join(", "),
                inspection.comparison.unknown.join(", ")
            ))),
            ModelRelation::Absent => Err(StateError::Unavailable(
                "no Compute model is active; run `compute control-plane provision`".into(),
            )),
        }
    }

    /// Connect, or, when FeltDB cannot be reached at all, return a state
    /// that discovers the revision on first use (and `false`). A FeltDB
    /// that answers and refuses (a wrong key, a missing application) is
    /// still an error: that is misconfiguration, not an outage.
    pub async fn connect_or_defer(config: FeltDbConfig) -> Result<(Self, bool), StateError> {
        let state = Self::new(config)?;
        match state.revision(true).await {
            Ok(_) => {
                state.require_compatible_model().await?;
                Ok((state, true))
            }
            Err(StateError::Unavailable(message)) if is_unreachable(&message) => Ok((state, false)),
            Err(error) => Err(error),
        }
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
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                let message = format!("FeltDB at {} is unreachable: {error}", self.config.url);
                self.failed("unreachable", &message);
                return Err(Err(StateError::Unavailable(message)));
            }
        };
        let status = response.status().as_u16();
        let text = match response.text().await {
            Ok(text) => text,
            Err(error) => {
                let message = format!("FeltDB response was interrupted: {error}");
                self.failed("unreachable", &message);
                return Err(Err(StateError::Unavailable(message)));
            }
        };
        let value = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
        if (200..300).contains(&status) {
            self.account(|access| access.connection = "connected".into());
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
        self.failed(
            if status == 401 || status == 403 {
                "refused"
            } else if status >= 500 {
                "unavailable"
            } else {
                "connected"
            },
            &format!("{status} {code}: {message}"),
        );
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
                Ok(refusal) if refusal.status == 401 || refusal.status == 403 => {
                    StateError::Unavailable(format!(
                        "FeltDB refused Compute's credentials for application {} ({} {}): {}",
                        self.config.application_id, refusal.status, refusal.code, refusal.message
                    ))
                }
                Ok(refusal) => StateError::Unavailable(format!(
                    "the Compute application {} is not available in FeltDB environment {} ({} {}): {}; run `compute control-plane provision`",
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

    /// One planned query: FeltDB orders (by the field, then by ID) and
    /// limits; pages are read only until the limit is reached.
    async fn run(&self, query: &Query) -> Result<Vec<Record>, StateError> {
        let mut records = vec![];
        let mut cursor = None;
        loop {
            let page = query
                .limit
                .map_or(PAGE, |limit| (limit - records.len()).min(PAGE));
            if page == 0 {
                break;
            }
            let (mut batch, next) = self.query_page(query, page, cursor.as_deref()).await?;
            records.append(&mut batch);
            if next.is_none() || query.limit.is_some_and(|limit| records.len() >= limit) {
                break;
            }
            cursor = next;
        }
        if let Some(limit) = query.limit {
            records.truncate(limit);
        }
        Ok(records)
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
                if filter.comparison == Comparison::In {
                    return json!({
                        "operator": "in",
                        "field": filter.field,
                        "values": filter.value,
                    });
                }
                json!({
                    "operator": match filter.comparison {
                        Comparison::Eq => "eq",
                        Comparison::Gt => "gt",
                        Comparison::Gte => "gte",
                        Comparison::Lt => "lt",
                        Comparison::Lte => "lte",
                        Comparison::In => unreachable!("handled above"),
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
        // Ordered by FeltDB always: by the requested field, then by
        // identity, which is also the order without one. A limit then
        // bounds what FeltDB reads and returns.
        let mut order = vec![];
        if let Some((field, descending)) = &query.order_by {
            order.push(json!({
                "field": field,
                "direction": if *descending { "desc" } else { "asc" },
            }));
        }
        order.push(json!({ "field": ID_FIELD, "direction": "asc" }));
        canonical["order_by"] = json!(order);
        if let Some(cursor) = cursor {
            canonical["cursor"] = json!(cursor);
        }
        let response = self
            .scoped("/v1/query", "query", canonical)
            .await
            .map_err(|refusal| refused(refusal, &[]))?;
        let plan = &response["plan"];
        let count = |key: &str| plan[key].as_u64().unwrap_or_default();
        self.account(|access| {
            access.queries += 1;
            access.last_read_at = Some(chrono::Utc::now());
            match plan["access_method"].as_str() {
                Some("index" | "index_lookup" | "maintained_count") => access.indexed_queries += 1,
                Some(_) => access.scanned_queries += 1,
                None => access.unplanned_queries += 1,
            }
            access.rows_scanned += count("actual_rows_scanned");
            access.rows_returned += count("actual_rows_returned");
            access.unrelated_rows_examined += count("unrelated_records_examined");
        });
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
    map.retain(|key, _| !key.starts_with('_') && key != "id" && key != IDENTITY_FIELD);
    let value: Map<String, Value> = map;
    Ok(Record { id, version, value })
}

/// A document as FeltDB stores it: with its identity as the indexed
/// `record_id` field.
fn identified(value: &Map<String, Value>, id: &str) -> Value {
    let mut value = value.clone();
    value.insert(IDENTITY_FIELD.into(), Value::String(id.into()));
    Value::Object(value)
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
        // An index lookup through `record_id`, never a scan through `_id`.
        let query = Query::all(collection).ids([id]).limit(1);
        Ok(self.query(&query).await?.into_iter().next())
    }

    async fn query(&self, query: &Query) -> Result<Vec<Record>, StateError> {
        let mut planned = plan(query);
        if planned.len() == 1 {
            return self.run(&planned.remove(0)).await;
        }
        // One indexed equality per value, a bounded number at a time; the
        // union is ordered and limited exactly as one query would be.
        let limit = std::sync::Arc::new(tokio::sync::Semaphore::new(CONCURRENT_LOOKUPS));
        let mut lookups = tokio::task::JoinSet::new();
        for single in planned {
            let limit = limit.clone();
            let this = self.clone();
            lookups.spawn(async move {
                let _permit = limit.acquire_owned().await.expect("open");
                this.run(&single).await
            });
        }
        let mut merged = vec![];
        while let Some(result) = lookups.join_next().await {
            merged.extend(result.map_err(|error| StateError::Unavailable(error.to_string()))??);
        }
        Ok(query.evaluate(merged.into_iter()))
    }

    /// FeltDB's committed state version: it advances inside the critical
    /// section that commits a transaction, so it is totally ordered. The
    /// scope names the state namespace it counts, so revisions of two
    /// stores are never compared.
    async fn revision(&self) -> Result<Option<Revision>, StateError> {
        let revision = self.revision(false).await?;
        let path = format!(
            "/v1/state/version?application_id={}&environment={}&revision_id={}",
            encode(&self.config.application_id),
            encode(&self.config.environment),
            encode(&revision)
        );
        let version = match self.send(reqwest::Method::GET, &path, None).await {
            Ok(version) => version,
            Err(Ok(refusal)) => return Err(refused(refusal, &[])),
            Err(Err(error)) => return Err(error),
        };
        self.account(|access| {
            access.revision_reads += 1;
            access.last_read_at = Some(chrono::Utc::now());
        });
        let value = version["state_version"]
            .as_u64()
            .ok_or_else(|| StateError::Invalid("FeltDB did not report a state version".into()))?;
        Ok(Some(Revision {
            value,
            scope: format!(
                "feltdb:{}:{}",
                self.config.url,
                version["state_namespace"].as_str().unwrap_or_default()
            ),
        }))
    }

    fn access(&self) -> Option<AccessReport> {
        Some(self.access.lock().expect("access").clone())
    }

    /// Give every record written before `record_id` existed its identity
    /// field, so identity lookups through the index find it. Each batch is
    /// one transaction of merge-updates fenced on the versions read; a
    /// record another writer changed meanwhile is picked up next pass.
    async fn upgrade_records(&self) -> Result<std::collections::BTreeMap<String, u64>, StateError> {
        let mut upgraded = std::collections::BTreeMap::new();
        for collection in Collection::ALL {
            let mut total = 0;
            loop {
                let response = self
                    .scoped(
                        "/v1/query",
                        "query",
                        json!({
                            "collection": collection.name(),
                            "filter": { "operator": "exists", "field": IDENTITY_FIELD, "exists": false },
                            "order_by": [{ "field": ID_FIELD, "direction": "asc" }],
                            "limit": 200,
                        }),
                    )
                    .await
                    .map_err(|refusal| refused(refusal, &[]))?;
                let records = response["records"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(record)
                    .collect::<Result<Vec<_>, _>>()?;
                if records.is_empty() {
                    break;
                }
                let writes = records
                    .iter()
                    .map(|record| Write::Update {
                        collection,
                        id: record.id.clone(),
                        fields: Map::from_iter([(
                            IDENTITY_FIELD.to_string(),
                            Value::String(record.id.clone()),
                        )]),
                        expected: Some(record.version),
                    })
                    .collect::<Vec<_>>();
                match self.commit(writes).await {
                    Ok(()) => total += records.len() as u64,
                    // Someone else wrote one of them: read again.
                    Err(error) if error.is_race() => {}
                    Err(error) => return Err(error),
                }
            }
            if total > 0 {
                upgraded.insert(collection.name().to_string(), total);
            }
        }
        Ok(upgraded)
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
                    "kind": "insert", "collection": collection.name(), "id": id, "value": identified(value, id),
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
                        "kind": "insert", "collection": collection.name(), "id": id, "value": identified(value, id),
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
                Ok(_) => {
                    self.account(|access| {
                        access.transactions += 1;
                        access.last_mutation_at = Some(chrono::Utc::now());
                    });
                    return Ok(());
                }
                Err(refusal) if refusal.status == 0 && attempts == 1 => continue,
                Err(refusal) => {
                    self.account(|access| access.failed_transactions += 1);
                    return Err(refused(refusal, &writes));
                }
            }
        }
    }
}

/// Whether an unavailability is FeltDB not answering, rather than
/// answering with a refusal.
fn is_unreachable(message: &str) -> bool {
    message.contains(" is unreachable: ") || message.starts_with("FeltDB response was interrupted")
}
