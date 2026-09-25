//! Control state in one JSON document on local disk.
//!
//! Every commit rewrites the document atomically: write a temporary file,
//! flush it to disk, and rename it over the previous one. A crash leaves
//! either the old state or the new state, never a mixture. Control state is
//! small, so rewriting it whole keeps the format obvious.
//!
//! One daemon owns a state file at a time; the daemon enforces that with a
//! lock on its state directory.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use compute_state::{
    BackendInfo, Collection, Query, Record, Revision, STATE_VERSION, StateError, StateStore,
    Tables, Write, apply_in_memory,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

#[derive(Serialize, Deserialize)]
struct Document {
    version: String,
    next_version: u64,
    collections: BTreeMap<String, BTreeMap<String, StoredDocument>>,
}

#[derive(Serialize, Deserialize)]
struct StoredDocument {
    version: u64,
    value: Map<String, Value>,
}

pub struct FileState {
    path: PathBuf,
    inner: Mutex<(Tables, u64)>,
}

fn io(path: &Path, error: std::io::Error) -> StateError {
    StateError::Unavailable(format!("{}: {error}", path.display()))
}

impl FileState {
    /// Open the state file, creating it when absent.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, StateError> {
        let path = path.into();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
        }
        let (tables, next_version) = if path.is_file() {
            let bytes = std::fs::read(&path).map_err(|error| io(&path, error))?;
            let document: Document = serde_json::from_slice(&bytes)
                .map_err(|error| StateError::Invalid(format!("{}: {error}", path.display())))?;
            if document.version != STATE_VERSION {
                return Err(StateError::Invalid(format!(
                    "{} is {}, not {STATE_VERSION}",
                    path.display(),
                    document.version
                )));
            }
            let mut tables = Tables::new();
            for (name, documents) in document.collections {
                let collection = Collection::from_name(&name).ok_or_else(|| {
                    StateError::Invalid(format!("unknown collection {name} in {}", path.display()))
                })?;
                tables.insert(
                    collection,
                    documents
                        .into_iter()
                        .map(|(id, stored)| (id, (stored.version, stored.value)))
                        .collect(),
                );
            }
            (tables, document.next_version)
        } else {
            (Tables::new(), 0)
        };
        Ok(Self {
            path,
            inner: Mutex::new((tables, next_version)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, tables: &Tables, next_version: u64) -> Result<(), StateError> {
        let document = Document {
            version: STATE_VERSION.into(),
            next_version,
            collections: tables
                .iter()
                .map(|(collection, documents)| {
                    (
                        collection.name().to_string(),
                        documents
                            .iter()
                            .map(|(id, (version, value))| {
                                (
                                    id.clone(),
                                    StoredDocument {
                                        version: *version,
                                        value: value.clone(),
                                    },
                                )
                            })
                            .collect(),
                    )
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|error| StateError::Invalid(error.to_string()))?;
        let temporary = self.path.with_extension("json.tmp");
        {
            let mut file =
                std::fs::File::create(&temporary).map_err(|error| io(&temporary, error))?;
            file.write_all(&bytes)
                .map_err(|error| io(&temporary, error))?;
            file.sync_all().map_err(|error| io(&temporary, error))?;
        }
        std::fs::rename(&temporary, &self.path).map_err(|error| io(&self.path, error))?;
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            && let Ok(directory) = std::fs::File::open(parent)
        {
            let _ = directory.sync_all();
        }
        Ok(())
    }
}

#[async_trait]
impl StateStore for FileState {
    fn backend(&self) -> BackendInfo {
        BackendInfo {
            kind: "file".into(),
            location: self.path.display().to_string(),
            durable: true,
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
        let mut tables = inner.0.clone();
        let mut next_version = inner.1;
        apply_in_memory(&mut tables, writes, &mut next_version)?;
        // Durable first: memory changes only once the file does.
        self.write(&tables, next_version)?;
        *inner = (tables, next_version);
        Ok(())
    }

    /// Every committed write advances the version counter, which the file
    /// keeps across restarts.
    async fn revision(&self) -> Result<Option<Revision>, StateError> {
        Ok(Some(Revision {
            value: self.inner.lock().await.1,
            scope: format!("file:{}", self.path.display()),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use compute_state::StateStore;

    #[tokio::test]
    async fn file_state_conforms_and_survives_reopening() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control-state.json");
        let store = Arc::new(super::FileState::open(&path).unwrap());
        let reopen =
            move || -> Arc<dyn StateStore> { Arc::new(super::FileState::open(&path).unwrap()) };
        compute_state::conformance::check(store, Some(&reopen)).await;
    }

    #[test]
    fn a_foreign_document_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"version":"other@9","next_version":0,"collections":{}}"#,
        )
        .unwrap();
        assert!(super::FileState::open(&path).is_err());
    }
}

/// Artifacts as files named by digest in a directory. File-backed control
/// state keeps artifacts out of its JSON document.
pub struct DirectoryArtifacts {
    directory: PathBuf,
}

impl DirectoryArtifacts {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    fn path(&self, digest: &str) -> Result<PathBuf, StateError> {
        let hex = digest
            .strip_prefix("sha256:")
            .filter(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| StateError::Invalid(format!("invalid artifact digest {digest}")))?;
        Ok(self.directory.join(hex))
    }
}

#[async_trait]
impl compute_state::ArtifactStore for DirectoryArtifacts {
    async fn put(&self, _kind: &str, bytes: &[u8]) -> Result<String, StateError> {
        let digest = compute_state::artifacts::digest(bytes);
        let path = self.path(&digest)?;
        if path.is_file() {
            return Ok(digest);
        }
        std::fs::create_dir_all(&self.directory).map_err(|error| io(&self.directory, error))?;
        let temporary = path.with_extension("tmp");
        {
            let mut file =
                std::fs::File::create(&temporary).map_err(|error| io(&temporary, error))?;
            file.write_all(bytes)
                .map_err(|error| io(&temporary, error))?;
            file.sync_all().map_err(|error| io(&temporary, error))?;
        }
        std::fs::rename(&temporary, &path).map_err(|error| io(&path, error))?;
        Ok(digest)
    }

    async fn get(&self, digest: &str) -> Result<Option<Vec<u8>>, StateError> {
        let path = self.path(digest)?;
        match std::fs::read(&path) {
            Ok(bytes) => compute_state::artifacts::verified(digest, bytes).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io(&path, error)),
        }
    }

    fn location(&self) -> String {
        self.directory.display().to_string()
    }
}

#[cfg(test)]
mod artifact_tests {
    use compute_state::ArtifactStore;

    #[tokio::test]
    async fn directory_artifacts_round_trip_and_detect_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let store = super::DirectoryArtifacts::new(directory.path());
        let digest = store.put("bundle", b"bundle bytes").await.unwrap();
        assert_eq!(
            store.get(&digest).await.unwrap().as_deref(),
            Some(&b"bundle bytes"[..])
        );
        let path = directory
            .path()
            .join(digest.strip_prefix("sha256:").unwrap());
        std::fs::write(path, b"tampered").unwrap();
        assert!(store.get(&digest).await.is_err(), "corruption is detected");
        assert!(
            store.get("../etc/passwd").await.is_err(),
            "digests are validated"
        );
    }
}
