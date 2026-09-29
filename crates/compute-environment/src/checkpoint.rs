//! `compute.checkpoint@1`: an immutable, portable capture of a workspace.
//!
//! A checkpoint artifact is a canonical tar: `manifest.json`, then every file
//! of the workspace as `files/<path>`, sorted, with zero timestamps and
//! ownership and normalized modes (`0644`, `0755`). The manifest names every
//! entry with its SHA-256 and carries the workspace identity
//! (`compute.workspace@1`, see `docs/workspace.md`) the files must reproduce.
//!
//! **Identity derives from content, never from circumstance.** The artifact is
//! a function of the workspace contents, the source environment's id, its
//! computer and contents generations, the platform the capture was read on, the
//! exclusion policy, and the parent checkpoint. It is not a function of capture
//! time, machine, session, provider, target, or any temporary path. The same
//! state captured twice is the same bytes, so `artifact_digest`,
//! `checkpoint_id` (`ckp_` + that digest) and `tree_digest` are all reproducible.
//!
//! **One set of filesystem rules.** The workspace module is the authority for
//! what a portable workspace is. A checkpoint is built from a workspace archive
//! that module has already validated ([`crate::daemon::ArchivedWorkspace`]), and
//! [`validate`] runs the files back through the same reader, so symbolic and
//! hard links, special files, absolute and escaping paths, control characters,
//! and path collisions are refused by the same code. There is no second
//! implementation here.
//!
//! **Validation is total.** [`validate`] rebuilds the canonical artifact from
//! what it parsed and requires the bytes to be identical, so a truncated,
//! corrupted, re-ordered, or otherwise non-canonical artifact is refused, and
//! so is one whose files do not reproduce the recorded workspace digest.

use std::collections::BTreeMap;
use std::io::Read;

use serde::{Deserialize, Serialize};

use crate::EnvironmentError;
use crate::daemon::{
    ArchivedWorkspace, WORKSPACE_ARCHIVE_LIMIT, WORKSPACE_IDENTITY, read_workspace,
};

pub const CHECKPOINT_FORMAT: &str = "compute.checkpoint@1";

/// The largest checkpoint artifact accepted: the workspace transport bound plus
/// what the manifest and tar framing add.
pub const CHECKPOINT_ARTIFACT_LIMIT: usize = WORKSPACE_ARCHIVE_LIMIT + 4 * 1024 * 1024;

const MANIFEST: &str = "manifest.json";

/// What a capture leaves out, recorded so a reader knows what the state is not.
/// These are exactly the workspace contract's exclusions.
pub const EXCLUDED_PATHS: [&str; 2] = [
    "repos",
    ".compute (controller state; .compute/sources is kept)",
];

/// A statement, not a promise: what is known and what cannot be.
pub const EXCLUSION_NOTE: &str = "Compute writes no credential, token, session, or connection material into a workspace, and none is captured. Files a workload wrote into its own workspace are workspace content and are captured, secrets included; they cannot be recognised.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: String,
    pub workspace_identity: String,
    /// The workspace digest (`compute.workspace@1`) the files reproduce.
    pub tree_digest: String,
    pub source: Source,
    pub exclusions: Exclusions,
    /// The checkpoint this one was derived from: lineage, not authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Empty directories, sorted, then files, sorted.
    pub entries: Vec<Entry>,
}

/// Where the state came from. Provenance only: nothing here grants anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub environment_id: String,
    pub computer_generation: u64,
    pub contents_generation: u64,
    /// `uname -s`-`uname -m` of the machine it was read on: a workspace can
    /// hold architecture-bound files.
    pub platform: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exclusions {
    pub paths: Vec<String>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Entry {
    Directory {
        path: String,
    },
    File {
        path: String,
        executable: bool,
        size: u64,
        sha256: String,
    },
}

/// Everything a checkpoint records that is not the files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub environment_id: String,
    pub computer_generation: u64,
    pub contents_generation: u64,
    pub platform: String,
    pub parent: Option<String>,
}

/// A checkpoint artifact that validated.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub manifest: Manifest,
    /// `sha256:` of the artifact bytes.
    pub artifact_digest: String,
    /// `ckp_…`, derived from the artifact digest.
    pub checkpoint_id: String,
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
    pub size: u64,
}

impl Checkpoint {
    /// The file contents by path, from a validated artifact's bytes.
    pub fn read_files(bytes: &[u8]) -> Result<BTreeMap<String, (bool, Vec<u8>)>, EnvironmentError> {
        let (_, workspace) = parse(bytes)?;
        Ok(workspace
            .files
            .into_iter()
            .map(|(path, file)| (path, (file.executable, file.data)))
            .collect())
    }
}

fn invalid(what: impl std::fmt::Display) -> EnvironmentError {
    EnvironmentError::Invalid(format!("the checkpoint {what}"))
}

/// Build the canonical artifact for a validated workspace.
pub(crate) fn build(
    workspace: &ArchivedWorkspace,
    provenance: &Provenance,
) -> Result<Vec<u8>, EnvironmentError> {
    let mut entries = workspace
        .empty_directories
        .iter()
        .map(|path| Entry::Directory { path: path.clone() })
        .collect::<Vec<_>>();
    entries.extend(workspace.files.iter().map(|(path, file)| Entry::File {
        path: path.clone(),
        executable: file.executable,
        size: file.data.len() as u64,
        sha256: file.sha256.clone(),
    }));
    let manifest = Manifest {
        format: CHECKPOINT_FORMAT.into(),
        workspace_identity: WORKSPACE_IDENTITY.into(),
        tree_digest: workspace.identity().digest,
        source: Source {
            environment_id: provenance.environment_id.clone(),
            computer_generation: provenance.computer_generation,
            contents_generation: provenance.contents_generation,
            platform: provenance.platform.clone(),
        },
        exclusions: Exclusions {
            paths: EXCLUDED_PATHS.map(str::to_owned).to_vec(),
            note: EXCLUSION_NOTE.into(),
        },
        parent: provenance.parent.clone(),
        entries,
    };
    render(&manifest, workspace)
}

/// The canonical bytes of a manifest and the files it names.
fn render(manifest: &Manifest, workspace: &ArchivedWorkspace) -> Result<Vec<u8>, EnvironmentError> {
    let json = serde_json::to_vec_pretty(manifest).map_err(|error| invalid(error))?;
    let mut builder = tar::Builder::new(Vec::new());
    let mut append = |path: &str, mode: u32, data: &[u8]| -> Result<(), EnvironmentError> {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(tar::EntryType::Regular);
        builder
            .append_data(&mut header, path, data)
            .map_err(|error| invalid(format!("cannot hold the path {path:?}: {error}")))
    };
    append(MANIFEST, 0o644, &json)?;
    for (path, file) in &workspace.files {
        append(
            &format!("files/{path}"),
            if file.executable { 0o755 } else { 0o644 },
            &file.data,
        )?;
    }
    builder.into_inner().map_err(|error| invalid(error))
}

/// Parse an artifact into its manifest and the workspace it holds. Does not
/// establish that it is canonical; [`validate`] does.
fn parse(bytes: &[u8]) -> Result<(Manifest, ArchivedWorkspace), EnvironmentError> {
    if bytes.len() > CHECKPOINT_ARTIFACT_LIMIT {
        return Err(invalid(format!(
            "is {} bytes; the limit is {CHECKPOINT_ARTIFACT_LIMIT}",
            bytes.len()
        )));
    }
    let mut manifest = None;
    let mut contents = BTreeMap::<String, (bool, Vec<u8>)>::new();
    let mut tar = tar::Archive::new(bytes);
    for (index, entry) in tar
        .entries()
        .map_err(|error| invalid(format!("is unreadable: {error}")))?
        .enumerate()
    {
        let mut entry = entry.map_err(|error| invalid(format!("is unreadable: {error}")))?;
        if !entry.header().entry_type().is_file() {
            return Err(invalid("holds something other than files"));
        }
        let path = entry
            .path()
            .map_err(|error| invalid(format!("has an unreadable path: {error}")))?
            .to_string_lossy()
            .into_owned();
        let executable = entry.header().mode().unwrap_or(0) & 0o100 != 0;
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|error| invalid(format!("is truncated: {error}")))?;
        if index == 0 {
            if path != MANIFEST {
                return Err(invalid("does not begin with its manifest"));
            }
            manifest = Some(
                serde_json::from_slice::<Manifest>(&data)
                    .map_err(|error| invalid(format!("has an unreadable manifest: {error}")))?,
            );
            continue;
        }
        let Some(name) = path.strip_prefix("files/") else {
            return Err(invalid(format!("holds an unexpected entry: {path}")));
        };
        if contents
            .insert(name.to_owned(), (executable, data))
            .is_some()
        {
            return Err(invalid(format!("holds {name} twice")));
        }
    }
    let manifest = manifest.ok_or_else(|| invalid("has no manifest"))?;
    if manifest.format != CHECKPOINT_FORMAT {
        return Err(invalid(format!(
            "is {:?}, not {CHECKPOINT_FORMAT}",
            manifest.format
        )));
    }
    if manifest.workspace_identity != WORKSPACE_IDENTITY {
        return Err(invalid(format!(
            "records workspace identity {:?}, not {WORKSPACE_IDENTITY}",
            manifest.workspace_identity
        )));
    }
    // What the manifest names must be exactly what the artifact holds.
    let mut directories = vec![];
    let mut named = 0usize;
    for entry in &manifest.entries {
        match entry {
            Entry::Directory { path } => directories.push(path.clone()),
            Entry::File {
                path,
                executable,
                size,
                sha256,
            } => {
                named += 1;
                let (held_executable, data) = contents
                    .get(path)
                    .ok_or_else(|| invalid(format!("names {path}, which it does not hold")))?;
                if held_executable != executable
                    || data.len() as u64 != *size
                    || compute_core::sha256_identity(data).trim_start_matches("sha256:") != sha256
                {
                    return Err(invalid(format!(
                        "holds {path} differently from its manifest"
                    )));
                }
            }
        }
    }
    if named != contents.len() {
        return Err(invalid("holds files its manifest does not name"));
    }
    // The files go back through the workspace reader: its rules are the rules.
    let mut builder = tar::Builder::new(Vec::new());
    let mut add = |path: &str, mode: u32, directory: bool, data: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_entry_type(if directory {
            tar::EntryType::Directory
        } else {
            tar::EntryType::Regular
        });
        // Raw, so an unsafe path reaches the reader instead of the builder's
        // own checks.
        let name = header.as_old_mut().name.as_mut();
        if path.len() >= name.len() {
            return Err(invalid(format!("holds a path that is too long: {path}")));
        }
        name.fill(0);
        name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        builder
            .append(&header, data)
            .map_err(|error| invalid(error))
    };
    for path in &directories {
        add(path, 0o755, true, &[])?;
    }
    for (path, (executable, data)) in &contents {
        add(path, if *executable { 0o755 } else { 0o644 }, false, data)?;
    }
    let plain = builder.into_inner().map_err(|error| invalid(error))?;
    let workspace = read_workspace(&plain)?;
    if workspace.empty_directories != directories {
        return Err(invalid(
            "records directories that are not the workspace's empty ones",
        ));
    }
    if workspace.identity().digest != manifest.tree_digest {
        return Err(invalid(format!(
            "does not reproduce its workspace digest {}",
            manifest.tree_digest
        )));
    }
    Ok((manifest, workspace))
}

/// Validate an artifact completely: parse it, reproduce the workspace digest
/// from its files, and require the bytes to be exactly the canonical rendering.
pub fn validate(bytes: &[u8]) -> Result<Checkpoint, EnvironmentError> {
    let (manifest, workspace) = parse(bytes)?;
    if render(&manifest, &workspace)? != bytes {
        return Err(invalid("is not in canonical form"));
    }
    let identity = workspace.identity();
    let artifact_digest = compute_core::sha256_identity(bytes);
    Ok(Checkpoint {
        checkpoint_id: compute_state::ids::checkpoint(&artifact_digest),
        artifact_digest,
        files: identity.files,
        directories: identity.directories,
        bytes: identity.bytes,
        size: bytes.len() as u64,
        manifest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tar::EntryType;

    /// A tar entry with a raw name, so hostile paths reach the reader.
    fn entry(
        builder: &mut tar::Builder<Vec<u8>>,
        name: &str,
        kind: EntryType,
        mode: u32,
        data: &[u8],
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_entry_type(kind);
        if kind == EntryType::Link || kind == EntryType::Symlink {
            header.set_link_name("target").unwrap();
        }
        let raw = header.as_old_mut().name.as_mut();
        raw.fill(0);
        raw[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        builder.append(&header, data).unwrap();
    }

    fn archive(entries: &[(&str, EntryType, u32, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind, mode, data) in entries {
            entry(&mut builder, name, *kind, *mode, data);
        }
        builder.into_inner().unwrap()
    }

    fn provenance() -> Provenance {
        Provenance {
            environment_id: "env_1".into(),
            computer_generation: 1,
            contents_generation: 2,
            platform: "Linux-x86_64".into(),
            parent: None,
        }
    }

    fn portable() -> Vec<u8> {
        archive(&[
            ("./", EntryType::Directory, 0o755, b""),
            ("./b.txt", EntryType::Regular, 0o600, b"bee"),
            ("./bin/", EntryType::Directory, 0o700, b""),
            ("./bin/run.sh", EntryType::Regular, 0o700, b"#!/bin/sh\n"),
            ("./empty/", EntryType::Directory, 0o755, b""),
            ("./a.txt", EntryType::Regular, 0o644, b"ay"),
        ])
    }

    fn built() -> Vec<u8> {
        build(&read_workspace(&portable()).unwrap(), &provenance()).unwrap()
    }

    #[test]
    fn a_checkpoint_is_a_pure_function_of_portable_content() {
        let bytes = built();
        // Different tar framing (order, timestamps, ownership, modes beyond
        // the executable bit) is the same portable content.
        let reordered = {
            let mut builder = tar::Builder::new(Vec::new());
            for (name, kind, mode, data) in [
                ("./a.txt", EntryType::Regular, 0o664, &b"ay"[..]),
                ("./empty/", EntryType::Directory, 0o700, b""),
                ("./bin/run.sh", EntryType::Regular, 0o755, b"#!/bin/sh\n"),
                ("./b.txt", EntryType::Regular, 0o640, b"bee"),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(mode);
                header.set_mtime(1_700_000_000);
                header.set_uid(1234);
                header.set_gid(4321);
                header.set_entry_type(kind);
                header.set_path(name).unwrap();
                header.set_cksum();
                builder.append(&header, data).unwrap();
            }
            builder.into_inner().unwrap()
        };
        let again = build(&read_workspace(&reordered).unwrap(), &provenance()).unwrap();
        assert_eq!(bytes, again);
        let one = validate(&bytes).unwrap();
        let two = validate(&again).unwrap();
        assert_eq!(one.artifact_digest, two.artifact_digest);
        assert_eq!(one.checkpoint_id, two.checkpoint_id);
        assert_eq!(one.manifest.tree_digest, two.manifest.tree_digest);
        assert!(one.checkpoint_id.starts_with("ckp_"));

        // Provenance is content; circumstance is not. A different platform or
        // generation is a different artifact of the same tree.
        let other = build(
            &read_workspace(&portable()).unwrap(),
            &Provenance {
                platform: "Linux-aarch64".into(),
                ..provenance()
            },
        )
        .unwrap();
        let other = validate(&other).unwrap();
        assert_ne!(other.artifact_digest, one.artifact_digest);
        assert_eq!(other.manifest.tree_digest, one.manifest.tree_digest);
    }

    #[test]
    fn a_checkpoint_holds_the_portable_workspace_and_nothing_else() {
        let checkpoint = validate(&built()).unwrap();
        assert_eq!(checkpoint.manifest.format, CHECKPOINT_FORMAT);
        assert_eq!((checkpoint.files, checkpoint.directories), (3, 1));
        let files = Checkpoint::read_files(&built()).unwrap();
        assert_eq!(files["a.txt"], (false, b"ay".to_vec()));
        assert_eq!(
            files["b.txt"],
            (false, b"bee".to_vec()),
            "only the executable bit is kept"
        );
        assert_eq!(files["bin/run.sh"], (true, b"#!/bin/sh\n".to_vec()));
        assert!(checkpoint.manifest.entries.contains(&Entry::Directory {
            path: "empty".into()
        }));
        assert_eq!(checkpoint.manifest.exclusions.paths.len(), 2);
        assert!(
            checkpoint
                .manifest
                .exclusions
                .note
                .contains("cannot be recognised")
        );
    }

    #[test]
    fn lineage_is_provenance_and_changes_the_content_identity() {
        let root = validate(&built()).unwrap();
        let child = validate(
            &build(
                &read_workspace(&portable()).unwrap(),
                &Provenance {
                    parent: Some(root.checkpoint_id.clone()),
                    ..provenance()
                },
            )
            .unwrap(),
        )
        .unwrap();
        let sibling = validate(
            &build(
                &read_workspace(&portable()).unwrap(),
                &Provenance {
                    parent: Some(root.checkpoint_id.clone()),
                    contents_generation: 3,
                    ..provenance()
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            child.manifest.parent.as_deref(),
            Some(root.checkpoint_id.as_str())
        );
        assert_ne!(child.checkpoint_id, root.checkpoint_id);
        assert_ne!(
            child.checkpoint_id, sibling.checkpoint_id,
            "C1 has independent children"
        );
    }

    #[test]
    fn corrupted_truncated_and_non_canonical_artifacts_are_refused() {
        let bytes = built();
        assert!(validate(&bytes).is_ok());
        assert!(validate(&bytes[..bytes.len() / 2]).is_err(), "truncated");
        assert!(validate(&[]).is_err(), "empty");
        // A flipped bit anywhere that is content.
        for position in [700, bytes.len() / 3, bytes.len() / 2] {
            let mut damaged = bytes.clone();
            damaged[position] ^= 0x01;
            assert!(validate(&damaged).is_err(), "byte {position}");
        }
        // A file changed and its manifest entry left as it was.
        let mut tampered = bytes.clone();
        let at = tampered.windows(3).position(|w| w == b"bee").unwrap();
        tampered[at..at + 3].copy_from_slice(b"BEE");
        assert!(validate(&tampered).is_err());
        // An extra file appended is not what the manifest names.
        let mut extra = tar::Archive::new(&bytes[..]);
        let mut builder = tar::Builder::new(Vec::new());
        for item in extra.entries().unwrap() {
            let mut item = item.unwrap();
            let mut data = vec![];
            item.read_to_end(&mut data).unwrap();
            builder.append(&item.header().clone(), &data[..]).unwrap();
        }
        entry(&mut builder, "files/stray", EntryType::Regular, 0o644, b"x");
        assert!(validate(&builder.into_inner().unwrap()).is_err());
        // Not a checkpoint at all.
        assert!(validate(&portable()).is_err());
    }

    #[test]
    fn unsafe_workspace_entries_are_refused_by_the_one_reader() {
        let refused = |entries: &[(&str, EntryType, u32, &[u8])], why: &str| {
            let error = read_workspace(&archive(entries)).unwrap_err().to_string();
            assert!(error.contains(why), "{why}: {error}");
        };
        refused(
            &[("./link", EntryType::Symlink, 0o777, b"")],
            "other than files and directories",
        );
        refused(
            &[("./link", EntryType::Link, 0o644, b"")],
            "other than files and directories",
        );
        refused(
            &[("./pipe", EntryType::Fifo, 0o644, b"")],
            "other than files and directories",
        );
        refused(
            &[("./dev", EntryType::Char, 0o644, b"")],
            "other than files and directories",
        );
        refused(
            &[("/etc/passwd", EntryType::Regular, 0o644, b"x")],
            "unsafe path",
        );
        refused(
            &[("../escape", EntryType::Regular, 0o644, b"x")],
            "unsafe path",
        );
        refused(
            &[("./a/../../b", EntryType::Regular, 0o644, b"x")],
            "unsafe path",
        );
        refused(
            &[("./bad\nname", EntryType::Regular, 0o644, b"x")],
            "unsupported path",
        );
        refused(
            &[("./back\\slash", EntryType::Regular, 0o644, b"x")],
            "unsupported path",
        );
        refused(
            &[
                ("./a", EntryType::Regular, 0o644, b"1"),
                ("./a", EntryType::Regular, 0o644, b"2"),
            ],
            "twice",
        );
        refused(
            &[
                ("./a", EntryType::Regular, 0o644, b"1"),
                ("./a/b", EntryType::Regular, 0o644, b"2"),
            ],
            "both a file and a directory",
        );
        refused(
            &[
                ("./a", EntryType::Regular, 0o644, b"1"),
                ("./a/", EntryType::Directory, 0o755, b""),
            ],
            "both a file and a directory",
        );
        refused(
            &[("./repos/x", EntryType::Regular, 0o644, b"x")],
            "controller or re-derived state",
        );
        refused(
            &[(
                "./.compute/processes/p/pid",
                EntryType::Regular,
                0o644,
                b"1",
            )],
            "controller or re-derived state",
        );
        // What a checkpoint artifact holds goes through the same reader.
        let mut hostile = tar::Builder::new(Vec::new());
        let good = built();
        let mut archive = tar::Archive::new(&good[..]);
        let mut manifest = archive.entries().unwrap().next().unwrap().unwrap();
        let mut json = String::new();
        manifest.read_to_string(&mut json).unwrap();
        entry(
            &mut hostile,
            MANIFEST,
            EntryType::Regular,
            0o644,
            json.as_bytes(),
        );
        entry(
            &mut hostile,
            "files/../evil",
            EntryType::Regular,
            0o644,
            b"x",
        );
        assert!(validate(&hostile.into_inner().unwrap()).is_err());
    }
}
