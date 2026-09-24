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
}

impl Collection {
    pub const ALL: [Collection; 14] = [
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub field: String,
    pub comparison: Comparison,
    pub value: Value,
}

impl Filter {
    pub fn matches(&self, document: &Map<String, Value>) -> bool {
        let Some(actual) = document.get(&self.field) else {
            return false;
        };
        let ordering = compare(actual, &self.value);
        match self.comparison {
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
            .filter(|record| self.filters.iter().all(|f| f.matches(&record.value)))
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
    let mut staged = tables.clone();
    for write in writes {
        let collection = write.collection();
        let table = staged.entry(collection).or_default();
        let current = table.get(write.id()).map(|(version, _)| *version);
        let fence = |expected: Option<u64>, id: &str| match (expected, current) {
            (_, None) => Err(StateError::NotFound {
                collection,
                id: id.into(),
            }),
            (Some(expected), Some(actual)) if expected != actual => Err(StateError::Precondition {
                collection,
                id: id.into(),
                expected,
                actual: Some(actual),
            }),
            _ => Ok(()),
        };
        match write {
            Write::Create { id, value, .. } => {
                if current.is_some() {
                    return Err(StateError::Conflict { collection, id });
                }
                *next_version += 1;
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
                table.insert(id, (*next_version, value));
            }
            Write::Update {
                id,
                fields,
                expected,
                ..
            } => {
                fence(expected, &id)?;
                *next_version += 1;
                let (_, document) = table.get(&id).cloned().expect("fenced");
                let mut document = document;
                document.extend(fields);
                table.insert(id, (*next_version, document));
            }
            Write::Delete { id, expected, .. } => {
                fence(expected, &id)?;
                table.remove(&id);
            }
        }
    }
    *tables = staged;
    Ok(())
}
