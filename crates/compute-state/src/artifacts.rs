//! Content-addressed artifacts: workload bundles and receipts.
//!
//! Control state references artifacts by digest. Where the bytes live is
//! the backend's choice: a local directory for file state, or chunks in the
//! control state itself for FeltDB, so a daemon on a fresh host can restore
//! every revision it is asked to run.

use async_trait::async_trait;
use base64::Engine as _;
use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::control::{Batch, ControlState};
use crate::model::{ArtifactChunkRecord, ArtifactRecord, ids};
use crate::store::{Collection, Query, StateError};

/// `sha256:<hex>` of `bytes`.
pub fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Store `bytes` and return their digest. Storing the same bytes again
    /// is a no-op.
    async fn put(&self, kind: &str, bytes: &[u8]) -> Result<String, StateError>;

    /// The bytes with `digest`, verified, or `None` when absent.
    async fn get(&self, digest: &str) -> Result<Option<Vec<u8>>, StateError>;

    /// Where artifacts live, for status output.
    fn location(&self) -> String;
}

/// Refuse bytes that do not match the digest they were stored under.
pub fn verified(expected: &str, bytes: Vec<u8>) -> Result<Vec<u8>, StateError> {
    let actual = digest(&bytes);
    if actual == expected {
        Ok(bytes)
    } else {
        Err(StateError::Invalid(format!(
            "artifact {expected} is corrupt: its bytes hash to {actual}"
        )))
    }
}

/// Artifacts stored in control state, in chunks small enough for any
/// backend's request limit. Chunks are written first and the artifact
/// record last, so a present record always has every chunk.
pub struct StateArtifacts {
    state: ControlState,
    chunk_bytes: usize,
}

impl StateArtifacts {
    /// 512 KiB chunks: under FeltDB's 1 MiB request limit once encoded.
    pub const CHUNK_BYTES: usize = 512 * 1024;

    pub fn new(state: ControlState) -> Self {
        Self {
            state,
            chunk_bytes: Self::CHUNK_BYTES,
        }
    }
}

#[async_trait]
impl ArtifactStore for StateArtifacts {
    async fn put(&self, kind: &str, bytes: &[u8]) -> Result<String, StateError> {
        let digest = digest(bytes);
        let id = ids::artifact(&digest);
        if self.state.get::<ArtifactRecord>(&id).await?.is_some() {
            return Ok(digest);
        }
        let chunks = bytes.chunks(self.chunk_bytes).collect::<Vec<_>>();
        for (index, chunk) in chunks.iter().enumerate() {
            let chunk_id = ids::artifact_chunk(&digest, index as u64);
            // A chunk left by an interrupted earlier upload is identical.
            if self
                .state
                .get::<ArtifactChunkRecord>(&chunk_id)
                .await?
                .is_some()
            {
                continue;
            }
            let record = ArtifactChunkRecord {
                digest: digest.clone(),
                position: index as u64,
                data: base64::engine::general_purpose::STANDARD.encode(chunk),
            };
            match self
                .state
                .transaction(Batch::new().create(&chunk_id, &record))
                .await
            {
                Ok(()) => {}
                Err(StateError::Conflict { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        let record = ArtifactRecord {
            digest: digest.clone(),
            kind: kind.into(),
            size: bytes.len() as u64,
            chunks: chunks.len() as u64,
            created_at: Utc::now(),
        };
        match self
            .state
            .transaction(Batch::new().create(&id, &record))
            .await
        {
            Ok(()) | Err(StateError::Conflict { .. }) => Ok(digest),
            Err(error) => Err(error),
        }
    }

    async fn get(&self, digest: &str) -> Result<Option<Vec<u8>>, StateError> {
        let Some(record) = self
            .state
            .get::<ArtifactRecord>(&ids::artifact(digest))
            .await?
        else {
            return Ok(None);
        };
        let chunks = self
            .state
            .query::<ArtifactChunkRecord>(
                Query::all(Collection::ArtifactChunk)
                    .eq("digest", digest)
                    .ascending("position"),
            )
            .await?;
        if chunks.len() as u64 != record.value.chunks {
            return Err(StateError::Invalid(format!(
                "artifact {digest} has {} of {} chunks",
                chunks.len(),
                record.value.chunks
            )));
        }
        let mut bytes = Vec::with_capacity(record.value.size as usize);
        for chunk in chunks {
            bytes.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(chunk.value.data)
                    .map_err(|error| StateError::Invalid(format!("artifact chunk: {error}")))?,
            );
        }
        verified(digest, bytes).map(Some)
    }

    fn location(&self) -> String {
        format!("control state ({})", self.state.backend().location)
    }
}
