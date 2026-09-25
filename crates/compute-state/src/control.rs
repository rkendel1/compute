//! Typed access to the control model over any `StateStore`.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::model::Document;
use crate::snapshot::{SnapshotDefinition, SnapshotHandle, SnapshotRegistry};
use crate::store::{BackendInfo, Query, Record, StateError, StateStore, Write};

/// A typed document with its identity and version.
#[derive(Debug, Clone, PartialEq)]
pub struct Stored<T> {
    pub id: String,
    pub version: u64,
    pub value: T,
}

pub(crate) fn decode<T: Document>(record: Record) -> Result<Stored<T>, StateError> {
    let value = serde_json::from_value(Value::Object(record.value)).map_err(|error| {
        StateError::Invalid(format!(
            "{} {} does not match compute.state@1: {error}",
            T::COLLECTION.name(),
            record.id
        ))
    })?;
    Ok(Stored {
        id: record.id,
        version: record.version,
        value,
    })
}

fn encode<T: Document>(value: &T) -> Result<Map<String, Value>, StateError> {
    match serde_json::to_value(value) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(StateError::Invalid(format!(
            "{} documents must be JSON objects",
            T::COLLECTION.name()
        ))),
        Err(error) => Err(StateError::Invalid(error.to_string())),
    }
}

/// An atomic batch of typed writes.
#[derive(Debug, Default)]
pub struct Batch {
    writes: Vec<Write>,
    error: Option<StateError>,
}

impl Batch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.writes.len()
    }

    /// The documents this batch writes.
    pub fn targets(&self) -> Vec<(crate::Collection, String)> {
        self.writes
            .iter()
            .map(|write| (write.collection(), write.id().to_string()))
            .collect()
    }

    /// Insert a document that must not exist yet.
    pub fn create<T: Document>(mut self, id: &str, value: &T) -> Self {
        match encode(value) {
            Ok(value) => self.writes.push(Write::Create {
                collection: T::COLLECTION,
                id: id.into(),
                value,
            }),
            Err(error) => self.error = Some(error),
        }
        self
    }

    /// Replace a document read at `stored.version`, failing if another
    /// writer changed it since.
    pub fn replace<T: Document>(mut self, stored: &Stored<T>, value: &T) -> Self {
        match encode(value) {
            Ok(value) => self.writes.push(Write::Replace {
                collection: T::COLLECTION,
                id: stored.id.clone(),
                value,
                expected: Some(stored.version),
            }),
            Err(error) => self.error = Some(error),
        }
        self
    }

    /// Merge fields into a document read at `stored.version`. Fields not
    /// given keep their values; use `replace` to clear an optional field.
    pub fn update<T: Document>(mut self, stored: &Stored<T>, fields: serde_json::Value) -> Self {
        match fields {
            Value::Object(fields) => self.writes.push(Write::Update {
                collection: T::COLLECTION,
                id: stored.id.clone(),
                fields,
                expected: Some(stored.version),
            }),
            _ => {
                self.error = Some(StateError::Invalid(
                    "update fields must be a JSON object".into(),
                ))
            }
        }
        self
    }

    /// Delete a document read at `stored.version`.
    pub fn delete<T: Document>(mut self, stored: &Stored<T>) -> Self {
        self.writes.push(Write::Delete {
            collection: T::COLLECTION,
            id: stored.id.clone(),
            expected: Some(stored.version),
        });
        self
    }

    pub fn into_writes(self) -> Result<Vec<Write>, StateError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.writes),
        }
    }
}

/// The daemon's view of durable control state.
#[derive(Clone)]
pub struct ControlState {
    store: Arc<dyn StateStore>,
    snapshots: Arc<SnapshotRegistry>,
}

impl ControlState {
    pub fn new(store: Arc<dyn StateStore>) -> Self {
        Self {
            store,
            snapshots: Arc::new(SnapshotRegistry::default()),
        }
    }

    /// Resolve a snapshot handle: the same name and definition always
    /// resolve the same handle.
    pub fn snapshot(
        &self,
        definition: SnapshotDefinition,
    ) -> Result<Arc<SnapshotHandle>, StateError> {
        self.snapshots.resolve(definition, &self.store)
    }

    pub fn snapshots(&self) -> &SnapshotRegistry {
        &self.snapshots
    }

    /// Documents of `T` by identity, in bounded requests.
    pub async fn get_many<T: Document>(
        &self,
        ids: &[String],
    ) -> Result<Vec<Stored<T>>, StateError> {
        let mut stored = vec![];
        for chunk in ids.chunks(200) {
            for record in self.store.get_many(T::COLLECTION, chunk).await? {
                stored.push(decode(record)?);
            }
        }
        Ok(stored)
    }

    pub fn backend(&self) -> BackendInfo {
        self.store.backend()
    }

    pub fn store(&self) -> &Arc<dyn StateStore> {
        &self.store
    }

    pub async fn get<T: Document>(&self, id: &str) -> Result<Option<Stored<T>>, StateError> {
        self.store
            .get(T::COLLECTION, id)
            .await?
            .map(decode)
            .transpose()
    }

    /// Documents of `T`'s collection matching `query`. The query's
    /// collection is always `T`'s.
    pub async fn query<T: Document>(&self, mut query: Query) -> Result<Vec<Stored<T>>, StateError> {
        query.collection = T::COLLECTION;
        self.store
            .query(&query)
            .await?
            .into_iter()
            .map(decode)
            .collect()
    }

    /// Every document of `T`'s collection. Unbounded: for collections the
    /// caller genuinely needs whole and that are bounded by construction
    /// (providers, services, credentials), never to find a few records.
    pub async fn list<T: Document>(&self) -> Result<Vec<Stored<T>>, StateError> {
        self.query(Query::all(T::COLLECTION)).await
    }

    /// Apply a batch atomically.
    pub async fn transaction(&self, batch: Batch) -> Result<(), StateError> {
        let writes = batch.into_writes()?;
        if writes.is_empty() {
            return Ok(());
        }
        self.store.commit(writes).await
    }
}
