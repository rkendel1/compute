//! Coherent, bounded snapshots of durable control state.
//!
//! This is FeltDB 0.11.8's runtime-snapshot contract (`db.snapshot({ name,
//! sources })`, `runtime-snapshot.ts`) for a Rust consumer of the FeltDB
//! Service API. The JavaScript `db.snapshot` is not callable from Rust, so
//! Compute implements the same contract over the same public primitives —
//! the authoritative revision and bounded queries — and nothing else:
//!
//! - **Explicit and bounded.** A definition names its sources (a collection
//!   and optional filters) and its references (records named by a field of
//!   another source, read by identity). Nothing outside them is read.
//! - **Coherent.** The revision is read on each side of the materialization.
//!   When it is unchanged the snapshot is a point-in-time view of one
//!   durable state (`coherence: proven`); when it moved, the work is
//!   discarded and retried. A backend that cannot report a revision gets
//!   `validation: refresh` and is never claimed coherent.
//! - **Immutable and atomically replaced.** A published snapshot is shared
//!   behind an `Arc` and never mutated; a refresh publishes a new one.
//! - **Reused.** When the authoritative revision still equals the active
//!   snapshot's, a refresh reads nothing else and returns the active
//!   snapshot: one revision read instead of a materialization.
//! - **Deterministic identity.** `snap_` + SHA-256 over the definition, the
//!   authority it was read under, and the revision (or, without one, the
//!   content), derived as FeltDB derives it.
//! - **Authorized.** Every read goes through the backend's own credentials;
//!   a snapshot can never read more than its store could.
//!
//! A snapshot is derived state, never authority: it has no writes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::control::Stored;
use crate::model::Document;
use crate::store::{Collection, Filter, Query, Record, Revision, StateError, StateStore};

/// IDs per identity read: a reference to many records is split into
/// bounded requests.
const IDS_PER_READ: usize = 200;

/// One collection a snapshot materializes, optionally filtered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSource {
    pub collection: Collection,
    #[serde(default)]
    pub filters: Vec<Filter>,
}

impl SnapshotSource {
    pub fn all(collection: Collection) -> Self {
        Self {
            collection,
            filters: vec![],
        }
    }

    pub fn filtered(query: Query) -> Self {
        Self {
            collection: query.collection,
            filters: query.filters,
        }
    }
}

/// Records of `to` whose identity is the value of `field` in the records
/// of `from` materialized so far. Read by identity, so bounded by what the
/// snapshot already holds, never by the size of `to`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotReference {
    pub from: Collection,
    pub field: String,
    pub to: Collection,
    /// `to` records never change once written (revisions): ones the
    /// previous snapshot holds are reused instead of read again.
    #[serde(default)]
    pub immutable: bool,
}

/// What to do when durable state moved during every attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Incoherent {
    /// Fail the build and keep the active snapshot (FeltDB's behavior).
    Fail,
    /// Publish the last attempt as `coherence: unproven`, with no
    /// revision, so it is never reused and the next refresh rebuilds it.
    PublishUnproven,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotDefinition {
    /// Stable name: the same name and definition resolve the same handle.
    pub name: String,
    pub sources: Vec<SnapshotSource>,
    #[serde(default)]
    pub references: Vec<SnapshotReference>,
    /// Materializations tried before giving up on coherence. Default 3.
    pub coherence_attempts: u32,
    pub incoherent: Incoherent,
}

impl SnapshotDefinition {
    pub fn new(name: impl Into<String>, sources: Vec<SnapshotSource>) -> Self {
        Self {
            name: name.into(),
            sources,
            references: vec![],
            coherence_attempts: 3,
            incoherent: Incoherent::Fail,
        }
    }

    pub fn reference(mut self, from: Collection, field: &str, to: Collection) -> Self {
        self.references.push(SnapshotReference {
            from,
            field: field.into(),
            to,
            immutable: false,
        });
        self
    }

    pub fn immutable_reference(mut self, from: Collection, field: &str, to: Collection) -> Self {
        self.references.push(SnapshotReference {
            from,
            field: field.into(),
            to,
            immutable: true,
        });
        self
    }

    pub fn on_incoherent(mut self, incoherent: Incoherent) -> Self {
        self.incoherent = incoherent;
        self
    }

    fn validate(&self) -> Result<(), StateError> {
        if self.name.trim().is_empty() {
            return Err(StateError::Invalid("a snapshot needs a name".into()));
        }
        if self.sources.is_empty() {
            return Err(StateError::Invalid(format!(
                "snapshot {} has no sources; a snapshot is bounded by the sources it names",
                self.name
            )));
        }
        let mut seen = BTreeSet::new();
        for source in &self.sources {
            if !seen.insert(source.collection) {
                return Err(StateError::Invalid(format!(
                    "snapshot {} names {} twice",
                    self.name,
                    source.collection.name()
                )));
            }
        }
        for reference in &self.references {
            if !seen.contains(&reference.from) {
                return Err(StateError::Invalid(format!(
                    "snapshot {} references {} from {}, which it does not hold yet",
                    self.name,
                    reference.to.name(),
                    reference.from.name()
                )));
            }
            seen.insert(reference.to);
        }
        Ok(())
    }

    /// SHA-256 of the definition's sources, filters, and references: what
    /// makes two definitions the same snapshot.
    pub fn digest(&self) -> String {
        let mut sources = self
            .sources
            .iter()
            .map(|source| {
                serde_json::json!({
                    "collection": source.collection.name(),
                    "where": source.filters,
                    "indexes": [],
                })
            })
            .collect::<Vec<_>>();
        sources.sort_by_key(|source| source["collection"].as_str().unwrap_or_default().to_owned());
        sha256_hex(&canonical_json(&serde_json::json!({
            "name": self.name,
            "sources": sources,
            "references": self.references,
        })))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Validation {
    /// Identified by an authoritative revision.
    Revision,
    /// No revision identifies it; only a re-read can tell if it is current.
    Refresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coherence {
    /// The revision was unchanged across the whole materialization.
    Proven,
    /// The materialization may span concurrent writes.
    Unproven,
}

/// What durable state a snapshot represents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotBasis {
    pub validation: Validation,
    pub revision: Option<Revision>,
    pub reason: Option<String>,
    pub coherence: Coherence,
    /// Reporting only; never a freshness signal.
    pub materialized_at: DateTime<Utc>,
    /// Materializations it took.
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotIdentity {
    /// `snap_` + SHA-256 over the fields below. Deterministic across
    /// handles and processes.
    pub id: String,
    pub name: String,
    pub version: Option<u64>,
    pub scope: Option<String>,
    /// Publication counter of the handle; not part of `id`.
    pub generation: u64,
    pub source_set: Vec<String>,
    pub definition_digest: String,
    /// The authority the materialization read under.
    pub authority_context: String,
    /// Present only without a revision: then the content is the identity.
    pub content_digest: Option<String>,
    /// `revision` or `content`.
    pub derivation: String,
}

/// A published snapshot. Immutable; reading it performs no I/O.
#[derive(Debug)]
pub struct Snapshot {
    pub identity: SnapshotIdentity,
    pub basis: SnapshotBasis,
    records: BTreeMap<Collection, BTreeMap<String, Record>>,
    /// Durable requests this build made.
    pub durable_reads: u64,
    pub build_ms: f64,
}

impl Snapshot {
    pub fn get(&self, collection: Collection, id: &str) -> Option<&Record> {
        self.records.get(&collection)?.get(id)
    }

    pub fn all(&self, collection: Collection) -> impl Iterator<Item = &Record> {
        self.records
            .get(&collection)
            .into_iter()
            .flatten()
            .map(|(_, record)| record)
    }

    /// A bounded query over materialized state, with the store's semantics.
    pub fn query(&self, query: &Query) -> Vec<Record> {
        query.evaluate(self.all(query.collection).cloned())
    }

    /// Every materialized record of `T`'s collection, decoded.
    pub fn typed<T: Document>(&self) -> Result<Vec<Stored<T>>, StateError> {
        self.all(T::COLLECTION)
            .cloned()
            .map(crate::control::decode)
            .collect()
    }

    pub fn records(&self) -> usize {
        self.records.values().map(BTreeMap::len).sum()
    }

    pub fn holds(&self, collection: Collection) -> bool {
        self.records.contains_key(&collection)
    }
}

/// Whether a published snapshot still represents current durable state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotStaleness {
    /// `unknown` means currency cannot be proven, not that it is current.
    pub state: String,
    pub snapshot: Option<Revision>,
    pub current: Option<Revision>,
    pub reason: Option<String>,
}

/// What a handle holds and has done.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SnapshotReport {
    pub name: String,
    pub definition_digest: String,
    pub authority_context: String,
    pub generation: u64,
    /// Builds that reached publication.
    pub builds: u64,
    /// Builds that failed; a failed build never replaces the active one.
    pub failures: u64,
    /// Refreshes answered by the active snapshot: nothing but the revision
    /// was read.
    pub reused: u64,
    /// Builds published without proven coherence.
    pub unproven: u64,
    /// Durable requests across every build and reuse check.
    pub durable_reads: u64,
    pub last_failure: Option<String>,
    pub active: Option<ActiveSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveSnapshot {
    pub identity: SnapshotIdentity,
    pub basis: SnapshotBasis,
    pub records: usize,
    pub build_ms: f64,
}

#[derive(Default)]
struct HandleState {
    active: Option<Arc<Snapshot>>,
    report: SnapshotReport,
}

/// The lifecycle owner: build, validate coherence, publish, reuse, replace.
pub struct SnapshotHandle {
    definition: SnapshotDefinition,
    digest: String,
    authority_context: String,
    store: Arc<dyn StateStore>,
    state: std::sync::Mutex<HandleState>,
    /// Single flight: concurrent refreshes produce one build.
    building: tokio::sync::Mutex<()>,
}

/// Whether a refresh published a new snapshot or reused the active one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refreshed {
    Built,
    Reused,
}

impl SnapshotHandle {
    pub fn new(
        definition: SnapshotDefinition,
        store: Arc<dyn StateStore>,
    ) -> Result<Self, StateError> {
        definition.validate()?;
        let backend = store.backend();
        let authority_context = format!("{}:{}", backend.kind, backend.location);
        let digest = definition.digest();
        Ok(Self {
            state: std::sync::Mutex::new(HandleState {
                active: None,
                report: SnapshotReport {
                    name: definition.name.clone(),
                    definition_digest: digest.clone(),
                    authority_context: authority_context.clone(),
                    ..SnapshotReport::default()
                },
            }),
            definition,
            digest,
            authority_context,
            store,
            building: tokio::sync::Mutex::new(()),
        })
    }

    pub fn definition(&self) -> &SnapshotDefinition {
        &self.definition
    }

    /// The active snapshot, if one was published. Never performs I/O.
    pub fn current(&self) -> Option<Arc<Snapshot>> {
        self.state.lock().expect("snapshot").active.clone()
    }

    pub fn report(&self) -> SnapshotReport {
        self.state.lock().expect("snapshot").report.clone()
    }

    /// Drop the active snapshot: the next refresh builds from durable
    /// state, whatever the revision says. Used after an outage, so nothing
    /// derived before it is carried across it.
    pub fn invalidate(&self) {
        self.state.lock().expect("snapshot").active = None;
    }

    /// Whether the active snapshot still represents current durable state.
    pub async fn staleness(&self) -> Result<SnapshotStaleness, StateError> {
        let active = self.current();
        let snapshot = active
            .as_ref()
            .and_then(|active| active.basis.revision.clone());
        let Some(snapshot) = snapshot else {
            return Ok(SnapshotStaleness {
                state: "unknown".into(),
                snapshot: None,
                current: None,
                reason: Some(match active {
                    None => "nothing is published".into(),
                    Some(_) => "the active snapshot has no revision".into(),
                }),
            });
        };
        let current = self.read_revision().await?;
        Ok(SnapshotStaleness {
            state: if current.as_ref() == Some(&snapshot) {
                "current"
            } else {
                "stale"
            }
            .into(),
            snapshot: Some(snapshot),
            current,
            reason: None,
        })
    }

    async fn read_revision(&self) -> Result<Option<Revision>, StateError> {
        let revision = self.store.revision().await;
        self.state.lock().expect("snapshot").report.durable_reads += 1;
        revision
    }

    /// Build and publish a snapshot, or return the active one when the
    /// authoritative revision has not moved since it was built (`force`
    /// skips that check). A failed build leaves the active one in place.
    pub async fn refresh(&self, force: bool) -> Result<(Arc<Snapshot>, Refreshed), StateError> {
        let _single = self.building.lock().await;
        let result = self.build(force).await;
        let mut state = self.state.lock().expect("snapshot");
        match result {
            Ok((snapshot, Refreshed::Reused)) => {
                state.report.reused += 1;
                Ok((snapshot, Refreshed::Reused))
            }
            Ok((snapshot, Refreshed::Built)) => {
                state.report.builds += 1;
                state.report.generation = snapshot.identity.generation;
                if snapshot.basis.coherence == Coherence::Unproven {
                    state.report.unproven += 1;
                }
                state.report.active = Some(ActiveSnapshot {
                    identity: snapshot.identity.clone(),
                    basis: snapshot.basis.clone(),
                    records: snapshot.records(),
                    build_ms: snapshot.build_ms,
                });
                state.active = Some(snapshot.clone());
                Ok((snapshot, Refreshed::Built))
            }
            Err(error) => {
                state.report.failures += 1;
                state.report.last_failure = Some(error.to_string());
                Err(error)
            }
        }
    }

    async fn build(&self, force: bool) -> Result<(Arc<Snapshot>, Refreshed), StateError> {
        let started = Instant::now();
        let previous = self.current();
        let mut before = self.read_revision().await?;
        let mut reads = 1;
        if !force
            && let (Some(previous), Some(current)) = (&previous, &before)
            && previous.basis.revision.as_ref() == Some(current)
        {
            return Ok((previous.clone(), Refreshed::Reused));
        }
        let generation = self.report().generation + 1;
        let Some(_) = before else {
            let (records, count) = self.materialize(previous.as_deref()).await?;
            reads += count;
            let basis = SnapshotBasis {
                validation: Validation::Refresh,
                revision: None,
                reason: Some(format!(
                    "{} does not report an authoritative revision",
                    self.store.backend().kind
                )),
                coherence: Coherence::Unproven,
                materialized_at: Utc::now(),
                attempts: 1,
            };
            return Ok((
                self.publish(records, basis, generation, reads, started),
                Refreshed::Built,
            ));
        };
        let attempts = self.definition.coherence_attempts.max(1);
        let mut last = None;
        for attempt in 1..=attempts {
            let (records, count) = self.materialize(previous.as_deref()).await?;
            let after = self.read_revision().await?;
            reads += count + 1;
            if after == before {
                let basis = SnapshotBasis {
                    validation: Validation::Revision,
                    revision: before,
                    reason: None,
                    coherence: Coherence::Proven,
                    materialized_at: Utc::now(),
                    attempts: attempt,
                };
                return Ok((
                    self.publish(records, basis, generation, reads, started),
                    Refreshed::Built,
                ));
            }
            last = Some(records);
            before = after;
        }
        match self.definition.incoherent {
            Incoherent::Fail => Err(StateError::Unavailable(format!(
                "durable state changed during each of {attempts} materialization attempts for snapshot {}",
                self.definition.name
            ))),
            Incoherent::PublishUnproven => {
                let basis = SnapshotBasis {
                    validation: Validation::Refresh,
                    revision: None,
                    reason: Some(format!(
                        "durable state changed during each of {attempts} materialization attempts"
                    )),
                    coherence: Coherence::Unproven,
                    materialized_at: Utc::now(),
                    attempts,
                };
                let records = last.expect("at least one attempt");
                Ok((
                    self.publish(records, basis, generation, reads, started),
                    Refreshed::Built,
                ))
            }
        }
    }

    /// Read the sources concurrently, then each reference by identity.
    async fn materialize(
        &self,
        previous: Option<&Snapshot>,
    ) -> Result<(BTreeMap<Collection, BTreeMap<String, Record>>, u64), StateError> {
        let mut reads = 0;
        let mut queries = tokio::task::JoinSet::new();
        for source in &self.definition.sources {
            let store = self.store.clone();
            let query = Query {
                collection: source.collection,
                filters: source.filters.clone(),
                order_by: None,
                limit: None,
            };
            queries.spawn(async move { (query.collection, store.query(&query).await) });
            reads += 1;
        }
        let mut records: BTreeMap<Collection, BTreeMap<String, Record>> = BTreeMap::new();
        while let Some(joined) = queries.join_next().await {
            let (collection, result) =
                joined.map_err(|error| StateError::Unavailable(error.to_string()))?;
            let table = records.entry(collection).or_default();
            for record in result? {
                // The source's filter is applied again: a backend that
                // over-returns cannot widen a snapshot.
                if self
                    .definition
                    .sources
                    .iter()
                    .find(|source| source.collection == collection)
                    .is_some_and(|source| source.filters.iter().all(|f| f.matches_record(&record)))
                {
                    table.insert(record.id.clone(), record);
                }
            }
        }
        for reference in &self.definition.references {
            let named = records
                .get(&reference.from)
                .into_iter()
                .flatten()
                .filter_map(|(_, record)| record.value.get(&reference.field))
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<BTreeSet<_>>();
            let table = records.entry(reference.to).or_default();
            let mut wanted = vec![];
            for id in named {
                if table.contains_key(&id) {
                    continue;
                }
                if reference.immutable
                    && let Some(known) =
                        previous.and_then(|previous| previous.get(reference.to, &id))
                {
                    table.insert(id, known.clone());
                    continue;
                }
                wanted.push(id);
            }
            let mut gets = tokio::task::JoinSet::new();
            for chunk in wanted.chunks(IDS_PER_READ) {
                let store = self.store.clone();
                let chunk = chunk.to_vec();
                let collection = reference.to;
                gets.spawn(async move { store.get_many(collection, &chunk).await });
                reads += 1;
            }
            while let Some(joined) = gets.join_next().await {
                for record in
                    joined.map_err(|error| StateError::Unavailable(error.to_string()))??
                {
                    table.insert(record.id.clone(), record);
                }
            }
        }
        self.state.lock().expect("snapshot").report.durable_reads += reads;
        Ok((records, reads))
    }

    fn publish(
        &self,
        records: BTreeMap<Collection, BTreeMap<String, Record>>,
        basis: SnapshotBasis,
        generation: u64,
        reads: u64,
        started: Instant,
    ) -> Arc<Snapshot> {
        let mut source_set = records
            .keys()
            .map(|collection| collection.name().to_string())
            .collect::<Vec<_>>();
        source_set.sort();
        let content_digest = basis.revision.is_none().then(|| {
            let content = records
                .iter()
                .map(|(collection, table)| {
                    let mut rows = table
                        .values()
                        .map(|record| {
                            canonical_json(&serde_json::json!({
                                "id": record.id,
                                "value": record.value,
                            }))
                        })
                        .collect::<Vec<_>>();
                    rows.sort();
                    serde_json::json!([collection.name(), rows])
                })
                .collect::<Vec<_>>();
            sha256_hex(&canonical_json(&Value::Array(content)))
        });
        let basis_identity = match (&basis.revision, &content_digest) {
            (Some(revision), _) => serde_json::json!(["revision", revision.scope, revision.value]),
            (None, Some(content)) => serde_json::json!(["content", content]),
            (None, None) => unreachable!("a snapshot without a revision has a content digest"),
        };
        let id = format!(
            "snap_{}",
            sha256_hex(&canonical_json(&serde_json::json!([
                "compute.snapshot.v1",
                self.definition.name,
                self.digest,
                self.authority_context,
                basis_identity,
            ])))
        );
        let identity = SnapshotIdentity {
            id,
            name: self.definition.name.clone(),
            version: basis.revision.as_ref().map(|revision| revision.value),
            scope: basis
                .revision
                .as_ref()
                .map(|revision| revision.scope.clone()),
            generation,
            source_set,
            definition_digest: self.digest.clone(),
            authority_context: self.authority_context.clone(),
            derivation: if content_digest.is_some() {
                "content"
            } else {
                "revision"
            }
            .into(),
            content_digest,
        };
        Arc::new(Snapshot {
            identity,
            basis,
            records,
            durable_reads: reads,
            build_ms: started.elapsed().as_secs_f64() * 1000.0,
        })
    }
}

/// Snapshot handles by name: requesting the same name and definition
/// resolves the same handle, and so its published snapshot.
#[derive(Default)]
pub struct SnapshotRegistry {
    handles: std::sync::Mutex<HashMap<String, Arc<SnapshotHandle>>>,
}

impl SnapshotRegistry {
    pub fn resolve(
        &self,
        definition: SnapshotDefinition,
        store: &Arc<dyn StateStore>,
    ) -> Result<Arc<SnapshotHandle>, StateError> {
        let mut handles = self.handles.lock().expect("snapshots");
        if let Some(handle) = handles.get(&definition.name) {
            if handle.digest == definition.digest() {
                return Ok(handle.clone());
            }
            return Err(StateError::Invalid(format!(
                "snapshot {} is already defined differently; a name means one definition",
                definition.name
            )));
        }
        let handle = Arc::new(SnapshotHandle::new(definition, store.clone())?);
        handles.insert(handle.definition.name.clone(), handle.clone());
        Ok(handle)
    }

    pub fn reports(&self) -> Vec<SnapshotReport> {
        let mut reports = self
            .handles
            .lock()
            .expect("snapshots")
            .values()
            .map(|handle| handle.report())
            .collect::<Vec<_>>();
        reports.sort_by(|left, right| left.name.cmp(&right.name));
        reports
    }

    pub fn invalidate_all(&self) {
        for handle in self.handles.lock().expect("snapshots").values() {
            handle.invalidate();
        }
    }
}

/// JSON with object keys sorted at every depth, as FeltDB's
/// `canonicalJson`.
pub fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            let entries = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("a string"),
                        canonical_json(&map[key])
                    )
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", entries.join(","))
        }
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => serde_json::to_string(other).expect("a scalar"),
    }
}

pub fn sha256_hex(input: &str) -> String {
    Sha256::digest(input.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
