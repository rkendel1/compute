//! Compute control state in Managed FeltDB.
//!
//! Managed FeltDB is the durable authority for Compute control state. This
//! adapter reaches FeltDB only through the native client's named methods
//! (`feltdb-client`), whose request and response documents are FeltDB's own
//! types; it builds no route or request body by hand:
//!
//! - `Application::discover` (`GET /v1/application`): the active revision of
//!   the Compute application in a FeltDB environment
//! - `Application::query` (`POST /v1/query`): reads, bounded and paginated;
//!   indexed where the model declares an index, and the plan FeltDB reports
//!   is counted in [`AccessReport`]
//! - `Application::commit` (`POST /v1/transactions`): writes, atomically,
//!   each fenced on the record version it was read at
//! - `Application::state_version` (`GET /v1/state/version`): the
//!   authoritative committed revision, which proves snapshots coherent and
//!   caches current
//! - `Client::health` (`GET /health`): the server's own health and version
//! - `Client::control` (`/api/…`, `/admin/backups`): provisioning, model
//!   upgrades, and backup ([`provision`], [`upgrade`])
//!
//! A private certificate authority (`[state.feltdb] ca_file`) is added to the
//! public roots; verification is never disabled.
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
use feltdb_client::AppTransaction;
use feltdb_client::wire::{CanonicalQuery, QueryFilter, QueryOrder, QueryResult, SortDirection};
use serde_json::{Map, Value};
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

/// The FeltDB release Compute is certified against: the `feltdb-client` this
/// crate is built with (pinned exactly; a test fails if `Cargo.lock`
/// resolves another), and the crates.io `feltdb-server` CI runs every
/// FeltDB test against.
pub const CERTIFIED_FELTDB_VERSION: &str = "0.2.1";

/// The `@feltdb/core` release that compiles `compute.flow` to the checked-in
/// manifest (`packages/compute-state-model`); a development-time tool, not
/// part of the runtime. `scripts/feltdb/verify-version.mjs` fails if npm
/// resolves anything else.
pub const MODEL_COMPILER_VERSION: &str = "0.11.9";

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
    client: feltdb_client::Client,
    /// The Compute application in its environment: discovery and queries.
    application: feltdb_client::Application,
    config: FeltDbConfig,
    revision: std::sync::Arc<RwLock<Option<String>>>,
    transactions: std::sync::Arc<AtomicU64>,
    access: std::sync::Arc<std::sync::Mutex<AccessReport>>,
    transitions: std::sync::Arc<compute_state::Transitions>,
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
        // A private CA is added to the public roots, never in place of
        // verification; one that is not a PEM certificate is refused here,
        // not at the first request.
        let mut builder = feltdb_client::Client::builder(&config.url)
            .token(config.token.clone())
            .timeout(Duration::from_secs(30));
        if let Some(ca) = &config.ca_certificate {
            builder = builder.add_root_certificate_pem(ca.clone());
        }
        let client = builder
            .build()
            .map_err(|error| StateError::Invalid(format!("FeltDB {}: {error}", config.url)))?;
        let application = client
            .application(config.application_id.clone())
            .environment(config.environment.clone());
        Ok(Self {
            client,
            application,
            config: FeltDbConfig {
                url: config.url.trim_end_matches('/').to_string(),
                ..config
            },
            revision: std::sync::Arc::new(RwLock::new(None)),
            transitions: std::sync::Arc::default(),
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
    pub async fn health(&self) -> Result<feltdb_client::Health, StateError> {
        let health = self
            .client
            .health()
            .await
            .map_err(|error| match self.refusal(error) {
                Ok(refusal) => StateError::Unavailable(format!(
                    "FeltDB health refused ({} {}): {}",
                    refusal.status, refusal.code, refusal.message
                )),
                Err(error) => error,
            })?;
        self.account(|access| {
            access.connection = "connected".into();
            access.server_version = Some(health.version.clone());
        });
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

    /// Account a committed transaction and its state versions, when read.
    fn committed(&self, transition: Option<(u64, u64)>, writes: &[Write]) -> Option<(u64, u64)> {
        self.account(|access| {
            access.transactions += 1;
            access.last_mutation_at = Some(chrono::Utc::now());
        });
        self.transitions.record(transition, writes);
        transition
    }

    /// A client error as a refusal (FeltDB answered and refused) or an
    /// unavailability (it did not answer, or answered unreadably), recorded
    /// for diagnostics.
    fn refusal(&self, error: feltdb_client::Error) -> Result<Refusal, StateError> {
        let feltdb_client::Error::Api(api) = error else {
            let message = match &error {
                feltdb_client::Error::Transport { .. } => {
                    format!("FeltDB at {} is unreachable: {error}", self.config.url)
                }
                _ => format!("FeltDB at {} answered unreadably: {error}", self.config.url),
            };
            self.failed("unreachable", &message);
            return Err(StateError::Unavailable(message));
        };
        let api = *api;
        // The code as FeltDB reported it; one it did not report is never
        // inferred from the status.
        let code = api.body["code"]
            .as_str()
            .or_else(|| api.body["error"].as_str())
            .unwrap_or("REQUEST_FAILED")
            .to_string();
        self.failed(
            if api.status == 401 || api.status == 403 {
                "refused"
            } else if api.status >= 500 {
                "unavailable"
            } else {
                "connected"
            },
            &format!("{} {code}: {}", api.status, api.message),
        );
        Ok(Refusal {
            status: api.status,
            code,
            message: api.message,
            body: api.body,
        })
    }

    async fn revision(&self, refresh: bool) -> Result<String, StateError> {
        if !refresh && let Some(revision) = self.revision.read().await.clone() {
            return Ok(revision);
        }
        let application = match self.application.discover().await {
            Ok(application) => {
                self.account(|access| access.connection = "connected".into());
                application
            }
            Err(error) => {
                return Err(match self.refusal(error) {
                    Ok(refusal) if refusal.status == 401 || refusal.status == 403 => {
                        StateError::Unavailable(format!(
                            "FeltDB refused Compute's credentials for application {} ({} {}): {}",
                            self.config.application_id,
                            refusal.status,
                            refusal.code,
                            refusal.message
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
                });
            }
        };
        if application.application_name.as_deref() != Some("compute") {
            return Err(StateError::Invalid(format!(
                "FeltDB application {} is not a Compute control plane",
                self.config.application_id
            )));
        }
        let revision = application.revision_id;
        *self.revision.write().await = Some(revision.clone());
        Ok(revision)
    }

    /// Whether a refusal means the active model moved underneath us.
    fn schema_moved(refusal: &Refusal) -> bool {
        matches!(
            refusal.code.as_str(),
            "SCHEMA_MISMATCH" | "UNKNOWN_FIELD" | "UNKNOWN_COLLECTION"
        )
    }

    /// A canonical query, rediscovering the revision once if the schema
    /// moved underneath us.
    async fn canonical_query(&self, query: &CanonicalQuery) -> Result<QueryResult, Refusal> {
        let mut refreshed = false;
        loop {
            // Discovered here, with its checks, before the client uses it.
            self.revision(false).await.map_err(unavailable)?;
            match self.application.query(query).await {
                Ok(result) => {
                    self.account(|access| access.connection = "connected".into());
                    return Ok(result);
                }
                Err(error) => match self.refusal(error) {
                    Ok(refusal) if !refreshed && Self::schema_moved(&refusal) => {
                        refreshed = true;
                        self.revision(true).await.map_err(unavailable)?;
                    }
                    Ok(refusal) => return Err(refusal),
                    Err(error) => return Err(unavailable(error)),
                },
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

    /// Records of `collection` written without `record_id`, at most `limit`.
    async fn without_identity(
        &self,
        collection: Collection,
        limit: usize,
    ) -> Result<Vec<Record>, StateError> {
        let query = canonical(
            collection,
            Some(QueryFilter::Exists {
                field: IDENTITY_FIELD.into(),
                exists: false,
            }),
            vec![QueryOrder {
                field: ID_FIELD.into(),
                direction: SortDirection::Asc,
            }],
            limit,
            None,
        );
        let response = self
            .canonical_query(&query)
            .await
            .map_err(|refusal| refused(refusal, &[]))?;
        response.records.into_iter().map(record).collect()
    }

    async fn query_page(
        &self,
        query: &Query,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<Record>, Option<String>), StateError> {
        let mut filters = query
            .filters
            .iter()
            .map(|filter| {
                let field = filter.field.clone();
                let value = filter.value.clone();
                Ok(match filter.comparison {
                    Comparison::Eq => QueryFilter::Eq { field, value },
                    Comparison::Gt => QueryFilter::Gt { field, value },
                    Comparison::Gte => QueryFilter::Gte { field, value },
                    Comparison::Lt => QueryFilter::Lt { field, value },
                    Comparison::Lte => QueryFilter::Lte { field, value },
                    Comparison::In => QueryFilter::In {
                        values: value.as_array().cloned().ok_or_else(|| {
                            StateError::Invalid(format!("`in` on {field} needs a list of values"))
                        })?,
                        field,
                    },
                })
            })
            .collect::<Result<Vec<_>, StateError>>()?;
        let filter = match filters.len() {
            0 => None,
            1 => filters.pop(),
            _ => Some(QueryFilter::And { filters }),
        };
        // Ordered by FeltDB always: by the requested field, then by
        // identity, which is also the order without one. A limit then
        // bounds what FeltDB reads and returns.
        let mut order = vec![];
        if let Some((field, descending)) = &query.order_by {
            order.push(QueryOrder {
                field: field.clone(),
                direction: if *descending {
                    SortDirection::Desc
                } else {
                    SortDirection::Asc
                },
            });
        }
        order.push(QueryOrder {
            field: ID_FIELD.into(),
            direction: SortDirection::Asc,
        });
        let canonical = canonical(
            query.collection,
            filter,
            order,
            limit,
            cursor.map(str::to_owned),
        );
        let response = self
            .canonical_query(&canonical)
            .await
            .map_err(|refusal| refused(refusal, &[]))?;
        let plan = &response.plan;
        let count = |value: Option<usize>| value.unwrap_or_default() as u64;
        self.account(|access| {
            access.queries += 1;
            access.last_read_at = Some(chrono::Utc::now());
            match plan.access_method.as_deref() {
                Some("index" | "index_lookup" | "maintained_count") => access.indexed_queries += 1,
                Some(_) => {
                    access.scanned_queries += 1;
                    *access.scans.entry(shape(query)).or_default() += 1;
                }
                None => access.unplanned_queries += 1,
            }
            access.rows_scanned += count(plan.actual_rows_scanned);
            access.rows_returned += count(plan.actual_rows_returned);
            access.unrelated_rows_examined += count(plan.unrelated_records_examined);
        });
        let records = response
            .records
            .into_iter()
            .map(record)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((records, response.next_cursor))
    }
}

/// A canonical query with only what Compute uses.
fn canonical(
    collection: Collection,
    filter: Option<QueryFilter>,
    order_by: Vec<QueryOrder>,
    limit: usize,
    cursor: Option<String>,
) -> CanonicalQuery {
    CanonicalQuery {
        collection: collection.name().into(),
        filter,
        order_by,
        limit: Some(limit),
        offset: 0,
        cursor,
        projection: vec![],
        group_by: vec![],
        aggregates: vec![],
        references: vec![],
        semantic: None,
    }
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

/// A query's shape, for diagnostics: `Collection[field=,field>] by field`.
/// Field names and operators only; never values.
fn shape(query: &Query) -> String {
    let filters = query
        .filters
        .iter()
        .map(|filter| {
            format!(
                "{}{}",
                filter.field,
                match filter.comparison {
                    Comparison::Eq => "=",
                    Comparison::Gt => ">",
                    Comparison::Gte => ">=",
                    Comparison::Lt => "<",
                    Comparison::Lte => "<=",
                    Comparison::In => " in",
                }
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let order = query
        .order_by
        .as_ref()
        .map(|(field, descending)| format!(" by {field}{}", if *descending { " desc" } else { "" }))
        .unwrap_or_default();
    format!("{}[{filters}]{order}", query.collection.name())
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
        // Discovered (and refreshed) through the same application handle,
        // so this is one request at the revision Compute holds.
        self.revision(false).await?;
        let version = match self.application.state_version().await {
            Ok(version) => {
                self.account(|access| access.connection = "connected".into());
                version
            }
            Err(error) => {
                return Err(match self.refusal(error) {
                    Ok(refusal) => refused(refusal, &[]),
                    Err(error) => error,
                });
            }
        };
        self.account(|access| {
            access.revision_reads += 1;
            access.last_read_at = Some(chrono::Utc::now());
        });
        Ok(Some(Revision {
            value: version.state_version,
            scope: format!(
                "feltdb:{}:{}",
                self.config.url,
                version.state_namespace.unwrap_or_default()
            ),
        }))
    }

    fn access(&self) -> Option<AccessReport> {
        Some(self.access.lock().expect("access").clone())
    }

    fn take_transitions(&self) -> Option<Vec<compute_state::Transition>> {
        Some(self.transitions.take())
    }

    /// Give every record written before `record_id` existed its identity
    /// field, so identity lookups through the index find it. Each batch is
    /// one transaction of merge-updates fenced on the versions read; a
    /// record another writer changed meanwhile is picked up next pass.
    async fn upgrade_records(&self) -> Result<std::collections::BTreeMap<String, u64>, StateError> {
        let mut upgraded = std::collections::BTreeMap::new();
        // Which collections hold records without it: one probe each, all
        // at once. Once upgraded, this is the whole cost of a start.
        let mut probes = tokio::task::JoinSet::new();
        for collection in Collection::ALL {
            let this = self.clone();
            probes.spawn(async move {
                let missing = this.without_identity(collection, 1).await?;
                Ok::<_, StateError>((collection, !missing.is_empty()))
            });
        }
        let mut pending = vec![];
        while let Some(probe) = probes.join_next().await {
            let (collection, missing) =
                probe.map_err(|error| StateError::Unavailable(error.to_string()))??;
            if missing {
                pending.push(collection);
            }
        }
        for collection in pending {
            let mut total = 0;
            loop {
                let records = self.without_identity(collection, 200).await?;
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
        self.commit_tracked(writes).await.map(|_| ())
    }

    /// FeltDB reports the committed state version immediately before and
    /// after the transaction, read inside its commit critical section.
    async fn commit_tracked(&self, writes: Vec<Write>) -> Result<Option<(u64, u64)>, StateError> {
        let transaction_id = format!(
            "compute-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default(),
            self.transactions.fetch_add(1, Ordering::Relaxed)
        );
        // Each write fenced on the version it was read at, where it was.
        let mut transaction = AppTransaction::new(transaction_id);
        for write in &writes {
            transaction = match write {
                Write::Create {
                    collection,
                    id,
                    value,
                } => transaction.insert(collection.name(), id.as_str(), identified(value, id)),
                // FeltDB updates merge fields; Compute replaces documents.
                // A fenced delete and an insert in one transaction is an
                // atomic replace.
                Write::Replace {
                    collection,
                    id,
                    value,
                    expected,
                } => match expected {
                    Some(version) => {
                        transaction.delete_if_version(collection.name(), id.as_str(), *version)
                    }
                    None => transaction.delete(collection.name(), id.as_str()),
                }
                .insert(collection.name(), id.as_str(), identified(value, id)),
                // FeltDB's native update merges top-level fields.
                Write::Update {
                    collection,
                    id,
                    fields,
                    expected,
                } => {
                    let fields = Value::Object(fields.clone());
                    match expected {
                        Some(version) => transaction.update_if_version(
                            collection.name(),
                            id.as_str(),
                            fields,
                            *version,
                        ),
                        None => transaction.update(collection.name(), id.as_str(), fields),
                    }
                }
                Write::Delete {
                    collection,
                    id,
                    expected,
                } => match expected {
                    Some(version) => {
                        transaction.delete_if_version(collection.name(), id.as_str(), *version)
                    }
                    None => transaction.delete(collection.name(), id.as_str()),
                },
            };
        }
        // A transport failure is retried once with the same transaction ID;
        // FeltDB answers a replayed ID with the original result. A schema
        // that moved is rediscovered once.
        let (mut retried, mut refreshed) = (false, false);
        loop {
            let refusal = match self.application.commit(&transaction).await {
                Ok(commit) => {
                    let transition = Some((commit.result.state_before, commit.result.state_after));
                    return Ok(self.committed(transition, &writes));
                }
                // Committed: an answer Compute cannot read does not undo
                // it, so its state versions are simply unknown.
                Err(feltdb_client::Error::Decode { .. }) => {
                    return Ok(self.committed(None, &writes));
                }
                Err(error) => self.refusal(error),
            };
            match refusal {
                Err(_) if !retried => retried = true,
                Ok(refusal) if !refreshed && Self::schema_moved(&refusal) => {
                    refreshed = true;
                    self.revision(true).await?;
                }
                Ok(refusal) => {
                    self.account(|access| access.failed_transactions += 1);
                    return Err(refused(refusal, &writes));
                }
                Err(error) => {
                    self.account(|access| access.failed_transactions += 1);
                    return Err(error);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> FeltDbConfig {
        FeltDbConfig {
            url: "https://feltdb.example.com".into(),
            token: "token".into(),
            application_id: "app".into(),
            environment: "production".into(),
            ca_certificate: None,
        }
    }

    #[test]
    fn a_private_ca_that_is_not_a_certificate_is_refused_at_start() {
        let error = FeltDbState::new(FeltDbConfig {
            ca_certificate: Some(b"not a certificate".to_vec()),
            ..config()
        })
        .err()
        .expect("refused");
        assert!(
            matches!(&error, StateError::Invalid(message) if message.contains("feltdb.example.com")),
            "{error:?}"
        );
        assert!(FeltDbState::new(config()).is_ok());
    }

    #[test]
    fn the_certified_version_is_the_client_this_build_resolves() {
        let lock = include_str!("../../../Cargo.lock");
        let resolved = lock
            .split("[[package]]")
            .find(|package| package.contains("name = \"feltdb-client\"\n"))
            .and_then(|package| {
                package
                    .lines()
                    .find_map(|line| line.strip_prefix("version = \""))
                    .map(|version| version.trim_end_matches('"').to_owned())
            })
            .expect("feltdb-client in Cargo.lock");
        assert_eq!(resolved, CERTIFIED_FELTDB_VERSION);
    }
}
