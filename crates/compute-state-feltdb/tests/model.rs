//! compute.flow and the Rust control model describe the same documents.

use std::collections::{BTreeMap, BTreeSet};

use compute_state::Collection;
use compute_state_feltdb::COMPUTE_MANIFEST;
use serde_json::Value;

/// Field name to (type, required) for every collection in the manifest.
fn manifest() -> BTreeMap<String, BTreeMap<String, (String, bool)>> {
    let manifest: Value = serde_json::from_str(COMPUTE_MANIFEST).unwrap();
    manifest["collections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|collection| {
            (
                collection["name"].as_str().unwrap().to_string(),
                collection["fields"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|field| {
                        (
                            field["name"].as_str().unwrap().to_string(),
                            (
                                field["type"].as_str().unwrap().to_string(),
                                field["required"].as_bool().unwrap(),
                            ),
                        )
                    })
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn every_collection_is_in_the_model() {
    let manifest = manifest();
    let ours = Collection::ALL
        .iter()
        .map(|collection| collection.name().to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(manifest.keys().cloned().collect::<BTreeSet<_>>(), ours);
}

/// A fully populated and a minimally populated document of each type must
/// use only declared fields, and always include every required one.
#[tokio::test]
async fn documents_match_the_declared_fields() {
    let manifest = manifest();
    let memory = std::sync::Arc::new(Recorder::default());
    compute_state::conformance::check(memory.clone(), None).await;
    let written = memory.written.lock().await;
    let mut seen = BTreeSet::new();
    for (collection, document) in written.iter() {
        seen.insert(collection.name());
        let fields = &manifest[collection.name()];
        for key in document.keys() {
            assert!(
                fields.contains_key(key),
                "{}.{key} is written but not declared in compute.flow",
                collection.name()
            );
        }
        for (field, (_, required)) in fields {
            if *required {
                assert!(
                    document.contains_key(field),
                    "{}.{field} is required by compute.flow but was not written",
                    collection.name()
                );
            }
        }
        for (key, value) in document {
            assert!(
                !value.is_null(),
                "{}.{key} is null; FeltDB refuses null, omit the field instead",
                collection.name()
            );
        }
    }
    for collection in Collection::ALL {
        assert!(
            seen.contains(collection.name()),
            "{} was not exercised",
            collection.name()
        );
    }
}

/// A memory store that also records every document written.
#[derive(Default)]
struct Recorder {
    inner: compute_state_memory::MemoryState,
    written: tokio::sync::Mutex<Vec<(Collection, serde_json::Map<String, Value>)>>,
}

#[async_trait::async_trait]
impl compute_state::StateStore for Recorder {
    fn backend(&self) -> compute_state::BackendInfo {
        compute_state::StateStore::backend(&self.inner)
    }
    async fn get(
        &self,
        collection: Collection,
        id: &str,
    ) -> Result<Option<compute_state::Record>, compute_state::StateError> {
        compute_state::StateStore::get(&self.inner, collection, id).await
    }
    async fn query(
        &self,
        query: &compute_state::Query,
    ) -> Result<Vec<compute_state::Record>, compute_state::StateError> {
        compute_state::StateStore::query(&self.inner, query).await
    }
    async fn commit(
        &self,
        writes: Vec<compute_state::Write>,
    ) -> Result<(), compute_state::StateError> {
        let mut written = self.written.lock().await;
        for write in &writes {
            match write {
                compute_state::Write::Create {
                    collection, value, ..
                }
                | compute_state::Write::Replace {
                    collection, value, ..
                } => {
                    written.push((*collection, value.clone()));
                }
                compute_state::Write::Update { .. } | compute_state::Write::Delete { .. } => {}
            }
        }
        compute_state::StateStore::commit(&self.inner, writes).await
    }
}
