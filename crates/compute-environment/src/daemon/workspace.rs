//! Workspace state as a Compute capability: export, seed, verify.
//!
//! ```text
//! computer ──export──▶ workspace archive + digest
//! workspace archive ──seed──▶ computer            (proves it landed)
//! computer ──verify──▶ digest ≟ expected          (verified / mismatch)
//! ```
//!
//! Each is a durable job (or a few) on the computer's target, with the
//! ordinary job and execution evidence, and an `environment.command` event.
//! None of them writes a record: a workspace is a property of a computer,
//! described by a digest, not a stored object. What a workspace *is* — and so
//! what "the same workspace" means — is the contract in
//! [`docs/workspace.md`](../../../../docs/workspace.md), summarized here.
//!
//! # Context independence
//!
//! Workspace state is context-independent. These operations take an
//! environment, an operator, and an archive or digest, and nothing else: they
//! do not know, and cannot be told, whether a computer is for development, a
//! demo, tests, CI, staging, production, a customer, or an AI workload. Those
//! are compositions and policies above Compute (`clone` is the first).
//!
//! # `compute.workspace@1`
//!
//! The workspace is the computer's working directory minus:
//!
//! * `repos/`: declared repositories are declared state, re-derived from
//!   their declared revision, never captured;
//! * controller state: everything under `.compute/` except
//!   `.compute/sources` (imported source repositories the declared contents
//!   refer to).
//!
//! Its digest is SHA-256 over this text:
//!
//! ```text
//! compute.workspace@1
//! dir <path>                 one line per *empty* directory, sorted
//! file <x|-> <sha256> <path> one line per regular file, sorted
//! ```
//!
//! Paths are relative, `/`-separated, sorted by bytes. `x` means the owner
//! execute bit is set. **Not part of the identity:** any other permission bit,
//! ownership, timestamps, extended attributes, non-empty directories (implied
//! by their files). **Unsupported, refused rather than approximated:**
//! symbolic links, hard-link entries, devices, sockets, pipes, and paths
//! holding control characters or backslashes.
//!
//! # Transport bound
//!
//! An archive travels through job output (export) and job environments
//! (seed), so it is bounded by [`WORKSPACE_ARCHIVE_LIMIT`]. Larger workspaces
//! are refused with that reason, not truncated. Lifting the bound is the job
//! of a streamed artifact capability, not of this one.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Component;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use compute_state::events;
use serde_json::json;

use super::computers::script;
use super::{Change, Daemon, Scope};
use crate::EnvironmentError;
use crate::model::*;

/// The identity contract this module implements.
pub const WORKSPACE_IDENTITY: &str = "compute.workspace@1";

/// The largest workspace archive the job transport carries, in bytes.
pub const WORKSPACE_ARCHIVE_LIMIT: usize = 8 * 1024 * 1024;

/// Shell shared by every workspace script. `workspace` lists the workspace
/// (see the module documentation); `workspace_digest` prints the text that is
/// hashed. [`identify`] builds the same text from an archive.
const FUNCTIONS: &str = r#"
sum() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
workspace() {
  find . -mindepth 1 \( -path ./repos -o \( -path './.compute/*' ! -path ./.compute/sources ! -path './.compute/sources/*' \) \) -prune -o "$@" -print
}
workspace_check() {
  bad="$(workspace ! -type f ! -type d | head -n 1)"
  if [ -n "$bad" ]; then echo "unsupported entry: $bad" >&2; return 3; fi
  bad="$(workspace \( -name '*[[:cntrl:]]*' -o -name '*\\*' \) | head -n 1)"
  if [ -n "$bad" ]; then echo "unsupported path: $bad" >&2; return 3; fi
}
workspace_digest() {
  echo compute.workspace@1
  workspace -type d -empty ! -path ./.compute | LC_ALL=C sort | while IFS= read -r d; do printf 'dir %s\n' "${d#./}"; done
  workspace -type f | LC_ALL=C sort | while IFS= read -r f; do
    x=-; if [ -n "$(find "$f" -maxdepth 0 -perm -u+x)" ]; then x=x; fi
    printf 'file %s %s %s\n' "$x" "$(sum "$f" | cut -d' ' -f1)" "${f#./}"
  done
}
digest() { workspace_digest | sum | cut -d' ' -f1; }
"#;

/// Export. Argument: the archive limit. Prints the archive as one base64
/// line, the archive's own digest, and the workspace digest. The workspace
/// digest is taken before *and after* the archive is made: a workspace that
/// changed in between is refused, whatever `tar` noticed.
const EXPORT_TAIL: &str = r#"
workspace_check
before="$(digest)"
tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT
tar -cf "$tmp" --exclude=./repos --exclude=./.compute/processes --exclude=./.compute/imports .
size="$(wc -c <"$tmp" | tr -d ' ')"
if [ "$size" -gt "$1" ]; then echo "the workspace archive is $size bytes; the transport carries $1" >&2; exit 5; fi
after="$(digest)"
if [ "$before" != "$after" ]; then echo "the workspace changed while it was captured" >&2; exit 4; fi
base64 <"$tmp" | tr -d '\n'; echo
sum <"$tmp" | cut -d' ' -f1
echo "$before"
"#;

/// Verify: print the workspace digest.
const DIGEST_TAIL: &str = r#"
workspace_check
digest
"#;

/// Seed. Arguments: import ID, archive digest. The workspace must hold
/// nothing (`.compute` aside), so everything extracted is ours; a failed
/// extraction removes exactly that, and nothing is removed before it starts.
const SEED_TAIL: &str = r#"
dir=".compute/imports/$1"
ok=1
cleanup() {
  rm -rf "$dir"
  if [ "$ok" != 1 ]; then
    find . -mindepth 1 -maxdepth 1 ! -name .compute -exec rm -rf {} +
    rm -rf .compute/sources
  fi
}
trap cleanup EXIT
base64 -d <"$dir/source.b64" >"$dir/source.tar"
observed="$(sum "$dir/source.tar" | cut -d' ' -f1)"
if [ "sha256:$observed" != "$2" ]; then echo "the archive is sha256:$observed, not $2" >&2; exit 1; fi
if [ -n "$(find . -mindepth 1 -maxdepth 1 ! -name .compute)" ] || [ -e .compute/sources ]; then
  echo "the workspace already holds files; a seed needs an empty one" >&2; exit 1
fi
ok=0
tar -xf "$dir/source.tar" --exclude=./.compute/imports
ok=1
"#;

fn workspace_script(tail: &str) -> String {
    format!("set -eu\n{FUNCTIONS}{tail}")
}

/// What an archive holds, by the workspace identity contract.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceIdentity {
    /// `sha256:` of the identity text.
    pub digest: String,
    pub files: usize,
    /// Empty directories (the only ones the identity records).
    pub directories: usize,
    pub bytes: u64,
}

/// Validate an archive against the contract and compute its digest without
/// extracting it. Only regular files and directories are accepted, at safe
/// relative paths, holding nothing the workspace excludes.
pub(crate) fn identify(archive: &[u8]) -> Result<WorkspaceIdentity, EnvironmentError> {
    let bad = |what: String| EnvironmentError::Invalid(format!("the workspace archive {what}"));
    let mut files = BTreeMap::<String, (bool, String)>::new();
    let mut directories = BTreeSet::<String>::new();
    let mut bytes = 0u64;
    let mut tar = tar::Archive::new(archive);
    for entry in tar
        .entries()
        .map_err(|error| bad(format!("is unreadable: {error}")))?
    {
        let mut entry = entry.map_err(|error| bad(format!("is unreadable: {error}")))?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(bad(format!(
                "holds something other than files and directories ({kind:?})"
            )));
        }
        let path = entry
            .path()
            .map_err(|error| bad(format!("has an unreadable path: {error}")))?
            .into_owned();
        let mut parts = vec![];
        for component in path.components() {
            match component {
                Component::CurDir if parts.is_empty() => {}
                Component::Normal(part) => match part.to_str() {
                    Some(part) if !part.contains(|c: char| c.is_control() || c == '\\') => {
                        parts.push(part.to_owned())
                    }
                    _ => return Err(bad(format!("has an unsupported path: {}", path.display()))),
                },
                _ => return Err(bad(format!("has an unsafe path: {}", path.display()))),
            }
        }
        let allowed = match parts.first().map(String::as_str) {
            Some("repos") => false,
            Some(".compute") => {
                parts.len() == 1 || parts.get(1).map(String::as_str) == Some("sources")
            }
            _ => true,
        };
        if !allowed {
            return Err(bad(format!(
                "holds controller or re-derived state: {}",
                path.display()
            )));
        }
        let name = parts.join("/");
        if kind.is_dir() {
            if !parts.is_empty() && name != ".compute" {
                directories.insert(name);
            }
            continue;
        }
        let executable = entry.header().mode().unwrap_or(0) & 0o100 != 0;
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|error| bad(format!("is truncated: {error}")))?;
        bytes += data.len() as u64;
        let digest = compute_core::sha256_identity(&data);
        if files
            .insert(
                name.clone(),
                (executable, digest.trim_start_matches("sha256:").to_owned()),
            )
            .is_some()
        {
            return Err(bad(format!("holds {name} twice")));
        }
    }
    // A directory is recorded only when it is empty: one that holds anything
    // is implied by what it holds.
    let mut parents = BTreeSet::<&str>::new();
    for name in files
        .keys()
        .map(String::as_str)
        .chain(directories.iter().map(String::as_str))
    {
        let mut rest = name;
        while let Some((parent, _)) = rest.rsplit_once('/') {
            parents.insert(parent);
            rest = parent;
        }
    }
    let empty = directories
        .iter()
        .filter(|name| !parents.contains(name.as_str()))
        .collect::<Vec<_>>();
    let mut text = format!("{WORKSPACE_IDENTITY}\n");
    for name in &empty {
        text.push_str(&format!("dir {name}\n"));
    }
    for (name, (executable, digest)) in &files {
        text.push_str(&format!(
            "file {} {digest} {name}\n",
            if *executable { 'x' } else { '-' }
        ));
    }
    Ok(WorkspaceIdentity {
        digest: compute_core::sha256_identity(text.as_bytes()),
        files: files.len(),
        directories: empty.len(),
        bytes,
    })
}

impl Daemon {
    /// Read the workspace of an environment's running computer as an archive,
    /// with its digest. Fails closed: unsupported entries, a workspace that
    /// changed while it was captured, an archive over the transport bound, or
    /// one that does not match the digest the computer computed.
    pub async fn export_workspace(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
    ) -> Result<WorkspaceExport, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        let (_, client, session_id) = self.running(&record).await?;
        let (evidence, output) = self
            .run_in_computer_command(
                &client,
                &session_id,
                script(
                    &workspace_script(EXPORT_TAIL),
                    [WORKSPACE_ARCHIVE_LIMIT.to_string()],
                ),
                Duration::from_secs(300),
            )
            .await;
        let failed = |why: String| {
            EnvironmentError::RuntimeUnavailable(format!(
                "exporting the workspace of {environment} failed (job {}): {why}",
                evidence.job_id
            ))
        };
        if evidence.outcome != "succeeded" {
            return Err(failed(evidence.error.clone().unwrap_or_default()));
        }
        let mut lines = output.lines();
        let (Some(encoded), Some(archive_digest), Some(observed), None) =
            (lines.next(), lines.next(), lines.next(), lines.next())
        else {
            return Err(failed("the output was cut short".into()));
        };
        let archive = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|error| failed(format!("the archive did not decode: {error}")))?;
        if compute_core::sha256_identity(&archive) != format!("sha256:{}", archive_digest.trim()) {
            return Err(failed("the archive does not match its digest".into()));
        }
        let identity = identify(&archive)?;
        if identity.digest != format!("sha256:{}", observed.trim()) {
            return Err(failed(
                "the archive does not describe the workspace the computer measured".into(),
            ));
        }
        let export = WorkspaceExport {
            identity: WORKSPACE_IDENTITY.into(),
            digest: identity.digest,
            archive_digest: compute_core::sha256_identity(&archive),
            files: identity.files,
            directories: identity.directories,
            bytes: identity.bytes,
            job_id: evidence.job_id.clone(),
            archive,
        };
        self.workspace_event(
            &record.id,
            environment,
            operator,
            "workspace.export",
            json!({ "digest": export.digest, "archive": export.archive_digest,
                    "files": export.files, "job_id": export.job_id }),
        )
        .await?;
        Ok(export)
    }

    /// Place a workspace archive into an environment's running computer,
    /// whose workspace must be empty, and prove it landed: the archive is
    /// validated and its digest computed here (and checked against the
    /// caller's expectation, if any) before anything is sent; a job extracts
    /// it; another recomputes the digest inside the computer, which must
    /// match. A failed extraction removes what it wrote.
    pub async fn seed_workspace(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        request: WorkspaceSeedRequest,
    ) -> Result<WorkspaceSeed, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        if request.archive.len() > WORKSPACE_ARCHIVE_LIMIT {
            return Err(EnvironmentError::Invalid(format!(
                "the workspace archive is {} bytes; the transport carries {WORKSPACE_ARCHIVE_LIMIT}",
                request.archive.len()
            )));
        }
        let identity = identify(&request.archive)?;
        if let Some(expected) = &request.digest
            && *expected != identity.digest
        {
            return Err(EnvironmentError::Conflict(format!(
                "the archive holds workspace {}, not {expected}",
                identity.digest
            )));
        }
        let (_, client, session_id) = self.running(&record).await?;
        let (import, archive_digest, mut jobs) = self
            .upload_archive(
                &client,
                &session_id,
                &request.archive,
                &format!("seeding {environment}"),
            )
            .await?;
        let (evidence, _) = self
            .run_in_computer_command(
                &client,
                &session_id,
                script(&workspace_script(SEED_TAIL), [import, archive_digest]),
                Duration::from_secs(300),
            )
            .await;
        jobs.push(evidence.job_id.clone());
        if evidence.outcome != "succeeded" {
            return Err(EnvironmentError::RuntimeUnavailable(format!(
                "seeding {environment} failed (job {}): {}",
                evidence.job_id,
                evidence.error.unwrap_or_default()
            )));
        }
        let verification = self.measure(&client, &session_id, environment).await?;
        jobs.push(verification.1.clone());
        if verification.0 != identity.digest {
            return Err(EnvironmentError::Conflict(format!(
                "the seeded workspace of {environment} is {}, not {} (job {})",
                verification.0, identity.digest, verification.1
            )));
        }
        let seed = WorkspaceSeed {
            digest: identity.digest,
            files: identity.files,
            directories: identity.directories,
            verified: true,
            jobs,
        };
        self.workspace_event(
            &record.id,
            environment,
            operator,
            "workspace.seed",
            json!({ "digest": seed.digest, "files": seed.files, "jobs": seed.jobs }),
        )
        .await?;
        Ok(seed)
    }

    /// Compute the workspace digest of an environment's running computer and
    /// compare it with an expected one. A mismatch is a result, not an error.
    pub async fn verify_workspace(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        request: WorkspaceVerifyRequest,
    ) -> Result<WorkspaceVerification, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        let (_, client, session_id) = self.running(&record).await?;
        let (observed, job_id) = self.measure(&client, &session_id, environment).await?;
        let verification = WorkspaceVerification {
            verified: request.digest.as_deref() == Some(observed.as_str()),
            digest: observed,
            expected: request.digest,
            job_id,
        };
        self.workspace_event(
            &record.id,
            environment,
            operator,
            "workspace.verify",
            json!({ "digest": verification.digest, "expected": verification.expected,
                    "verified": verification.verified, "job_id": verification.job_id }),
        )
        .await?;
        Ok(verification)
    }

    /// The workspace digest inside a computer, and the job that measured it.
    async fn measure(
        &self,
        client: &compute_provider::RemoteProvider,
        session_id: &str,
        environment: &str,
    ) -> Result<(String, String), EnvironmentError> {
        let (evidence, output) = self
            .run_in_computer_command(
                client,
                session_id,
                script(&workspace_script(DIGEST_TAIL), []),
                Duration::from_secs(300),
            )
            .await;
        if evidence.outcome != "succeeded" {
            return Err(EnvironmentError::RuntimeUnavailable(format!(
                "measuring the workspace of {environment} failed (job {}): {}",
                evidence.job_id,
                evidence.error.unwrap_or_default()
            )));
        }
        Ok((format!("sha256:{}", output.trim()), evidence.job_id))
    }

    async fn workspace_event(
        &self,
        environment_id: &str,
        environment: &str,
        operator: &str,
        command: &str,
        mut data: serde_json::Value,
    ) -> Result<(), EnvironmentError> {
        if let Some(data) = data.as_object_mut() {
            data.insert("environment_id".into(), environment_id.into());
            data.insert("command".into(), command.into());
        }
        let change = self.event(
            Change::new(),
            events::ENVIRONMENT_COMMAND,
            Scope::environment(environment),
            format!("{operator}: {command} {environment}"),
            data,
        );
        self.apply(change).await
    }

    /// A composition that failed after creating something must not leave it
    /// looking ready. Stop the new environment (Reality then says stopped),
    /// record the failure with its phase and that the workspace is
    /// unverified, and return the error to give the caller. There is no
    /// rollback: the environment stays, inert. Any composition over the
    /// workspace primitives uses this (`composition` names it).
    pub(crate) async fn abandon_composition(
        self: &Arc<Self>,
        composition: &str,
        source: &str,
        name: &str,
        operator: &str,
        phase: &str,
        error: &EnvironmentError,
    ) -> EnvironmentError {
        let stopped = self
            .set_environment_state(name, DesiredState::Stopped, false)
            .await
            .is_ok();
        if let Ok(record) = self.owned_environment(name, operator).await {
            let change = self.event(
                Change::new(),
                events::ENVIRONMENT_COMMAND,
                Scope::environment(name),
                format!("{composition} of {source} into {name} failed while {phase}"),
                json!({
                    "environment_id": record.id, "command": composition, "source": source,
                    "outcome": "failed", "phase": phase, "workspace_verified": false,
                    "stopped": stopped, "error": error.to_string(),
                }),
            );
            let _ = self.apply(change).await;
        }
        EnvironmentError::Conflict(format!(
            "{composition} of {source} into {name} failed while {phase}; {name} exists, is stopped, and its workspace is unverified: {error}"
        ))
    }
}
