//! The storage interface every backend implements.
//!
//! The model is deliberately small: typed collections of JSON documents,
//! each with a backend-assigned version, and atomic batches of writes that
//! can be fenced on those versions. Memory, file, and FeltDB implement the
//! same semantics, and `crate::conformance` proves it.

use std::cmp::Ordering;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Every durable collection of the Compute control model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Collection {
    Project,
    ProjectRevision,
    Environment,
    EnvironmentProject,
    Deployment,
    Workload,
    Execution,
    Service,
    Provider,
    Receipt,
    Event,
    WorkloadStatus,
    Artifact,
    ArtifactChunk,
    WorkloadInstance,
    TrafficAssignment,
    Domain,
    DnsRecord,
    Certificate,
    OperatorCredential,
    Audit,
}

impl Collection {
    pub const ALL: [Collection; 21] = [
        Self::Project,
        Self::ProjectRevision,
        Self::Environment,
        Self::EnvironmentProject,
        Self::Deployment,
        Self::Workload,
        Self::Execution,
        Self::Service,
        Self::Provider,
        Self::Receipt,
        Self::Event,
        Self::WorkloadStatus,
        Self::Artifact,
        Self::ArtifactChunk,
        Self::WorkloadInstance,
        Self::TrafficAssignment,
        Self::Domain,
        Self::DnsRecord,
        Self::Certificate,
        Self::OperatorCredential,
        Self::Audit,
    ];

    /// The collection's name in every backend, and in `compute.flow`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::ProjectRevision => "ProjectRevision",
            Self::Environment => "Environment",
            Self::EnvironmentProject => "EnvironmentProject",
            Self::Deployment => "Deployment",
            Self::Workload => "Workload",
            Self::Execution => "Execution",
            Self::Service => "Service",
            Self::Provider => "Provider",
            Self::Receipt => "Receipt",
            Self::Event => "Event",
            Self::WorkloadStatus => "WorkloadStatus",
            Self::Artifact => "Artifact",
            Self::ArtifactChunk => "ArtifactChunk",
            Self::WorkloadInstance => "WorkloadInstance",
            Self::TrafficAssignment => "TrafficAssignment",
            Self::Domain => "Domain",
            Self::DnsRecord => "DnsRecord",
            Self::Certificate => "Certificate",
            Self::OperatorCredential => "OperatorCredential",
            Self::Audit => "Audit",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|collection| collection.name() == name)
    }
}

/// A stored document. `version` is assigned by the backend and increases on
/// every write; it is what a fence compares.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub version: u64,
    pub value: Map<String, Value>,
}

/// One write in an atomic batch.
#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    /// Insert a document that must not exist.
    Create {
        collection: Collection,
        id: String,
        value: Map<String, Value>,
    },
    /// Replace an existing document entirely. With `expected`, only if its
    /// version still matches.
    Replace {
        collection: Collection,
        id: String,
        value: Map<String, Value>,
        expected: Option<u64>,
    },
    /// Merge top-level fields into an existing document; fields not given
    /// keep their values. With `expected`, only if its version still
    /// matches.
    Update {
        collection: Collection,
        id: String,
        fields: Map<String, Value>,
        expected: Option<u64>,
    },
    /// Delete an existing document. With `expected`, only if its version
    /// still matches.
    Delete {
        collection: Collection,
        id: String,
        expected: Option<u64>,
    },
}

impl Write {
    pub fn collection(&self) -> Collection {
        match self {
            Self::Create { collection, .. }
            | Self::Replace { collection, .. }
            | Self::Update { collection, .. }
            | Self::Delete { collection, .. } => *collection,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Create { id, .. }
            | Self::Replace { id, .. }
            | Self::Update { id, .. }
            | Self::Delete { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
    /// The field equals one of the values of an array.
    In,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub field: String,
    pub comparison: Comparison,
    pub value: Value,
}

/// The field that names a record's identity in every backend, as in
/// FeltDB. It is not stored in the document.
pub const ID_FIELD: &str = "_id";

impl Filter {
    pub fn matches(&self, document: &Map<String, Value>) -> bool {
        self.matches_value(document.get(&self.field))
    }

    /// Whether a record matches, with `_id` meaning its identity.
    pub fn matches_record(&self, record: &Record) -> bool {
        if self.field == ID_FIELD {
            return self.matches_value(Some(&Value::String(record.id.clone())));
        }
        self.matches(&record.value)
    }

    fn matches_value(&self, actual: Option<&Value>) -> bool {
        let Some(actual) = actual else {
            return false;
        };
        let ordering = compare(actual, &self.value);
        match self.comparison {
            Comparison::In => self
                .value
                .as_array()
                .is_some_and(|values| values.contains(actual)),
            Comparison::Eq => actual == &self.value,
            Comparison::Gt => ordering == Some(Ordering::Greater),
            Comparison::Gte => matches!(ordering, Some(Ordering::Greater | Ordering::Equal)),
            Comparison::Lt => ordering == Some(Ordering::Less),
            Comparison::Lte => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
        }
    }
}

/// A bounded query: conjunctive filters, one ordering field, and a limit.
/// Results without an ordering are ordered by document ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Query {
    pub collection: Collection,
    pub filters: Vec<Filter>,
    pub order_by: Option<(String, bool)>,
    pub limit: Option<usize>,
}

impl Query {
    pub fn all(collection: Collection) -> Self {
        Self {
            collection,
            filters: vec![],
            order_by: None,
            limit: None,
        }
    }

    fn filter(mut self, field: &str, comparison: Comparison, value: impl Into<Value>) -> Self {
        self.filters.push(Filter {
            field: field.into(),
            comparison,
            value: value.into(),
        });
        self
    }

    pub fn eq(self, field: &str, value: impl Into<Value>) -> Self {
        self.filter(field, Comparison::Eq, value)
    }

    pub fn gt(self, field: &str, value: impl Into<Value>) -> Self {
        self.filter(field, Comparison::Gt, value)
    }

    /// The field equals one of `values`.
    pub fn one_of<V: Into<Value>>(self, field: &str, values: impl IntoIterator<Item = V>) -> Self {
        let values = values.into_iter().map(Into::into).collect::<Vec<Value>>();
        self.filter(field, Comparison::In, Value::Array(values))
    }

    /// The records whose identity is one of `ids`.
    pub fn ids<S: Into<String>>(self, ids: impl IntoIterator<Item = S>) -> Self {
        self.one_of(ID_FIELD, ids.into_iter().map(Into::into))
    }

    pub fn ascending(mut self, field: &str) -> Self {
        self.order_by = Some((field.into(), false));
        self
    }

    pub fn descending(mut self, field: &str) -> Self {
        self.order_by = Some((field.into(), true));
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Evaluate the query over documents in memory. The memory and file
    /// backends use this; it defines the semantics FeltDB must match.
    pub fn evaluate(&self, records: impl Iterator<Item = Record>) -> Vec<Record> {
        let mut records = records
            .filter(|record| self.filters.iter().all(|f| f.matches_record(record)))
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.id.cmp(&right.id));
        if let Some((field, descending)) = &self.order_by {
            records.sort_by(|left, right| {
                let ordering = match (left.value.get(field), right.value.get(field)) {
                    (Some(left), Some(right)) => compare(left, right).unwrap_or(Ordering::Equal),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                };
                if *descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            });
        }
        if let Some(limit) = self.limit {
            records.truncate(limit);
        }
        records
    }
}

fn compare(left: &Value, right: &Value) -> Option<Ordering> {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => match (left.as_i64(), right.as_i64()) {
            (Some(left), Some(right)) => Some(left.cmp(&right)),
            _ => left.as_f64()?.partial_cmp(&right.as_f64()?),
        },
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

/// The authoritative revision of a backend's state: a counter that moves
/// on every committed write, within a consistency scope. Two reads that see
/// the same revision (and scope) saw the same state; this is what proves a
/// snapshot coherent and a cache current. It is FeltDB's `Revision`
/// (`freshness.ts`): FeltDB's committed state version, scoped to its store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub value: u64,
    pub scope: String,
}

/// What a backend has done at its boundary, for diagnostics. Counters
/// only: never records, credentials, or tokens.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccessReport {
    /// The authority's reported server version, when it reports one.
    pub server_version: Option<String>,
    /// The client release this backend is certified against (FeltDB:
    /// the `@feltdb/core` version).
    pub certified_version: Option<String>,
    /// `connected`, `unreachable`, `refused`, or `unknown` (never tried).
    pub connection: String,
    pub last_read_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_mutation_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    pub last_error_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Query requests sent, and how the authority executed them.
    pub queries: u64,
    pub indexed_queries: u64,
    pub scanned_queries: u64,
    /// Queries whose plan the authority did not report.
    pub unplanned_queries: u64,
    pub rows_scanned: u64,
    pub rows_returned: u64,
    /// Candidates outside the requested scope that the authority examined.
    pub unrelated_rows_examined: u64,
    pub revision_reads: u64,
    pub transactions: u64,
    pub failed_transactions: u64,
    /// Queries the authority answered by scanning, by shape: collection,
    /// filtered fields, and ordering. Field names only, never values.
    #[serde(default)]
    pub scans: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    /// `memory`, `file`, or `feltdb`.
    pub kind: String,
    /// Where the state lives: a path, a URL and application, or `memory`.
    pub location: String,
    /// Whether state survives the daemon process.
    pub durable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{collection:?} {id} already exists")]
    Conflict { collection: Collection, id: String },
    #[error("{collection:?} {id} changed: expected version {expected}, found {actual:?}")]
    Precondition {
        collection: Collection,
        id: String,
        expected: u64,
        actual: Option<u64>,
    },
    #[error("{collection:?} {id} does not exist")]
    NotFound { collection: Collection, id: String },
    #[error("invalid state: {0}")]
    Invalid(String),
    #[error("state backend unavailable: {0}")]
    Unavailable(String),
}

impl StateError {
    /// A lost race with another writer, rather than a broken backend.
    pub fn is_race(&self) -> bool {
        matches!(self, Self::Conflict { .. } | Self::Precondition { .. })
    }
}

/// The Compute control-state boundary. The daemon depends on this trait
/// only; which backend stores the state is a deployment choice.
#[async_trait]
pub trait StateStore: Send + Sync {
    fn backend(&self) -> BackendInfo;

    async fn get(&self, collection: Collection, id: &str) -> Result<Option<Record>, StateError>;

    async fn query(&self, query: &Query) -> Result<Vec<Record>, StateError>;

    /// Apply every write or none.
    async fn commit(&self, writes: Vec<Write>) -> Result<(), StateError>;

    /// [`StateStore::commit`], also returning the revisions immediately
    /// before and after this commit, when the backend can state them
    /// exactly (read inside its commit critical section). A caller that
    /// holds state proven current at `before` knows it is current at
    /// `after` once it applies its own writes; any other writer in between
    /// breaks that chain.
    async fn commit_tracked(&self, writes: Vec<Write>) -> Result<Option<(u64, u64)>, StateError> {
        self.commit(writes).await.map(|()| None)
    }

    /// The current authoritative revision. `None` when the backend cannot
    /// report one: then a snapshot is `refresh`-validated and never
    /// claimed coherent.
    async fn revision(&self) -> Result<Option<Revision>, StateError> {
        Ok(None)
    }

    /// Records by identity, in one bounded request where the backend can:
    /// never a scan of the collection. Missing IDs are absent.
    async fn get_many(
        &self,
        collection: Collection,
        ids: &[String],
    ) -> Result<Vec<Record>, StateError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        self.query(&Query::all(collection).ids(ids.iter().cloned()))
            .await
    }

    /// Every commit made through this store since the last call: its
    /// revisions before and after, and the collections it wrote, in no
    /// particular order. `None` when the backend does not record them. A
    /// commit whose revisions could not be stated is [`UNCHAINED`], which
    /// never chains. One consumer (the controller) drains it.
    fn take_transitions(&self) -> Option<Vec<Transition>> {
        None
    }

    /// Boundary diagnostics, for backends that keep them.
    fn access(&self) -> Option<AccessReport> {
        None
    }

    /// Bring records an older Compute wrote up to what this build reads,
    /// in place and fenced on their versions. Idempotent; returns how many
    /// records changed, by collection. Never discards or rewrites data a
    /// record carries.
    async fn upgrade_records(&self) -> Result<std::collections::BTreeMap<String, u64>, StateError> {
        Ok(std::collections::BTreeMap::new())
    }
}

/// One committed transaction, as the store that committed it saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub before: u64,
    pub after: u64,
    pub collections: std::collections::BTreeSet<Collection>,
}

/// A commit whose revisions are unknown: it breaks any chain.
pub const UNCHAINED: (u64, u64) = (u64::MAX - 1, u64::MAX);

/// Commit transitions a backend has recorded, bounded: past the bound the
/// record is replaced by one [`UNCHAINED`] entry, which only costs the
/// consumer a full read.
#[derive(Default)]
pub struct Transitions(std::sync::Mutex<Vec<Transition>>);

impl Transitions {
    const CAPACITY: usize = 65_536;

    fn unchained() -> Transition {
        Transition {
            before: UNCHAINED.0,
            after: UNCHAINED.1,
            collections: Collection::ALL.into_iter().collect(),
        }
    }

    pub fn record(&self, transition: Option<(u64, u64)>, writes: &[Write]) {
        let mut recorded = self.0.lock().expect("transitions");
        if recorded.len() >= Self::CAPACITY {
            recorded.clear();
            recorded.push(Self::unchained());
        }
        recorded.push(match transition {
            Some((before, after)) => Transition {
                before,
                after,
                collections: writes.iter().map(Write::collection).collect(),
            },
            None => Self::unchained(),
        });
    }

    pub fn take(&self) -> Vec<Transition> {
        std::mem::take(&mut *self.0.lock().expect("transitions"))
    }
}

/// Documents by collection and ID, with their versions: the in-memory form
/// of state used by the memory and file backends.
pub type Tables = std::collections::BTreeMap<
    Collection,
    std::collections::BTreeMap<String, (u64, Map<String, Value>)>,
>;

/// Apply a batch to an in-memory table with the exact semantics every
/// backend must provide. Used by the memory and file backends.
pub fn apply_in_memory(
    tables: &mut Tables,
    writes: Vec<Write>,
    next_version: &mut u64,
) -> Result<(), StateError> {
    // Writes apply in place, each remembering what it replaced, so a batch
    // costs what it writes, not the size of the state. A refused write
    // undoes the ones before it: every write or none.
    let mut undo: Vec<(Collection, String, Option<(u64, Map<String, Value>)>)> = vec![];
    let result = (|| {
        for write in writes {
            let collection = write.collection();
            let table = tables.entry(collection).or_default();
            let current = table.get(write.id()).map(|(version, _)| *version);
            let fence = |expected: Option<u64>, id: &str| match (expected, current) {
                (_, None) => Err(StateError::NotFound {
                    collection,
                    id: id.into(),
                }),
                (Some(expected), Some(actual)) if expected != actual => {
                    Err(StateError::Precondition {
                        collection,
                        id: id.into(),
                        expected,
                        actual: Some(actual),
                    })
                }
                _ => Ok(()),
            };
            match write {
                Write::Create { id, value, .. } => {
                    if current.is_some() {
                        return Err(StateError::Conflict { collection, id });
                    }
                    *next_version += 1;
                    undo.push((collection, id.clone(), None));
                    table.insert(id, (*next_version, value));
                }
                Write::Replace {
                    id,
                    value,
                    expected,
                    ..
                } => {
                    fence(expected, &id)?;
                    *next_version += 1;
                    let previous = table.insert(id.clone(), (*next_version, value));
                    undo.push((collection, id, previous));
                }
                Write::Update {
                    id,
                    fields,
                    expected,
                    ..
                } => {
                    fence(expected, &id)?;
                    *next_version += 1;
                    let (version, document) = table.get(&id).cloned().expect("fenced");
                    let mut updated = document.clone();
                    updated.extend(fields);
                    table.insert(id.clone(), (*next_version, updated));
                    undo.push((collection, id, Some((version, document))));
                }
                Write::Delete { id, expected, .. } => {
                    fence(expected, &id)?;
                    // A delete moves the revision too.
                    *next_version += 1;
                    let previous = table.remove(&id);
                    undo.push((collection, id, previous));
                }
            }
        }
        Ok(())
    })();
    if result.is_err() {
        for (collection, id, previous) in undo.into_iter().rev() {
            let table = tables.entry(collection).or_default();
            match previous {
                Some(previous) => {
                    table.insert(id, previous);
                }
                None => {
                    table.remove(&id);
                }
            }
        }
    }
    result
}
