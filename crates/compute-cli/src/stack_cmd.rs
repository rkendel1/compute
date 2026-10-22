//! `compute stack`: keep an assembled stack reproducible and in sync.
//!
//! A stack's manifest (`stacks/<name>/stack.toml`) is policy. Its lock
//! (`stack.lock`) is the known-good revision: exact commits, published
//! versions, integrity, and the relationships that held. Nothing here edits
//! the known-good lock except `promote`, and `promote` requires recorded,
//! passing acceptance evidence for exactly that lock and an explicit approval.
//!
//! ```text
//! check     drift between the manifest, the lock, and the world
//! diff      what changed between two revisions, and what it affects
//! update    resolve a candidate (stacks/<name>/candidates/<digest>.lock)
//! test      materialize a revision from its declared sources and run its suites
//! promote   make a tested candidate the known-good revision (approval required)
//! reproduce restore and verify every input of a revision
//! inventory every use of every provided package, and where sources disagree
//! consumers who uses one package, and what moving them to a target leaves to do
//! ```
//!
//! Sources are read with `git` and packages with `npm view` / `npm pack`, the
//! tools that own them; nothing is resolved from a developer's checkout.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use clap::{Args, Subcommand};
use compute_core::ComputeError;
use compute_project::{
    LOCK_FILE, LockDiff, Overrides, PublishedPackage, RECORDED_FILES, Stack, StackLock,
    StackResolver, TreeEntry, diff_locks, find_stack_file, resolve_stack, sha256_hex, short_commit,
    upgrade_plan,
};
use serde::{Deserialize, Serialize};

/// `check`: the lock and the world agree.
const IN_SYNC: i32 = 0;
/// `check`: drift (a newer commit or release, a changed manifest, a relationship that changed).
const DRIFT: i32 = 1;
/// `check`/`reproduce`/`test`: the lock is invalid, an artifact changed under the same identity, or a check failed.
const FAILED: i32 = 2;
/// `promote` without `--approve`.
const APPROVAL_REQUIRED: i32 = 3;

pub const EVIDENCE_FORMAT: &str = "compute.stack-evidence@1";
pub const APPROVAL_FORMAT: &str = "compute.stack-approval@1";
/// Where the git object cache lives (default `$XDG_CACHE_HOME/compute/stack` or `~/.cache/compute/stack`).
pub const CACHE_ENV: &str = "COMPUTE_STACK_CACHE";

#[derive(Args, Debug)]
pub struct StackCommand {
    #[command(subcommand)]
    pub command: StackCommands,
    /// The stack: a name under `./stacks` (or `COMPUTE_STACKS`), or a path.
    #[arg(long, global = true, default_value = "randy")]
    pub stack: String,
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum StackCommands {
    /// Report drift between the manifest, the known-good lock, and the declared
    /// sources and registry. Exits 0 in sync, 1 on drift, 2 when the lock is
    /// invalid or an artifact changed under the same identity.
    Check {
        /// Only compare the lock with the manifest; contact nothing.
        #[arg(long)]
        offline: bool,
        /// Also compare local checkouts (`<DIR>/<source>`) with the lock.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// What makes the check fail: `drift` (any open finding, the default)
        /// or `violation` (only what blocks promotion).
        #[arg(long, default_value = "drift", value_parser = ["drift", "violation"])]
        fail_on: String,
    },
    /// Every use of every package the stack's sources provide, at a revision's
    /// locked commits: declarations, resolutions, installs, copies, and findings.
    Inventory {
        #[arg(default_value = "current")]
        revision: String,
        /// Only this package (`npm:<name>` / `cargo:<crate>`).
        #[arg(long)]
        package: Option<String>,
        #[arg(long)]
        consumer: Option<String>,
        /// Only findings of this status (`open`, `accepted`, `violated`).
        #[arg(long)]
        status: Option<String>,
    },
    /// The consumers of one package and, with `--target`, what moving each of
    /// them there leaves to do. It changes nothing: a consumer moves in its own
    /// repository, and the stack sees it as a candidate (`update --candidate
    /// --source NAME=BRANCH`).
    Consumers {
        package: String,
        #[arg(long, default_value = "current")]
        revision: String,
        /// A version (or, for a copy, a canonical commit) to plan towards.
        #[arg(long)]
        target: Option<String>,
    },
    /// Explain what changed between two revisions and which suites it affects.
    /// A revision is `current` (the known-good lock), `head` (a fresh
    /// resolution of the manifest), a digest prefix, or a lock file.
    Diff {
        #[arg(default_value = "current")]
        from: String,
        #[arg(default_value = "head")]
        to: String,
    },
    /// Resolve a candidate revision. The known-good lock is never changed.
    Update {
        /// Required: an update is always a candidate until it is promoted.
        #[arg(long)]
        candidate: bool,
        /// `<source>=<branch|tag|commit>`, instead of the manifest's.
        #[arg(long = "source", value_name = "NAME=REF")]
        sources: Vec<String>,
        /// `<package>=<version>` (within the manifest's constraint).
        #[arg(long = "package", value_name = "PACKAGE=VERSION")]
        packages: Vec<String>,
    },
    /// Materialize a revision from its declared sources (fresh clones at the
    /// locked commits, verified against the lock) and run its acceptance suites.
    Test {
        /// The revision to test (default: `current`).
        #[arg(default_value = "current")]
        revision: String,
        /// Run only these suites.
        #[arg(long = "suite")]
        suites: Vec<String>,
        /// Run only the suites the change from this revision affects.
        #[arg(long)]
        impacted_from: Option<String>,
        /// Keep the sources and outputs here (default: a new temporary directory).
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// Write the evidence next to the stack (`evidence/<digest>.json`).
        /// Promotion reads only recorded evidence.
        #[arg(long)]
        record: bool,
    },
    /// Make a tested candidate the known-good revision. Requires recorded
    /// evidence in which every mandatory suite passed for exactly this lock,
    /// from a clean harness, and `--approve --approved-by NAME`.
    Promote {
        candidate: String,
        #[arg(long)]
        approve: bool,
        #[arg(long)]
        approved_by: Option<String>,
        /// The approver's SSH private key. Required when the stack names its
        /// approvers (`allowed_signers` beside the manifest): the approval is
        /// signed with it and verified against that file before it counts.
        #[arg(long)]
        sign_key: Option<PathBuf>,
    },
    /// Restore every input of a revision from its declared sources and the
    /// registry, and verify each against the lock.
    Reproduce {
        #[arg(default_value = "current")]
        revision: String,
        #[arg(long)]
        into: Option<PathBuf>,
    },
}

fn coded(code: &str, message: impl Into<String>) -> ComputeError {
    ComputeError::Coded {
        code: code.into(),
        message: message.into(),
    }
}

fn print<T: Serialize>(json: bool, value: &T, human: impl FnOnce()) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).expect("serializes")
        );
    } else {
        human();
    }
}

/// Run a command; stdout on success, the failure otherwise.
fn run(command: &mut Command) -> Result<Vec<u8>, String> {
    let output = command
        .output()
        .map_err(|error| format!("{:?}: {error}", command.get_program()))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn git(directory: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

/// The URL a repository identity names.
pub fn repository_url(repository: &str) -> String {
    match repository.split_once(':') {
        Some(("github", path)) => format!("https://github.com/{path}"),
        Some(("file", path)) => path.to_owned(),
        _ => repository.to_owned(),
    }
}

fn is_commit(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Sources through `git` (one bare object cache per repository), packages through `npm`.
pub struct ToolResolver {
    cache: PathBuf,
}

impl ToolResolver {
    pub fn new() -> Result<Self, ComputeError> {
        let cache = std::env::var_os(CACHE_ENV)
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_CACHE_HOME").map(|d| PathBuf::from(d).join("compute/stack"))
            })
            .or_else(|| {
                std::env::var_os("HOME").map(|d| PathBuf::from(d).join(".cache/compute/stack"))
            })
            .ok_or_else(|| coded("stack_cache_unavailable", format!("set {CACHE_ENV}")))?;
        std::fs::create_dir_all(&cache)
            .map_err(|e| coded("stack_cache_unavailable", e.to_string()))?;
        Ok(Self { cache })
    }

    fn repository(&self, repository: &str) -> Result<PathBuf, String> {
        let name: String = repository
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let path = self.cache.join(format!("{name}.git"));
        if !path.join("HEAD").is_file() {
            run(Command::new("git")
                .args(["init", "-q", "--bare"])
                .arg(&path))?;
        }
        Ok(path)
    }

    /// Ensure `commit` is in the cache, fetching it (shallow) from its declared source.
    fn ensure(&self, repository: &str, commit: &str) -> Result<PathBuf, String> {
        let cache = self.repository(repository)?;
        if run(git(&cache).args(["cat-file", "-e", &format!("{commit}^{{commit}}")])).is_err() {
            run(git(&cache).args([
                "fetch",
                "-q",
                "--depth",
                "1",
                &repository_url(repository),
                commit,
            ]))?;
            run(git(&cache).args(["update-ref", &format!("refs/stack/{commit}"), commit]))?;
        }
        Ok(cache)
    }
}

impl StackResolver for ToolResolver {
    fn resolve_reference(&self, repository: &str, reference: &str) -> Result<String, String> {
        let url = repository_url(repository);
        if is_commit(reference) {
            self.ensure(repository, &reference.to_ascii_lowercase())?;
            return Ok(reference.to_ascii_lowercase());
        }
        let listed = run(Command::new("git").env("GIT_TERMINAL_PROMPT", "0").args([
            "ls-remote",
            &url,
            &format!("refs/heads/{reference}"),
            &format!("refs/tags/{reference}"),
            &format!("refs/tags/{reference}^{{}}"),
        ]))?;
        let text = String::from_utf8_lossy(&listed);
        let mut refs: BTreeMap<String, String> = BTreeMap::new();
        for line in text.lines() {
            if let Some((commit, name)) = line.split_once('\t') {
                refs.insert(name.to_owned(), commit.to_owned());
            }
        }
        let commit = refs
            .get(&format!("refs/tags/{reference}^{{}}"))
            .or_else(|| refs.get(&format!("refs/heads/{reference}")))
            .or_else(|| refs.get(&format!("refs/tags/{reference}")))
            .cloned()
            .ok_or_else(|| format!("no branch or tag `{reference}` in {url}"))?;
        self.ensure(repository, &commit)?;
        Ok(commit)
    }

    fn tree(&self, repository: &str, commit: &str) -> Result<String, String> {
        let cache = self.ensure(repository, commit)?;
        let out = run(git(&cache).args(["rev-parse", &format!("{commit}^{{tree}}")]))?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }

    fn read_file(
        &self,
        repository: &str,
        commit: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        let cache = self.ensure(repository, commit)?;
        let object = format!("{commit}:{path}");
        if run(git(&cache).args(["cat-file", "-e", &object])).is_err() {
            return Ok(None);
        }
        run(git(&cache).args(["cat-file", "blob", &object])).map(Some)
    }

    fn has_commit(&self, repository: &str, commit: &str) -> Result<bool, String> {
        Ok(self.ensure(repository, commit).is_ok())
    }

    fn list_tree(&self, repository: &str, commit: &str) -> Result<Vec<TreeEntry>, String> {
        let cache = self.ensure(repository, commit)?;
        let out = run(git(&cache).args(["ls-tree", "-r", "-t", "-z", "--full-tree", commit]))?;
        let mut entries = vec![];
        for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let record = String::from_utf8_lossy(record);
            let Some((meta, path)) = record.split_once('\t') else {
                continue;
            };
            let mut fields = meta.split(' ');
            let (_mode, kind, object) = (fields.next(), fields.next(), fields.next());
            if let (Some(kind), Some(object)) = (kind, object) {
                entries.push(TreeEntry {
                    path: path.to_owned(),
                    kind: kind.to_owned(),
                    object: object.to_owned(),
                });
            }
        }
        Ok(entries)
    }

    fn changed_paths(&self, repository: &str, from: &str, to: &str) -> Result<Vec<String>, String> {
        self.ensure(repository, from)?;
        let cache = self.ensure(repository, to)?;
        let out = run(git(&cache).args(["diff", "--name-only", from, to]))?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .map(str::to_owned)
            .collect())
    }

    fn package_versions(&self, package: &str) -> Result<Vec<String>, String> {
        let out = run(Command::new("npm").args(["view", package, "versions", "--json"]))?;
        match serde_json::from_slice::<serde_json::Value>(&out).map_err(|e| e.to_string())? {
            serde_json::Value::Array(items) => Ok(items
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()),
            serde_json::Value::String(one) => Ok(vec![one]),
            other => Err(format!("unexpected npm answer {other}")),
        }
    }

    fn package(&self, package: &str, version: &str) -> Result<PublishedPackage, String> {
        let out = run(Command::new("npm").args([
            "view",
            &format!("{package}@{version}"),
            "dist.integrity",
            "gitHead",
            "dependencies",
            "--json",
        ]))?;
        let mut value: serde_json::Value =
            serde_json::from_slice(&out).map_err(|e| e.to_string())?;
        // With one of the asked fields present, npm prints that field alone; only integrity is always present.
        if let serde_json::Value::String(integrity) = value {
            value = serde_json::json!({ "dist.integrity": integrity });
        }
        let integrity = value["dist.integrity"]
            .as_str()
            .ok_or_else(|| format!("{package}@{version} is not published"))?
            .to_owned();
        Ok(PublishedPackage {
            integrity,
            git_head: value["gitHead"].as_str().map(str::to_owned),
            dependencies: value["dependencies"]
                .as_object()
                .map(|deps| {
                    deps.iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// A stack, where its files live, and its known-good lock.
struct Located {
    stack: Stack,
    directory: PathBuf,
    /// The repository root holding `compatibility/stack/` (two levels above the stack directory).
    harness: PathBuf,
}

fn locate(selection: &str) -> Result<Located, ComputeError> {
    let cwd = std::env::current_dir().map_err(|e| coded("stack_not_found", e.to_string()))?;
    let file =
        find_stack_file(selection, &[cwd]).map_err(|e| coded("stack_not_found", e.to_string()))?;
    let text =
        std::fs::read_to_string(&file).map_err(|e| coded("stack_not_found", e.to_string()))?;
    let stack = Stack::parse(&text).map_err(|e| coded("stack_invalid", e.to_string()))?;
    let directory = file.parent().expect("a file has a parent").to_path_buf();
    let harness = directory
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| directory.clone());
    Ok(Located {
        stack,
        directory,
        harness,
    })
}

fn hex(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

/// A revision's file name: the first 16 hex digits of its digest.
fn revision_name(digest: &str) -> String {
    hex(digest)[..16].to_owned()
}

fn read_lock(path: &Path) -> Result<StackLock, ComputeError> {
    let bytes = std::fs::read(path)
        .map_err(|e| coded("stack_lock_unreadable", format!("{}: {e}", path.display())))?;
    StackLock::parse(&bytes)
        .map_err(|e| coded("stack_lock_invalid", format!("{}: {e}", path.display())))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ComputeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| coded("stack_write_failed", e.to_string()))?;
    }
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes).map_err(|e| coded("stack_write_failed", e.to_string()))?;
    std::fs::rename(&temporary, path).map_err(|e| coded("stack_write_failed", e.to_string()))
}

/// Find a revision: `current`, `head` (resolved now), a lock file, or a digest
/// prefix among the known-good lock, history, and candidates.
fn revision(
    located: &Located,
    name: &str,
    resolver: &dyn StackResolver,
) -> Result<(StackLock, String), ComputeError> {
    if name == "head" {
        let lock = resolve_stack(&located.stack, &Overrides::default(), resolver)
            .map_err(|e| coded("stack_unresolved", e.to_string()))?;
        return Ok((lock, "head".into()));
    }
    let current = located.directory.join(LOCK_FILE);
    if name == "current" {
        if !current.is_file() {
            return Err(coded(
                "stack_lock_missing",
                format!(
                    "{} has no known-good lock: resolve a candidate (`compute stack update --candidate`), test it, and promote it",
                    located.directory.display()
                ),
            ));
        }
        return Ok((read_lock(&current)?, "current".into()));
    }
    let as_path = PathBuf::from(name);
    if as_path.is_file() {
        return Ok((read_lock(&as_path)?, as_path.display().to_string()));
    }
    let prefix = hex(name).to_ascii_lowercase();
    let mut found = vec![];
    if current.is_file() {
        let lock = read_lock(&current)?;
        if hex(&lock.digest()).starts_with(&prefix) {
            found.push((lock, "current".to_owned()));
        }
    }
    for directory in ["history", "candidates"] {
        let path = located.directory.join(directory);
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = entry.path();
            if file.extension().is_some_and(|e| e == "lock")
                && file
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with(&prefix) || prefix.starts_with(s))
            {
                let lock = read_lock(&file)?;
                if hex(&lock.digest()).starts_with(&prefix)
                    && !found.iter().any(|(l, _)| l.digest() == lock.digest())
                {
                    found.push((
                        lock,
                        format!(
                            "{directory}/{}",
                            file.file_name().unwrap().to_string_lossy()
                        ),
                    ));
                }
            }
        }
    }
    match found.len() {
        1 => Ok(found.pop().unwrap()),
        0 => Err(coded(
            "stack_revision_not_found",
            format!("no revision `{name}`"),
        )),
        _ => Err(coded(
            "stack_revision_ambiguous",
            format!("`{name}` names more than one revision"),
        )),
    }
}

fn print_diff(diff: &LockDiff) {
    println!(
        "from {}  to {}",
        &hex(&diff.from)[..16],
        &hex(&diff.to)[..16]
    );
    if diff.is_empty() {
        println!("no change");
        return;
    }
    if diff.manifest_changed {
        println!("manifest: changed (stack identity or assembly differs)");
    }
    for source in &diff.sources {
        println!(
            "source {}: {} -> {} ({} paths changed)",
            source.name,
            source.from.as_deref().map(short_commit).unwrap_or("-"),
            source.to.as_deref().map(short_commit).unwrap_or("-"),
            source.changed_paths
        );
        for (label, items) in [
            ("lockfiles", &source.lockfiles),
            ("toolchains", &source.toolchains),
            ("versions", &source.recorded),
            ("contracts", &source.contracts),
        ] {
            if !items.is_empty() {
                println!("  {label}: {}", items.join(", "));
            }
        }
    }
    for package in &diff.packages {
        println!(
            "package {}: {} -> {}{}",
            package.package,
            package.from.as_deref().unwrap_or("-"),
            package.to.as_deref().unwrap_or("-"),
            if package.integrity_changed {
                " (SAME VERSION, DIFFERENT INTEGRITY)"
            } else {
                ""
            }
        );
    }
    for relationship in &diff.relationships {
        println!(
            "relationship {}: {} -> {}: {}",
            relationship.name,
            relationship.from.as_deref().unwrap_or("-"),
            relationship.to.as_deref().unwrap_or("-"),
            relationship.detail
        );
    }
    for finding in &diff.findings {
        println!(
            "finding {}/{} {} in {}{}: {} -> {}: {}",
            finding.kind,
            finding.check,
            finding.package,
            finding.consumer,
            if finding.path.is_empty() {
                String::new()
            } else {
                format!(":{}", finding.path)
            },
            finding.from.as_deref().unwrap_or("-"),
            finding.to.as_deref().unwrap_or("-"),
            finding.detail
        );
    }
    println!("affected sources: {}", diff.affected.join(", "));
    println!("suites to run: {}", diff.suites.join(", "));
}

#[derive(Serialize)]
struct CheckReport {
    stack: String,
    lock: Option<String>,
    status: &'static str,
    findings: Vec<Finding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<LockDiff>,
}

#[derive(Serialize)]
struct Finding {
    severity: &'static str,
    kind: String,
    detail: String,
}

fn check(
    located: &Located,
    offline: bool,
    workspace: Option<&Path>,
    fail_on: &str,
    json: bool,
) -> Result<i32, ComputeError> {
    let mut findings = vec![];
    let current = located.directory.join(LOCK_FILE);
    if !current.is_file() {
        findings.push(Finding {
            severity: "drift",
            kind: "lock-missing".into(),
            detail: "no known-good lock: resolve, test, and promote a candidate".into(),
        });
        let report = CheckReport {
            stack: located.stack.name.clone(),
            lock: None,
            status: "drift",
            findings,
            diff: None,
        };
        print(json, &report, || {
            println!(
                "drift: no known-good lock in {}",
                located.directory.display()
            )
        });
        return Ok(DRIFT);
    }
    let lock = match read_lock(&current) {
        Ok(lock) => lock,
        Err(error) => {
            let report = CheckReport {
                stack: located.stack.name.clone(),
                lock: None,
                status: "invalid",
                findings: vec![Finding {
                    severity: "failed",
                    kind: "lock-invalid".into(),
                    detail: error.to_string(),
                }],
                diff: None,
            };
            print(json, &report, || println!("invalid: {error}"));
            return Ok(FAILED);
        }
    };
    if lock.stack.assembly != located.stack.assembly_digest()
        || lock.stack.fingerprint != located.stack.identity().fingerprint
    {
        findings.push(Finding {
            severity: "drift",
            kind: "manifest-changed".into(),
            detail: "stack.toml no longer describes the locked revision: resolve and promote a candidate".into(),
        });
    }
    for package in &lock.packages {
        if let Some((_, constraint)) = located.stack.package(&package.package)
            && compute_core::version_satisfies(constraint, &package.version) != Some(true)
        {
            findings.push(Finding {
                severity: "drift",
                kind: "constraint".into(),
                detail: format!(
                    "{} {} no longer satisfies `{constraint}`",
                    package.package, package.version
                ),
            });
        }
    }
    for result in lock.violations() {
        findings.push(Finding {
            severity: "failed",
            kind: "relationship-violated".into(),
            detail: format!("{}: {}", result.name, result.detail),
        });
    }
    for finding in &lock.inventory.findings {
        findings.push(Finding {
            severity: match finding.status.as_str() {
                "violated" => "failed",
                "accepted" => "accepted",
                _ => "drift",
            },
            kind: format!("{}/{}", finding.kind, finding.check),
            detail: format!(
                "{} in {}{}: {}{}",
                finding.package,
                finding.consumer,
                if finding.path.is_empty() {
                    String::new()
                } else {
                    format!(":{}", finding.path)
                },
                finding.detail,
                finding
                    .accepted_by
                    .as_deref()
                    .map(|r| format!(" [accepted: {r}]"))
                    .unwrap_or_default()
            ),
        });
    }
    if let Some(workspace) = workspace {
        for source in &lock.sources {
            let directory = workspace.join(&source.name);
            if !directory.join(".git").exists() {
                continue;
            }
            let head = run(git(&directory).args(["rev-parse", "HEAD"]))
                .map(|o| String::from_utf8_lossy(&o).trim().to_owned());
            let dirty =
                run(git(&directory).args(["status", "--porcelain", "--untracked-files=no"]))
                    .map(|o| !o.is_empty());
            if let Ok(head) = head
                && head != source.commit
            {
                findings.push(Finding {
                    severity: "drift",
                    kind: "checkout-moved".into(),
                    detail: format!(
                        "{} is at {}, the lock has {}",
                        directory.display(),
                        short_commit(&head),
                        short_commit(&source.commit)
                    ),
                });
            }
            if dirty == Ok(true) {
                findings.push(Finding {
                    severity: "drift",
                    kind: "checkout-dirty".into(),
                    detail: format!("{} has uncommitted changes", directory.display()),
                });
            }
        }
    }
    let mut diff = None;
    if !offline {
        let resolver = ToolResolver::new()?;
        let head = resolve_stack(&located.stack, &Overrides::default(), &resolver)
            .map_err(|e| coded("stack_unresolved", e.to_string()))?;
        let computed = diff_locks(&located.stack, &lock, &head, Some(&resolver));
        for source in &computed.sources {
            findings.push(Finding {
                severity: "drift",
                kind: "source-moved".into(),
                detail: format!(
                    "{0} resolves to {2} at the reference the manifest follows; the lock has {1}{3}",
                    source.name,
                    source.from.as_deref().map(short_commit).unwrap_or("-"),
                    source.to.as_deref().map(short_commit).unwrap_or("-"),
                    if source.contracts.is_empty() {
                        String::new()
                    } else {
                        format!("; contracts changed: {}", source.contracts.join(", "))
                    }
                ),
            });
        }
        for package in &computed.packages {
            findings.push(Finding {
                severity: if package.integrity_changed {
                    "failed"
                } else {
                    "drift"
                },
                kind: if package.integrity_changed {
                    "artifact-changed".into()
                } else {
                    "release-available".into()
                },
                detail: format!(
                    "{}: locked {}, now {}",
                    package.package,
                    package.from.as_deref().unwrap_or("-"),
                    package.to.as_deref().unwrap_or("-")
                ),
            });
        }
        for change in &computed.findings {
            if change.to.is_none() {
                findings.push(Finding {
                    severity: "drift",
                    kind: "finding-resolved-upstream".into(),
                    detail: format!(
                        "{}/{} {} in {} no longer holds at the sources' heads: {}",
                        change.kind, change.check, change.package, change.consumer, change.detail
                    ),
                });
            } else if change.from.is_none() {
                findings.push(Finding {
                    severity: if change.to.as_deref() == Some("violated") {
                        "failed"
                    } else {
                        "drift"
                    },
                    kind: "finding-new-upstream".into(),
                    detail: format!(
                        "{}/{} {} in {} at the sources' heads: {}",
                        change.kind, change.check, change.package, change.consumer, change.detail
                    ),
                });
            }
        }
        for relationship in &computed.relationships {
            findings.push(Finding {
                severity: if relationship.to.as_deref() == Some("violated") {
                    "failed"
                } else {
                    "drift"
                },
                kind: "relationship-changed".into(),
                detail: format!(
                    "{}: {} -> {}: {}",
                    relationship.name,
                    relationship.from.as_deref().unwrap_or("-"),
                    relationship.to.as_deref().unwrap_or("-"),
                    relationship.detail
                ),
            });
        }
        if !computed.suites.is_empty() && !computed.is_empty() {
            findings.push(Finding {
                severity: "drift",
                kind: "suites-affected".into(),
                detail: format!(
                    "before promoting the change, run: {}",
                    computed.suites.join(", ")
                ),
            });
        }
        diff = Some(computed);
    }
    let failed = findings.iter().any(|f| f.severity == "failed");
    let drift = findings.iter().any(|f| f.severity == "drift");
    let status = if failed {
        "failed"
    } else if drift {
        "drift"
    } else {
        "in-sync"
    };
    let report = CheckReport {
        stack: located.stack.name.clone(),
        lock: Some(lock.digest()),
        status,
        findings,
        diff,
    };
    print(json, &report, || {
        println!(
            "stack {} lock {}: {status}",
            report.stack,
            &hex(report.lock.as_deref().unwrap())[..16]
        );
        for finding in &report.findings {
            println!(
                "  [{}] {}: {}",
                finding.severity, finding.kind, finding.detail
            );
        }
        let count = |s: &str| report.findings.iter().filter(|f| f.severity == s).count();
        println!(
            "{} failed, {} drift, {} accepted",
            count("failed"),
            count("drift"),
            count("accepted")
        );
    });
    Ok(if failed {
        FAILED
    } else if drift && fail_on == "drift" {
        DRIFT
    } else {
        IN_SYNC
    })
}

fn parse_pairs(items: &[String], what: &str) -> Result<BTreeMap<String, String>, ComputeError> {
    items
        .iter()
        .map(|item| {
            item.split_once('=')
                .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .ok_or_else(|| {
                    coded(
                        "invalid_arguments",
                        format!("--{what} `{item}` is not NAME=VALUE"),
                    )
                })
        })
        .collect()
}

// ---- test: materialize and run suites ---------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub format: String,
    pub stack: String,
    pub lock: String,
    pub harness: Harness,
    pub selection: String,
    pub materialized: Materialization,
    pub suites: Vec<SuiteResult>,
    pub started_at: String,
    pub finished_at: String,
    pub host: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Harness {
    pub repository: String,
    pub commit: String,
    pub dirty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Materialization {
    pub status: String,
    pub sources: Vec<MaterializedSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedSource {
    pub name: String,
    pub commit: String,
    pub tree_matches: bool,
    pub files_match: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteResult {
    pub name: String,
    pub mode: String,
    pub mandatory: bool,
    /// `passed`, `failed`, `missing` (no implementation), or `not-run`.
    pub status: String,
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    pub seconds: u64,
    pub log_sha256: Option<String>,
    #[serde(default)]
    pub artifacts: BTreeMap<String, String>,
    #[serde(default)]
    pub detail: String,
}

/// Clone each source at its locked commit from its declared repository into
/// `<directory>/<name>`, and verify its tree and recorded files against the lock.
pub fn materialize(lock: &StackLock, directory: &Path) -> Materialization {
    let mut sources = vec![];
    for source in &lock.sources {
        let target = directory.join(&source.name);
        let _ = std::fs::remove_dir_all(&target);
        let result = (|| -> Result<(bool, bool), String> {
            std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            run(git(&target).args(["init", "-q"]))?;
            run(git(&target).args([
                "fetch",
                "-q",
                "--depth",
                "1",
                &repository_url(&source.repository),
                &source.commit,
            ]))?;
            run(git(&target).args(["checkout", "-q", "--detach", &source.commit]))?;
            let tree = run(git(&target).args(["rev-parse", "HEAD^{tree}"]))?;
            let tree_matches = String::from_utf8_lossy(&tree).trim() == source.tree;
            let mut files_match = true;
            for name in RECORDED_FILES {
                let path = target.join(name);
                let actual = std::fs::read(&path).ok().map(|bytes| sha256_hex(&bytes));
                if actual.as_ref() != source.files.get(*name) {
                    files_match = false;
                }
            }
            Ok((tree_matches, files_match))
        })();
        sources.push(match result {
            Ok((tree_matches, files_match)) => MaterializedSource {
                name: source.name.clone(),
                commit: source.commit.clone(),
                tree_matches,
                files_match,
                error: None,
            },
            Err(error) => MaterializedSource {
                name: source.name.clone(),
                commit: source.commit.clone(),
                tree_matches: false,
                files_match: false,
                error: Some(error),
            },
        });
    }
    let status = if sources.iter().all(|s| s.tree_matches && s.files_match) {
        "passed"
    } else {
        "failed"
    };
    Materialization {
        status: status.into(),
        sources,
    }
}

fn harness_identity(root: &Path) -> Harness {
    let text = |args: &[&str]| {
        run(git(root).args(args))
            .map(|o| String::from_utf8_lossy(&o).trim().to_owned())
            .unwrap_or_default()
    };
    Harness {
        repository: text(&["config", "--get", "remote.origin.url"]),
        commit: text(&["rev-parse", "HEAD"]),
        dirty: !text(&["status", "--porcelain", "--untracked-files=no"]).is_empty(),
    }
}

fn host() -> BTreeMap<String, String> {
    let version = |program: &str, arg: &str| {
        run(Command::new(program).arg(arg))
            .map(|o| {
                String::from_utf8_lossy(&o)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            })
            .unwrap_or_else(|_| "unavailable".into())
    };
    BTreeMap::from([
        (
            "os".into(),
            format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        ),
        ("git".into(), version("git", "--version")),
        ("node".into(), version("node", "--version")),
        ("npm".into(), version("npm", "--version")),
        ("cargo".into(), version("cargo", "--version")),
    ])
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[derive(Deserialize)]
struct SuiteReport {
    status: String,
    #[serde(default)]
    passed: u64,
    #[serde(default)]
    failed: u64,
    #[serde(default)]
    skipped: u64,
    #[serde(default)]
    artifacts: BTreeMap<String, String>,
    #[serde(default)]
    detail: String,
}

fn run_suite(
    located: &Located,
    suite: &compute_project::Acceptance,
    lock_path: &Path,
    workdir: &Path,
) -> SuiteResult {
    let script = located
        .harness
        .join("compatibility/stack")
        .join(format!("{}.sh", suite.name));
    let out = workdir.join("out").join(&suite.name);
    let _ = std::fs::remove_dir_all(&out);
    let _ = std::fs::create_dir_all(&out);
    let base = SuiteResult {
        name: suite.name.clone(),
        mode: suite.mode.clone(),
        mandatory: suite.mandatory,
        status: "missing".into(),
        passed: 0,
        failed: 0,
        skipped: 0,
        seconds: 0,
        log_sha256: None,
        artifacts: BTreeMap::new(),
        detail: String::new(),
    };
    if !script.is_file() {
        return SuiteResult {
            detail: format!("no implementation at {}", script.display()),
            ..base
        };
    }
    let started = Instant::now();
    let log = out.join("log.txt");
    let status = std::fs::File::create(&log).and_then(|file| {
        let errors = file.try_clone()?;
        Command::new("bash")
            .arg(&script)
            .current_dir(workdir)
            .env("STACK_LOCK", lock_path)
            .env("STACK_SOURCES", workdir.join("sources"))
            .env("STACK_OUT", &out)
            .env("STACK_WORK", workdir)
            .env("STACK_HARNESS", located.harness.join("compatibility/stack"))
            .stdin(std::process::Stdio::null())
            .stdout(file)
            .stderr(errors)
            .status()
    });
    let seconds = started.elapsed().as_secs();
    let log_sha256 = std::fs::read(&log).ok().map(|bytes| sha256_hex(&bytes));
    let report: Option<SuiteReport> = std::fs::read(out.join("result.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let exited_ok = status.as_ref().is_ok_and(|s| s.success());
    match report {
        Some(report) => {
            let passed = exited_ok && report.status == "passed" && report.failed == 0;
            SuiteResult {
                status: if passed { "passed" } else { "failed" }.into(),
                passed: report.passed,
                failed: report.failed,
                skipped: report.skipped,
                seconds,
                log_sha256,
                artifacts: report.artifacts,
                detail: if !exited_ok && report.status == "passed" {
                    format!("the suite reported passed but exited {status:?}")
                } else {
                    report.detail
                },
                ..base
            }
        }
        None => SuiteResult {
            status: "failed".into(),
            seconds,
            log_sha256,
            detail: format!("no result.json; exit {status:?}"),
            ..base
        },
    }
}

fn evidence_path(located: &Located, digest: &str) -> PathBuf {
    located
        .directory
        .join("evidence")
        .join(format!("{}.json", revision_name(digest)))
}

#[allow(clippy::too_many_arguments)]
fn test(
    located: &Located,
    revision_name_arg: &str,
    selected: &[String],
    impacted_from: Option<&str>,
    workdir: Option<PathBuf>,
    record: bool,
    json: bool,
) -> Result<i32, ComputeError> {
    let resolver = ToolResolver::new()?;
    let (lock, label) = revision(located, revision_name_arg, &resolver)?;
    let digest = lock.digest();
    let workdir = match workdir {
        Some(directory) => directory,
        None => std::env::temp_dir().join(format!(
            "compute-stack-{}-{}",
            revision_name(&digest),
            std::process::id()
        )),
    };
    std::fs::create_dir_all(&workdir).map_err(|e| coded("stack_workdir", e.to_string()))?;
    let lock_path = workdir.join("stack.lock");
    write_atomic(&lock_path, &lock.canonical_bytes())?;

    let suites: Vec<_> = match (selected.is_empty(), impacted_from) {
        (false, _) => {
            for name in selected {
                if !located.stack.acceptance.iter().any(|s| &s.name == name) {
                    return Err(coded(
                        "stack_suite_unknown",
                        format!("the stack declares no suite `{name}`"),
                    ));
                }
            }
            located
                .stack
                .acceptance
                .iter()
                .filter(|s| selected.contains(&s.name))
                .collect()
        }
        (true, Some(base)) => {
            let (base_lock, _) = revision(located, base, &resolver)?;
            let change = diff_locks(&located.stack, &base_lock, &lock, Some(&resolver));
            located
                .stack
                .acceptance
                .iter()
                .filter(|s| change.suites.contains(&s.name))
                .collect()
        }
        (true, None) => located.stack.acceptance.iter().collect(),
    };
    let selection = match (selected.is_empty(), impacted_from) {
        (true, None) => "all".to_owned(),
        (true, Some(base)) => format!("impacted-from:{base}"),
        (false, _) => format!("selected:{}", selected.join(",")),
    };
    let started_at = now();
    let harness_before = harness_identity(&located.harness);
    if !json {
        println!(
            "testing {label} ({}) in {}",
            &hex(&digest)[..16],
            workdir.display()
        );
    }
    let materialized = materialize(&lock, &workdir.join("sources"));
    if !json {
        println!(
            "materialized from declared sources: {}",
            materialized.status
        );
    }
    let mut results = vec![];
    for suite in suites {
        let result = if materialized.status == "passed" {
            if !json {
                println!("running {} ({})", suite.name, suite.mode);
            }
            run_suite(located, suite, &lock_path, &workdir)
        } else {
            SuiteResult {
                name: suite.name.clone(),
                mode: suite.mode.clone(),
                mandatory: suite.mandatory,
                status: "not-run".into(),
                passed: 0,
                failed: 0,
                skipped: 0,
                seconds: 0,
                log_sha256: None,
                artifacts: BTreeMap::new(),
                detail: "the revision did not materialize".into(),
            }
        };
        if !json {
            println!(
                "  {}: {} ({} passed, {} failed, {} skipped, {}s) {}",
                result.name,
                result.status,
                result.passed,
                result.failed,
                result.skipped,
                result.seconds,
                result.detail
            );
        }
        results.push(result);
    }
    let harness_after = harness_identity(&located.harness);
    let evidence = Evidence {
        format: EVIDENCE_FORMAT.into(),
        stack: located.stack.name.clone(),
        lock: digest.clone(),
        harness: Harness {
            // The suites ran from the harness as it was at the start: it is clean only if it was
            // committed then and still is, at the same commit.
            dirty: harness_before.dirty
                || harness_after.dirty
                || harness_before.commit != harness_after.commit,
            ..harness_before
        },
        selection,
        materialized,
        suites: results,
        started_at,
        finished_at: now(),
        host: host(),
    };
    let bytes = serde_json::to_vec_pretty(&evidence).expect("serializes");
    write_atomic(&workdir.join("evidence.json"), &bytes)?;
    if record {
        write_atomic(&evidence_path(located, &digest), &bytes)?;
    }
    let passed = evidence.materialized.status == "passed"
        && evidence.suites.iter().all(|s| s.status == "passed");
    print(json, &evidence, || {
        println!(
            "{}: {}",
            if passed { "PASSED" } else { "FAILED" },
            &hex(&digest)[..16]
        );
        if record {
            println!(
                "evidence recorded at {}",
                evidence_path(located, &digest).display()
            );
        }
    });
    Ok(if passed { IN_SYNC } else { FAILED })
}

// ---- promote ------------------------------------------------------------------------------------

/// The SSH signature namespace of a stack approval, so a signature made for anything else (a
/// commit, a file) is never accepted as one.
const APPROVAL_NAMESPACE: &str = "compute-stack-approval";
/// Who may approve: OpenSSH's allowed-signers format, beside the stack manifest. When it is
/// present, an approval counts only signed by one of them, under the `--approved-by` identity.
const ALLOWED_SIGNERS: &str = "allowed_signers";

fn ssh_keygen() -> Command {
    Command::new("ssh-keygen")
}

/// Sign `file` with `key`, writing `<file>.sig`, then verify that signature against the stack's
/// approvers under `identity`. Either failing leaves no signature behind.
fn sign_approval(
    file: &Path,
    key: &Path,
    signers: &Path,
    identity: &str,
) -> Result<PathBuf, String> {
    let signature = PathBuf::from(format!("{}.sig", file.display()));
    let _ = std::fs::remove_file(&signature);
    run(ssh_keygen()
        .args(["-q", "-Y", "sign", "-n", APPROVAL_NAMESPACE, "-f"])
        .arg(key)
        .arg(file))
    .map_err(|e| format!("could not sign the approval: {e}"))?;
    let verified = std::fs::File::open(file)
        .map_err(|e| e.to_string())
        .and_then(|approval| {
            run(ssh_keygen()
                .args(["-Y", "verify", "-f"])
                .arg(signers)
                .args(["-I", identity, "-n", APPROVAL_NAMESPACE, "-s"])
                .arg(&signature)
                .stdin(approval))
        });
    match verified {
        Ok(_) => Ok(signature),
        Err(why) => {
            let _ = std::fs::remove_file(&signature);
            Err(format!(
                "the signature does not verify as {identity} against {}: {why}",
                signers.display()
            ))
        }
    }
}

fn promote(
    located: &Located,
    candidate: &str,
    approve: bool,
    approved_by: Option<&str>,
    sign_key: Option<&Path>,
    json: bool,
) -> Result<i32, ComputeError> {
    let resolver = ToolResolver::new()?;
    let (lock, label) = revision(located, candidate, &resolver)?;
    let digest = lock.digest();
    let mut refusals = vec![];
    if lock.stack.assembly != located.stack.assembly_digest() {
        refusals.push("the manifest changed after this candidate was resolved".to_owned());
    }
    for violation in lock.violations() {
        refusals.push(format!(
            "relationship {} is violated: {}",
            violation.name, violation.detail
        ));
    }
    for finding in lock.violated_findings() {
        refusals.push(format!(
            "{}/{} {} in {}: {}",
            finding.kind, finding.check, finding.package, finding.consumer, finding.detail
        ));
    }
    let evidence_file = evidence_path(located, &digest);
    let evidence: Option<Evidence> = std::fs::read(&evidence_file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    match &evidence {
        None => refusals.push(format!(
            "no recorded evidence at {} (run `compute stack test {} --record`)",
            evidence_file.display(),
            &hex(&digest)[..16]
        )),
        Some(evidence) => {
            if evidence.lock != digest {
                refusals.push("the recorded evidence is for another lock".into());
            }
            if evidence.harness.dirty {
                refusals.push("the evidence was produced by an uncommitted harness".into());
            }
            if evidence.materialized.status != "passed" {
                refusals.push("the revision did not materialize from its declared sources".into());
            }
            for suite in located.stack.acceptance.iter().filter(|s| s.mandatory) {
                match evidence.suites.iter().find(|s| s.name == suite.name) {
                    None => refusals.push(format!("mandatory suite {} was not run", suite.name)),
                    Some(result) if result.status != "passed" => {
                        refusals.push(format!("mandatory suite {} {}", suite.name, result.status))
                    }
                    _ => {}
                }
            }
        }
    }
    #[derive(Serialize)]
    struct Promotion<'a> {
        candidate: &'a str,
        lock: &'a str,
        promoted: bool,
        refusals: &'a [String],
    }
    if !refusals.is_empty() {
        print(
            json,
            &Promotion {
                candidate: &label,
                lock: &digest,
                promoted: false,
                refusals: &refusals,
            },
            || {
                println!("not promoted: {}", &hex(&digest)[..16]);
                for refusal in &refusals {
                    println!("  {refusal}");
                }
            },
        );
        return Ok(FAILED);
    }
    let Some(approved_by) = approved_by.filter(|_| approve) else {
        print(
            json,
            &Promotion {
                candidate: &label,
                lock: &digest,
                promoted: false,
                refusals: &["approval required: --approve --approved-by NAME".into()],
            },
            || {
                println!(
                    "{} passed every mandatory check; promoting it needs --approve --approved-by NAME",
                    &hex(&digest)[..16]
                );
            },
        );
        return Ok(APPROVAL_REQUIRED);
    };
    let signers = located.directory.join(ALLOWED_SIGNERS);
    if signers.is_file() && sign_key.is_none() {
        print(
            json,
            &Promotion {
                candidate: &label,
                lock: &digest,
                promoted: false,
                refusals: &[format!(
                    "approval must be signed: this stack names its approvers in {}; pass --sign-key",
                    signers.display()
                )],
            },
            || {
                println!(
                    "not promoted: approvals of this stack are signed (--sign-key KEY); see {}",
                    signers.display()
                );
            },
        );
        return Ok(APPROVAL_REQUIRED);
    }
    let current = located.directory.join(LOCK_FILE);
    let previous = if current.is_file() {
        Some(read_lock(&current)?)
    } else {
        None
    };
    let history = located.directory.join("history");
    if let Some(previous) = &previous {
        let kept = history.join(format!("{}.lock", revision_name(&previous.digest())));
        if !kept.is_file() {
            write_atomic(&kept, &previous.canonical_bytes())?;
        }
    }
    let evidence_bytes =
        std::fs::read(&evidence_file).map_err(|e| coded("stack_evidence", e.to_string()))?;
    let approval = serde_json::json!({
        "format": APPROVAL_FORMAT,
        "lock": digest,
        "approved_by": approved_by,
        "approved_at": now(),
        "evidence": sha256_hex(&evidence_bytes),
        "previous": previous.as_ref().map(StackLock::digest),
    });
    let approval_file = history.join(format!("{}.approval.json", revision_name(&digest)));
    write_atomic(
        &approval_file,
        &serde_json::to_vec_pretty(&approval).expect("serializes"),
    )?;
    if let Some(key) = sign_key {
        let signers = located.directory.join(ALLOWED_SIGNERS);
        if !signers.is_file() {
            let _ = std::fs::remove_file(&approval_file);
            return Err(coded(
                "stack_approval_signature",
                format!(
                    "--sign-key needs the stack's approvers in {}",
                    signers.display()
                ),
            ));
        }
        if let Err(why) = sign_approval(&approval_file, key, &signers, approved_by) {
            let _ = std::fs::remove_file(&approval_file);
            return Err(coded("stack_approval_signature", why));
        }
    }
    write_atomic(
        &history.join(format!("{}.lock", revision_name(&digest))),
        &lock.canonical_bytes(),
    )?;
    write_atomic(&current, &lock.canonical_bytes())?;
    print(json, &approval, || {
        println!(
            "promoted {} as the known-good {} stack (approved by {approved_by})",
            &hex(&digest)[..16],
            located.stack.name
        );
    });
    Ok(IN_SYNC)
}

// ---- reproduce ----------------------------------------------------------------------------------

fn reproduce(
    located: &Located,
    name: &str,
    into: Option<PathBuf>,
    json: bool,
) -> Result<i32, ComputeError> {
    let resolver = ToolResolver::new()?;
    let (lock, label) = revision(located, name, &resolver)?;
    let digest = lock.digest();
    let into = into.unwrap_or_else(|| {
        std::env::temp_dir().join(format!(
            "compute-stack-reproduce-{}",
            revision_name(&digest)
        ))
    });
    std::fs::create_dir_all(&into).map_err(|e| coded("stack_workdir", e.to_string()))?;
    let materialized = materialize(&lock, &into.join("sources"));
    let packages_dir = into.join("packages");
    let _ = std::fs::create_dir_all(&packages_dir);
    #[derive(Serialize)]
    struct PackageCheck {
        package: String,
        version: String,
        integrity_matches: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    }
    let mut packages = vec![];
    for package in &lock.packages {
        let checked = (|| -> Result<bool, String> {
            let out = run(Command::new("npm")
                .args([
                    "pack",
                    &format!("{}@{}", package.package, package.version),
                    "--json",
                    "--pack-destination",
                ])
                .arg(&packages_dir))?;
            let report: serde_json::Value =
                serde_json::from_slice(&out).map_err(|e| e.to_string())?;
            let file = report[0]["filename"]
                .as_str()
                .ok_or("npm pack reported no file")?;
            let bytes = std::fs::read(packages_dir.join(file)).map_err(|e| e.to_string())?;
            // Recompute the registry's integrity from the bytes, independently of npm's report.
            use sha2::Digest as _;
            let computed = format!("sha512-{}", base64_encode(&sha2::Sha512::digest(&bytes)));
            Ok(computed == package.integrity)
        })();
        packages.push(match checked {
            Ok(ok) => PackageCheck {
                package: package.package.clone(),
                version: package.version.clone(),
                integrity_matches: ok,
                error: None,
            },
            Err(error) => PackageCheck {
                package: package.package.clone(),
                version: package.version.clone(),
                integrity_matches: false,
                error: Some(error),
            },
        });
    }
    let reproduced =
        materialized.status == "passed" && packages.iter().all(|p| p.integrity_matches);
    #[derive(Serialize)]
    struct Reproduction {
        revision: String,
        lock: String,
        directory: String,
        reproduced: bool,
        sources: Materialization,
        packages: Vec<PackageCheck>,
    }
    let report = Reproduction {
        revision: label,
        lock: digest.clone(),
        directory: into.display().to_string(),
        reproduced,
        sources: materialized,
        packages,
    };
    print(json, &report, || {
        println!(
            "revision {} ({}) into {}",
            report.revision,
            &hex(&digest)[..16],
            report.directory
        );
        for source in &report.sources.sources {
            println!(
                "  source {} @{}: tree {} files {}{}",
                source.name,
                short_commit(&source.commit),
                ok(source.tree_matches),
                ok(source.files_match),
                source
                    .error
                    .as_deref()
                    .map(|e| format!(" ({e})"))
                    .unwrap_or_default()
            );
        }
        for package in &report.packages {
            println!(
                "  package {}@{}: integrity {}{}",
                package.package,
                package.version,
                ok(package.integrity_matches),
                package
                    .error
                    .as_deref()
                    .map(|e| format!(" ({e})"))
                    .unwrap_or_default()
            );
        }
        println!(
            "{}",
            if reproduced {
                "REPRODUCED"
            } else {
                "NOT REPRODUCED"
            }
        );
    });
    Ok(if reproduced { IN_SYNC } else { FAILED })
}

fn ok(value: bool) -> &'static str {
    if value { "matches" } else { "DIFFERS" }
}

fn print_inventory(inventory: &compute_project::Inventory) {
    for entry in &inventory.packages {
        let canonical = &entry.canonical;
        println!(
            "{}  canonical {}@{}:{} {}{}",
            entry.package,
            canonical.source,
            short_commit(&canonical.commit),
            canonical.path.as_deref().unwrap_or("?"),
            canonical.version.as_deref().unwrap_or("-"),
            entry
                .stack_version
                .as_deref()
                .map(|v| format!("  (stack runs {v})"))
                .unwrap_or_default()
        );
        for used in &entry.uses {
            println!(
                "  {:<17} {:<9} {:<11} {:<7} {} {} {}{}{}",
                used.consumer,
                used.kind,
                used.scope,
                used.context,
                if used.direct {
                    "direct    "
                } else {
                    "transitive"
                },
                used.version
                    .as_deref()
                    .or(used.specifier.as_deref())
                    .unwrap_or("-"),
                used.origin,
                used.matches
                    .as_deref()
                    .map(|m| format!(" [{m}]"))
                    .unwrap_or_default(),
                format!("  {}", used.path)
            );
        }
        if !entry.unlocated.is_empty() {
            println!(
                "  expected by {} but not sources of this stack (not inspected): {}",
                canonical.source,
                entry.unlocated.join(", ")
            );
        }
    }
    for finding in &inventory.findings {
        println!(
            "[{}] {}/{} {} in {}{}: {}{}",
            finding.status,
            finding.kind,
            finding.check,
            finding.package,
            finding.consumer,
            if finding.path.is_empty() {
                String::new()
            } else {
                format!(":{}", finding.path)
            },
            finding.detail,
            finding
                .accepted_by
                .as_deref()
                .map(|r| format!(" [accepted: {r}]"))
                .unwrap_or_default()
        );
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn stack(command: StackCommand) -> compute_core::Result<()> {
    let located = locate(&command.stack)?;
    let json = command.json;
    let code = match command.command {
        StackCommands::Check {
            offline,
            workspace,
            fail_on,
        } => check(&located, offline, workspace.as_deref(), &fail_on, json)?,
        StackCommands::Inventory {
            revision: name,
            package,
            consumer,
            status,
        } => {
            let resolver = ToolResolver::new()?;
            let (lock, _) = revision(&located, &name, &resolver)?;
            let mut inventory = lock.inventory.clone();
            inventory
                .packages
                .retain(|p| package.as_ref().is_none_or(|w| &p.package == w));
            for entry in &mut inventory.packages {
                entry
                    .uses
                    .retain(|u| consumer.as_ref().is_none_or(|c| &u.consumer == c));
            }
            inventory.findings.retain(|f| {
                package.as_ref().is_none_or(|w| &f.package == w)
                    && consumer.as_ref().is_none_or(|c| &f.consumer == c)
                    && status.as_ref().is_none_or(|s| &f.status == s)
            });
            print(json, &inventory, || print_inventory(&inventory));
            IN_SYNC
        }
        StackCommands::Consumers {
            package,
            revision: name,
            target,
        } => {
            let resolver = ToolResolver::new()?;
            let (lock, _) = revision(&located, &name, &resolver)?;
            let Some(entry) = lock
                .inventory
                .packages
                .iter()
                .find(|p| p.package == package)
            else {
                return Err(coded("stack_package_unknown", format!("`{package}` is not provided by a source of this stack (npm:<name> or cargo:<crate>)")).into());
            };
            match target {
                None => {
                    let mut only = lock.inventory.clone();
                    only.packages = vec![entry.clone()];
                    only.findings.retain(|f| f.package == package);
                    print(json, &only, || print_inventory(&only));
                    IN_SYNC
                }
                Some(target) => {
                    let plan = upgrade_plan(&lock.inventory, &located.stack, &package, &target)
                        .expect("present");
                    print(json, &plan, || {
                        println!(
                            "{package} -> {target}: {} of {} consumers at target, {} held by a declared reason, {} outstanding",
                            plan.at_target,
                            plan.consumers.len(),
                            plan.accepted,
                            plan.outstanding
                        );
                        for consumer in &plan.consumers {
                            println!("  {} [{}]", consumer.consumer, consumer.status);
                            for action in &consumer.actions {
                                println!("    - {action}");
                            }
                        }
                    });
                    if plan.outstanding == 0 {
                        IN_SYNC
                    } else {
                        DRIFT
                    }
                }
            }
        }
        StackCommands::Diff { from, to } => {
            let resolver = ToolResolver::new()?;
            let (a, _) = revision(&located, &from, &resolver)?;
            let (b, _) = revision(&located, &to, &resolver)?;
            let change = diff_locks(&located.stack, &a, &b, Some(&resolver));
            print(json, &change, || print_diff(&change));
            IN_SYNC
        }
        StackCommands::Update {
            candidate,
            sources,
            packages,
        } => {
            if !candidate {
                return Err(coded(
                    "stack_update_requires_candidate",
                    "an update resolves a candidate and never changes the known-good lock: pass --candidate",
                ));
            }
            let overrides = Overrides {
                sources: parse_pairs(&sources, "source")?,
                packages: parse_pairs(&packages, "package")?,
            };
            let resolver = ToolResolver::new()?;
            let lock = resolve_stack(&located.stack, &overrides, &resolver)
                .map_err(|e| coded("stack_unresolved", e.to_string()))?;
            let digest = lock.digest();
            let path = located
                .directory
                .join("candidates")
                .join(format!("{}.lock", revision_name(&digest)));
            write_atomic(&path, &lock.canonical_bytes())?;
            let current = located.directory.join(LOCK_FILE);
            let change = current
                .is_file()
                .then(|| read_lock(&current))
                .transpose()?
                .map(|base| diff_locks(&located.stack, &base, &lock, Some(&resolver)));
            #[derive(Serialize)]
            struct Candidate<'a> {
                candidate: String,
                file: String,
                violations: Vec<&'a compute_project::RelationshipResult>,
                diff: Option<LockDiff>,
            }
            let report = Candidate {
                candidate: digest.clone(),
                file: path.display().to_string(),
                violations: lock.violations(),
                diff: change,
            };
            print(json, &report, || {
                println!(
                    "candidate {} written to {}",
                    &hex(&digest)[..16],
                    report.file
                );
                for violation in &report.violations {
                    println!("  VIOLATED {}: {}", violation.name, violation.detail);
                }
                match &report.diff {
                    Some(change) => print_diff(change),
                    None => println!("there is no known-good lock yet; every suite applies"),
                }
            });
            IN_SYNC
        }
        StackCommands::Test {
            revision: name,
            suites,
            impacted_from,
            workdir,
            record,
        } => test(
            &located,
            &name,
            &suites,
            impacted_from.as_deref(),
            workdir,
            record,
            json,
        )?,
        StackCommands::Promote {
            candidate,
            approve,
            approved_by,
            sign_key,
        } => promote(
            &located,
            &candidate,
            approve,
            approved_by.as_deref(),
            sign_key.as_deref(),
            json,
        )?,
        StackCommands::Reproduce {
            revision: name,
            into,
        } => reproduce(&located, &name, into, json)?,
    };
    if code != IN_SYNC {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// An approval counts only when signed by a named approver under the approver's own identity,
    /// in the approval namespace; a refused signature leaves nothing behind.
    #[test]
    fn an_approval_is_signed_and_verified_against_the_stacks_approvers() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("approver");
        let made = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "approver", "-f"])
            .arg(&key)
            .status()
            .unwrap();
        assert!(made.success());
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        let signers = dir.path().join(ALLOWED_SIGNERS);
        std::fs::write(
            &signers,
            format!("randy@example.com namespaces=\"{APPROVAL_NAMESPACE}\" {public}"),
        )
        .unwrap();
        let approval = dir.path().join("x.approval.json");
        std::fs::write(&approval, b"{\"lock\":\"sha256:00\"}").unwrap();

        let signature = sign_approval(&approval, &key, &signers, "randy@example.com").unwrap();
        assert!(signature.is_file());

        // Another identity is refused, and leaves no signature.
        let refused = sign_approval(&approval, &key, &signers, "someone@example.com").unwrap_err();
        assert!(refused.contains("does not verify"), "{refused}");
        assert!(!signature.exists());
    }

    use super::*;

    #[test]
    fn integrity_is_base64_sha512() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"hi"), "aGk=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn repository_identities_map_to_urls() {
        assert_eq!(
            repository_url("github:rkendel1/compute"),
            "https://github.com/rkendel1/compute"
        );
        assert_eq!(repository_url("file:/srv/mirror"), "/srv/mirror");
    }
}
