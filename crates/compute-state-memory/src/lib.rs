//! In-process control state. Nothing survives the process: use it for
//! tests and ephemeral daemons.

use async_trait::async_trait;
use compute_state::{
    BackendInfo, Collection, Query, Record, StateError, StateStore, Tables, Write, apply_in_memory,
};
use tokio::sync::Mutex;

#[derive(Default)]
pub struct MemoryState {
    inner: Mutex<(Tables, u64)>,
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
        let mut inner = self.inner.lock().await;
        let (tables, version) = &mut *inner;
        apply_in_memory(tables, writes, version)
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
