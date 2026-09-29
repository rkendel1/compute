//! Clone an environment: the composition that proves the primitives.
//!
//! ```text
//! clone SOURCE NAME
//!   export   run a durable job in SOURCE's computer: tar its workspace
//!   verify   recompute the tree digest from the archive, here
//!   create   a new environment, the same requirements and policy, no contents
//!   seed     upload the archive; a durable job in the new computer extracts it
//!            and recomputes the tree digest; it must match
//!   start    apply SOURCE's declared contents (repositories, processes) to the
//!            new environment: the ordinary reconciler does the rest
//!   verify   the new computer converged; report the commits on both sides
//! ```
//!
//! Almost everything here is an existing primitive: environments, placement,
//! computers, durable jobs and their evidence, the archive transport that
//! `import_source` already used, contents, and the reconciler. What was
//! missing, and is added here as generic operations, is reading a tree *out*
//! of a computer ([`Daemon::export_workspace`]), placing a tree *into* an
//! empty workspace ([`Daemon::seed_workspace`]), and a digest of a tree that
//! both sides compute the same way. `clone_environment` is only the order in
//! which they are used.
//!
//! What a clone carries: the workspace's files, except `repos/` (declared
//! repositories are re-derived from the declared contents, at the same
//! revision) and the controller's own process state. What it does not carry:
//! anything not in the workspace's files (memory, processes, the machine),
//! symbolic links and special files (refused), configuration *values* unless
//! asked, and executable bits in the digest (the archive keeps them; the
//! digest does not cover them).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Component;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use compute_core::ComputerStatus;
use compute_state::events;
use serde_json::json;

use super::computers::script;
use super::{Change, Daemon, Scope};
use crate::EnvironmentError;
use crate::model::*;

/// Tar the workspace, then print the archive as one base64 line and the
/// archive's own digest as a second line (so a cut-short output is
/// detected, never mistaken for a smaller archive).
const EXPORT_WORKSPACE: &str = r#"set -eu
tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT
tar -cf "$tmp" --exclude=./repos --exclude=./.compute/processes --exclude=./.compute/imports .
base64 <"$tmp" | tr -d '\n'; echo
if command -v sha256sum >/dev/null 2>&1; then sha256sum <"$tmp"; else shasum -a 256 <"$tmp"; fi | cut -d' ' -f1
"#;

/// The tree digest: SHA-256 over `"<sha256>  ./<path>\n"` for every regular
/// file, sorted by path in byte order. [`summarize`] computes the same value
/// from an archive.
const TREE_DIGEST: &str = r#"sum() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
find . -type f ! -path './repos/*' ! -path './.compute/processes/*' ! -path './.compute/imports/*' \
  | LC_ALL=C sort | while IFS= read -r f; do sum "$f"; done | sum | cut -d' ' -f1
"#;

/// Extract an uploaded archive into a workspace that holds nothing but the
/// controller's directory. Arguments: import ID, archive digest.
const SEED_WORKSPACE: &str = r#"set -eu
dir=".compute/imports/$1"
trap 'rm -rf "$dir"' EXIT
base64 -d <"$dir/source.b64" >"$dir/source.tar"
if command -v sha256sum >/dev/null 2>&1; then
  digest="$(sha256sum "$dir/source.tar" | cut -d' ' -f1)"
else
  digest="$(shasum -a 256 "$dir/source.tar" | cut -d' ' -f1)"
fi
if [ "sha256:$digest" != "$2" ]; then echo "the archive is sha256:$digest, not $2" >&2; exit 1; fi
if [ -n "$(find . -mindepth 1 -maxdepth 1 ! -name .compute)" ]; then
  echo "the workspace already holds files; a seed needs an empty one" >&2; exit 1
fi
tar -xf "$dir/source.tar" --exclude=./.compute/imports
"#;

/// What an archive holds, as far as a clone cares.
pub(crate) struct ArchiveSummary {
    /// `sha256:` of the tree digest text (see [`TREE_DIGEST`]).
    pub tree_digest: String,
    pub files: usize,
    pub bytes: u64,
}

/// Validate a workspace archive and compute its tree digest, without
/// extracting it. Only regular files and directories are accepted, at safe
/// relative paths, and nothing that belongs to the controller (`repos/`,
/// `.compute/` except `.compute/sources`). Anything else is refused.
pub(crate) fn summarize(archive: &[u8]) -> Result<ArchiveSummary, EnvironmentError> {
    let bad = |what: String| EnvironmentError::Invalid(format!("the workspace archive {what}"));
    let mut files = BTreeMap::<String, String>::new();
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
                    Some(part) if !part.contains(['\n', '\\']) => parts.push(part.to_owned()),
                    _ => return Err(bad(format!("has an unsupported path: {}", path.display()))),
                },
                _ => return Err(bad(format!("has an unsafe path: {}", path.display()))),
            }
        }
        let first = parts.first().map(String::as_str);
        let allowed = match first {
            None => true,
            Some("repos") => false,
            Some(".compute") => {
                parts.len() == 1 || parts.get(1).map(String::as_str) == Some("sources")
            }
            Some(_) => true,
        };
        if !allowed {
            return Err(bad(format!(
                "holds controller or re-derived state: {}",
                path.display()
            )));
        }
        if kind.is_dir() {
            continue;
        }
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|error| bad(format!("is truncated: {error}")))?;
        bytes += data.len() as u64;
        let digest = compute_core::sha256_identity(&data);
        let name = format!("./{}", parts.join("/"));
        if files
            .insert(
                name.clone(),
                digest.trim_start_matches("sha256:").to_owned(),
            )
            .is_some()
        {
            return Err(bad(format!("holds {name} twice")));
        }
    }
    let text = files
        .iter()
        .map(|(path, digest)| format!("{digest}  {path}\n"))
        .collect::<String>();
    Ok(ArchiveSummary {
        tree_digest: compute_core::sha256_identity(text.as_bytes()),
        files: files.len(),
        bytes,
    })
}

/// A workspace read out of a computer.
pub(crate) struct WorkspaceArchive {
    pub bytes: Vec<u8>,
    pub summary: ArchiveSummary,
    pub job_id: String,
}

impl Daemon {
    /// Read the workspace of an environment's running computer as a verified
    /// tar archive, through one durable job. The archive is checked against
    /// the digest the computer printed for it, then validated.
    pub(crate) async fn export_workspace(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
    ) -> Result<WorkspaceArchive, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        let (_, client, session_id) = self.running(&record).await?;
        let (evidence, output) = self
            .run_in_computer_command(
                &client,
                &session_id,
                script(EXPORT_WORKSPACE, []),
                Duration::from_secs(300),
            )
            .await;
        let failed = |why: String| {
            EnvironmentError::RuntimeUnavailable(format!(
                "exporting {environment} failed (job {}): {why}",
                evidence.job_id
            ))
        };
        if evidence.outcome != "succeeded" {
            return Err(failed(evidence.error.clone().unwrap_or_default()));
        }
        let mut lines = output.lines();
        let (Some(encoded), Some(digest), None) = (lines.next(), lines.next(), lines.next()) else {
            return Err(failed("the output was cut short".into()));
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|error| failed(format!("the archive did not decode: {error}")))?;
        if compute_core::sha256_identity(&bytes) != format!("sha256:{}", digest.trim()) {
            return Err(failed("the archive does not match its digest".into()));
        }
        let summary = summarize(&bytes)?;
        Ok(WorkspaceArchive {
            bytes,
            summary,
            job_id: evidence.job_id.clone(),
        })
    }

    /// Place a workspace archive into an environment's running computer,
    /// whose workspace must be empty, and prove it landed: a durable job
    /// extracts it, and another recomputes the tree digest inside the
    /// computer, which must equal `tree_digest`. Returns the jobs.
    pub(crate) async fn seed_workspace(
        self: &Arc<Self>,
        environment: &str,
        operator: &str,
        archive: &[u8],
        tree_digest: &str,
    ) -> Result<Vec<String>, EnvironmentError> {
        let record = self.owned_environment(environment, operator).await?;
        let (_, client, session_id) = self.running(&record).await?;
        let (import, digest, mut jobs) = self
            .upload_archive(
                &client,
                &session_id,
                archive,
                &format!("seeding {environment}"),
            )
            .await?;
        let (evidence, _) = self
            .run_in_computer_command(
                &client,
                &session_id,
                script(SEED_WORKSPACE, [import, digest]),
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
        let (evidence, output) = self
            .run_in_computer_command(
                &client,
                &session_id,
                script(TREE_DIGEST, []),
                Duration::from_secs(300),
            )
            .await;
        jobs.push(evidence.job_id.clone());
        let observed = format!("sha256:{}", output.trim());
        if evidence.outcome != "succeeded" || observed != tree_digest {
            return Err(EnvironmentError::Conflict(format!(
                "the seeded workspace of {environment} is {observed}, not {tree_digest} (job {})",
                evidence.job_id
            )));
        }
        Ok(jobs)
    }

    /// Clone an environment: see the module documentation. Every phase is
    /// durable and evidenced; if one fails, the new environment exists but is
    /// inert (it has no contents until the seed is verified), and the error
    /// names the phase.
    pub async fn clone_environment(
        self: &Arc<Self>,
        source: &str,
        operator: &str,
        request: CloneRequest,
    ) -> Result<CloneReport, EnvironmentError> {
        let name = request.name.clone();
        let record = self.owned_environment(source, operator).await?;
        self.require_live(&record).await?;
        let spec = record
            .value
            .computer
            .clone()
            .ok_or_else(|| EnvironmentError::Invalid(format!("{source} has no computer")))?;
        let contents = record.value.contents.clone().unwrap_or_default();
        let policy = record
            .value
            .policy
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| EnvironmentError::Invalid(format!("{source}'s policy: {error}")))?;
        let omitted_config = if request.copy_config {
            vec![]
        } else {
            record.value.config.keys().cloned().collect()
        };

        // Capture, and prove the capture is what it says before anything exists.
        let exported = self.export_workspace(source, operator).await?;
        let mut jobs = vec![exported.job_id.clone()];

        // A new environment, on a computer placement finds for the same
        // requirements. It has no contents yet, so nothing runs before the seed.
        let phase = |what: &str, error: EnvironmentError| {
            EnvironmentError::Conflict(format!(
                "cloning {source} into {name} failed while {what}; {name} exists and is inert: {error}"
            ))
        };
        self.create_computer_environment(
            ComputerEnvironmentDefinition {
                name: name.clone(),
                desired_state: DesiredState::Running,
                env: if request.copy_config {
                    record.value.config.clone()
                } else {
                    Default::default()
                },
                policy,
                computer: ComputerRequest {
                    lifecycle: spec.lifecycle,
                    requirements: spec.requirements.clone(),
                    target: request.target.clone(),
                    ttl_seconds: None,
                },
                contents: Default::default(),
            },
            operator,
        )
        .await?;
        self.await_computer(&name, "run", |view| view.status == ComputerStatus::Running)
            .await
            .map_err(|error| phase("provisioning", error))?;

        jobs.extend(
            self.seed_workspace(
                &name,
                operator,
                &exported.bytes,
                &exported.summary.tree_digest,
            )
            .await
            .map_err(|error| phase("seeding", error))?,
        );

        // The seed is verified; now the declared contents, and the ordinary
        // reconciler starts what the source runs.
        self.change_environment(
            &name,
            operator,
            format!("cloned from {source}"),
            None,
            |value| {
                value.contents = Some(contents.clone());
                Ok(())
            },
        )
        .await
        .map_err(|error| phase("applying contents", error))?;
        let computer = self
            .await_computer(&name, "converge", |view| view.converged)
            .await
            .map_err(|error| phase("starting", error))?;
        let original = self.computer(source).await?;
        let repositories = contents
            .repositories
            .iter()
            .map(|repository| {
                let commit = |view: &crate::status::ComputerView| {
                    view.observed
                        .repositories
                        .get(&repository.name)
                        .and_then(|observed| observed.commit.clone())
                };
                (
                    repository.name.clone(),
                    (commit(&original), commit(&computer)),
                )
            })
            .collect();

        let cloned = self.owned_environment(&name, operator).await?;
        let change = self.event(
            Change::new(),
            events::ENVIRONMENT_COMMAND,
            Scope::environment(&name),
            format!("{operator} cloned {source} into {name}"),
            json!({
                "environment_id": cloned.id,
                "command": "clone",
                "source": source,
                "archive": compute_core::sha256_identity(&exported.bytes),
                "tree_digest": exported.summary.tree_digest,
                "files": exported.summary.files,
                "jobs": jobs,
            }),
        );
        self.apply(change).await?;
        Ok(CloneReport {
            source: source.to_owned(),
            environment: name,
            archive: compute_core::sha256_identity(&exported.bytes),
            tree_digest: exported.summary.tree_digest,
            files: exported.summary.files,
            bytes: exported.summary.bytes,
            seed_verified: true,
            repositories,
            omitted_config,
            jobs,
            computer,
        })
    }
}
