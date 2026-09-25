//! In-process control state. Nothing survives the process: use it for
//! tests and ephemeral daemons.

use async_trait::async_trait;
use std::sync::atomic::{AtomicU64, Ordering};

use compute_state::{
    BackendInfo, Collection, Query, Record, Revision, StateError, StateStore, Tables, Transitions,
    Write, apply_in_memory,
};
use tokio::sync::Mutex;

/// Distinguishes the stores of one process: revisions of two stores are
/// never comparable.
static STORES: AtomicU64 = AtomicU64::new(0);

pub struct MemoryState {
    inner: Mutex<(Tables, u64)>,
    scope: String,
    transitions: Transitions,
}

impl Default for MemoryState {
    fn default() -> Self {
        Self {
            inner: Mutex::default(),
            scope: format!(
                "memory:{}:{}",
                std::process::id(),
                STORES.fetch_add(1, Ordering::Relaxed)
            ),
            transitions: Transitions::default(),
        }
    }
}

impl MemoryState {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StateStore for MemoryState {
    fn backend(&self) -> BackendInfo {
        BackendInfo {
            kind: "memory".into(),
            location: "memory".into(),
            durable: false,
        }
    }

    async fn get(&self, collection: Collection, id: &str) -> Result<Option<Record>, StateError> {
        let inner = self.inner.lock().await;
        Ok(inner
            .0
            .get(&collection)
            .and_then(|table| table.get(id))
            .map(|(version, value)| Record {
                id: id.into(),
                version: *version,
                value: value.clone(),
            }))
    }

    async fn query(&self, query: &Query) -> Result<Vec<Record>, StateError> {
        let inner = self.inner.lock().await;
        let records =
            inner
                .0
                .get(&query.collection)
                .into_iter()
                .flatten()
                .map(|(id, (version, value))| Record {
                    id: id.clone(),
                    version: *version,
                    value: value.clone(),
                });
        Ok(query.evaluate(records))
    }

    async fn commit(&self, writes: Vec<Write>) -> Result<(), StateError> {
        self.commit_tracked(writes).await.map(|_| ())
    }

    async fn commit_tracked(&self, writes: Vec<Write>) -> Result<Option<(u64, u64)>, StateError> {
        let mut inner = self.inner.lock().await;
        let (tables, version) = &mut *inner;
        let before = *version;
        apply_in_memory(tables, writes, version)?;
        self.transitions.record(Some((before, *version)));
        Ok(Some((before, *version)))
    }

    fn take_transitions(&self) -> Option<Vec<(u64, u64)>> {
        Some(self.transitions.take())
    }

    /// Every committed write advances the version counter.
    async fn revision(&self) -> Result<Option<Revision>, StateError> {
        Ok(Some(Revision {
            value: self.inner.lock().await.1,
            scope: self.scope.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    #[tokio::test]
    async fn memory_state_conforms() {
        compute_state::conformance::check(Arc::new(super::MemoryState::new()), None).await;
    }
}
