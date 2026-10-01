//! The GitHub Actions worker, end to end through Compute's real shell
//! runtime, against a fake runner archive and a mocked GitHub. No GitHub
//! credential, network, or real runner is needed.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use compute_core::{ExecutionControl, ExecutionStatus};
use compute_runtime::Compute;
use compute_worker_github::{
    GitHubApi, Repository, RunnerRun, RunnerSpec, RunnerWorker, Secret, WorkerError, scrub,
};
use sha2::{Digest, Sha256};

const REGISTRATION_TOKEN: &str = "AREGTOKEN-0123456789-do-not-leak";
const CREDENTIAL: &str = "ghp_CREDENTIAL0123456789-do-not-leak";

struct MockApi {
    outcome: Mutex<Option<Result<(), WorkerError>>>,
    calls: Mutex<Vec<String>>,
}

impl MockApi {
    fn ok() -> Self {
        Self {
            outcome: Mutex::new(None),
            calls: Mutex::new(vec![]),
        }
    }

    fn failing(error: WorkerError) -> Self {
        Self {
            outcome: Mutex::new(Some(Err(error))),
            calls: Mutex::new(vec![]),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl GitHubApi for &MockApi {
    async fn registration_token(
        &self,
        repository: &Repository,
        credential: &Secret,
    ) -> Result<Secret, WorkerError> {
        assert_eq!(credential.expose(), CREDENTIAL);
        self.calls.lock().unwrap().push(repository.to_string());
        match self.outcome.lock().unwrap().take() {
            Some(Err(error)) => Err(error),
            _ => Ok(Secret::new(REGISTRATION_TOKEN)),
        }
    }
}

/// A fake runner release: `config.sh` and `run.sh` with the behaviour a test
/// needs, packed as the real archive is (`tar.gz`), plus where it records
/// what it saw.
struct FakeRunner {
    _root: tempfile::TempDir,
    archive: PathBuf,
    sha256: String,
    /// Written by `config.sh`: the arguments it received.
    config_args: PathBuf,
    /// Written by `run.sh` when it starts.
    ran: PathBuf,
    /// The pid of a descendant `run.sh` left behind, when it does.
    descendant: PathBuf,
}

impl FakeRunner {
    fn new(config_extra: &str, run_body: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("tree");
        std::fs::create_dir_all(tree.join("bin")).unwrap();
        let config_args = root.path().join("config-args.txt");
        let ran = root.path().join("ran");
        let descendant = root.path().join("descendant.pid");
        let config = format!(
            "#!/bin/sh\necho \"$@\" > {args}\n{extra}\nexit 0\n",
            args = config_args.display(),
            extra = config_extra,
        );
        let run = format!(
            "#!/bin/sh\n\
             if [ -n \"${{ACTIONS_RUNNER_INPUT_TOKEN:-}}\" ]; then echo token-inherited > {ran}.leak; fi\n\
             echo started > {ran}\n\
             mkdir -p _diag\n\
             {body}\n",
            ran = ran.display(),
            body = run_body,
        );
        std::fs::write(tree.join("config.sh"), config).unwrap();
        std::fs::write(tree.join("run.sh"), run).unwrap();
        std::fs::write(
            tree.join("bin/installdependencies.sh"),
            "#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        for script in ["config.sh", "run.sh", "bin/installdependencies.sh"] {
            use std::os::unix::fs::PermissionsExt;
            let path = tree.join(script);
            let mut mode = std::fs::metadata(&path).unwrap().permissions();
            mode.set_mode(0o755);
            std::fs::set_permissions(&path, mode).unwrap();
        }
        let archive = root.path().join("actions-runner.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("czf")
            .arg(&archive)
            .arg("-C")
            .arg(&tree)
            .arg(".")
            .status()
            .unwrap();
        assert!(status.success());
        let sha256 = format!("{:x}", Sha256::digest(std::fs::read(&archive).unwrap()));
        Self {
            _root: root,
            archive,
            sha256,
            config_args,
            ran,
            descendant,
        }
    }

    fn succeeding() -> Self {
        Self::new(
            "",
            "echo \"[x INFO Runner] Running job: build\" >> _diag/Runner_1.log\n\
             echo \"[x INFO Runner] Job build completed with result: Succeeded\" >> _diag/Runner_1.log\n\
             exit 0",
        )
    }

    fn spec(&self) -> RunnerSpec {
        let mut spec = RunnerSpec::new(
            Repository::parse("rkendel1/compute").unwrap(),
            "2.331.0",
            self.sha256.clone(),
        )
        .unwrap();
        spec.download_url = Some(format!("file://{}", self.archive.display()));
        spec.labels = vec!["compute".into(), "ephemeral".into()];
        spec.name = Some("compute-test".into());
        spec.timeout = Duration::from_secs(60);
        spec
    }
}

fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z"),
        Err(_) => false,
    }
}

fn pid_of(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

async fn run_with(
    api: &MockApi,
    spec: &RunnerSpec,
    control: &ExecutionControl,
) -> Result<RunnerRun, WorkerError> {
    RunnerWorker::new(api, Compute::new())
        .run(spec, &Secret::new(CREDENTIAL), control)
        .await
}

fn assert_no_secret(label: &str, text: &str) {
    assert!(
        !text.contains(REGISTRATION_TOKEN) && !text.contains(CREDENTIAL),
        "{label} leaked a secret: {text}"
    );
}

fn everything(run: &RunnerRun) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}",
        serde_json::to_string(&run.report).unwrap(),
        serde_json::to_string(&run.result).unwrap(),
        String::from_utf8_lossy(
            &run.result
                .receipt
                .as_ref()
                .unwrap()
                .encoded_bytes()
                .unwrap()
        ),
        run.result.stdout.text,
        run.result.stderr.text,
    )
}

#[tokio::test]
async fn registers_an_ephemeral_runner_runs_one_job_and_cleans_up() {
    let runner = FakeRunner::succeeding();
    let api = MockApi::ok();
    let run = run_with(&api, &runner.spec(), &ExecutionControl::new())
        .await
        .unwrap();

    assert_eq!(api.calls(), ["rkendel1/compute"], "one registration token");
    assert_eq!(run.result.status, ExecutionStatus::Completed);
    assert_eq!(run.result.exit_code, Some(0));

    // Registered ephemeral, unattended, pinned, against the repository, with
    // the requested name and labels; the token is not an argument.
    let args = std::fs::read_to_string(&runner.config_args).unwrap();
    for expected in [
        "--unattended",
        "--ephemeral",
        "--disableupdate",
        "--url https://github.com/rkendel1/compute",
        "--name compute-test",
        "--labels compute,ephemeral",
    ] {
        assert!(args.contains(expected), "{expected} not in {args}");
    }
    assert_no_secret("config args", &args);

    // The runner started, and did not inherit the registration token.
    assert!(runner.ran.exists());
    assert!(!runner.ran.with_extension("leak").exists());

    // Everything a reader needs to reproduce the run.
    let report = &run.report;
    assert_eq!(report.worker, "github-actions");
    assert_eq!(report.repository, "rkendel1/compute");
    assert_eq!(report.runner.version, "2.331.0");
    assert!(report.runner.ephemeral);
    assert_eq!(report.runner.name, "compute-test");
    assert_eq!(report.status, ExecutionStatus::Completed);
    assert_eq!(report.exit_code, Some(0));
    assert!(report.started_at.is_some() && report.finished_at.is_some());
    assert_eq!(report.stage, "done");
    assert_eq!(report.job.name.as_deref(), Some("build"));
    assert_eq!(report.job.result.as_deref(), Some("Succeeded"));
    assert!(report.environment.starts_with("shell"));
    assert_eq!(report.cleanup.workspace_removed, Some(true));

    // Compute's own receipt is the record: it names the shell runtime, the
    // environment variable names (never values), and the metadata output.
    let receipt = run.result.receipt.as_ref().expect("a receipt");
    assert!(report.receipt_hash.is_some());
    assert_eq!(receipt.execution.status, ExecutionStatus::Completed);
    assert!(
        receipt
            .outputs
            .iter()
            .any(|output| output.path == Path::new("runner-metadata.json"))
    );
    let receipt_json = serde_json::to_string(receipt).unwrap();
    assert!(
        receipt_json.contains("ACTIONS_RUNNER_INPUT_TOKEN"),
        "the name is recorded"
    );
    assert_no_secret("everything", &everything(&run));
}

#[tokio::test]
async fn a_runner_that_echoes_the_token_is_scrubbed_from_logs_and_results() {
    let runner = FakeRunner::new(
        "echo \"registering with $ACTIONS_RUNNER_INPUT_TOKEN\"\n\
         echo \"registering with $ACTIONS_RUNNER_INPUT_TOKEN\" >&2",
        "exit 0",
    );
    let run = run_with(&MockApi::ok(), &runner.spec(), &ExecutionControl::new())
        .await
        .unwrap();
    assert_eq!(run.result.status, ExecutionStatus::Completed);
    assert!(run.result.stdout.text.contains("[REDACTED]"));
    assert!(run.result.stderr.text.contains("[REDACTED]"));
    assert_no_secret("results", &everything(&run));
}

#[tokio::test]
async fn a_checksum_mismatch_refuses_to_run_the_archive() {
    let runner = FakeRunner::succeeding();
    let mut spec = runner.spec();
    spec.runner_sha256 = "0".repeat(64);
    let run = run_with(&MockApi::ok(), &spec, &ExecutionControl::new())
        .await
        .unwrap();
    assert_eq!(run.report.exit_code, Some(71));
    assert_eq!(run.report.stage, "verify");
    assert!(!runner.config_args.exists(), "nothing was configured");
    assert!(!runner.ran.exists(), "nothing was run");
    assert_eq!(run.report.cleanup.workspace_removed, Some(true));
}

#[tokio::test]
async fn a_registration_failure_never_starts_the_runner() {
    let runner = FakeRunner::new("exit 1", "exit 0");
    let run = run_with(&MockApi::ok(), &runner.spec(), &ExecutionControl::new())
        .await
        .unwrap();
    assert_eq!(run.report.exit_code, Some(75));
    assert_eq!(run.report.stage, "configure");
    assert!(!runner.ran.exists());
    assert_eq!(run.report.cleanup.workspace_removed, Some(true));
}

#[tokio::test]
async fn a_failing_runner_reports_its_exit_status_and_is_cleaned_up() {
    let runner = FakeRunner::new("", "exit 3");
    let run = run_with(&MockApi::ok(), &runner.spec(), &ExecutionControl::new())
        .await
        .unwrap();
    assert_eq!(run.report.exit_code, Some(3));
    // The process ended on its own: its failure is the exit status.
    assert_eq!(run.report.status, ExecutionStatus::Completed);
    assert_eq!(run.report.stage, "done");
    assert_eq!(run.report.cleanup.workspace_removed, Some(true));
}

/// A runner that leaves a descendant behind, as the real one does.
fn lingering(runner_pid: &Path) -> String {
    format!("sleep 300 &\necho $! > {}\nwait", runner_pid.display())
}

#[tokio::test]
async fn a_runner_that_exits_leaves_no_descendant_behind() {
    let probe = tempfile::tempdir().unwrap();
    let pid_file = probe.path().join("pid");
    // The runner exits cleanly but leaves a process that shares its output.
    let runner = FakeRunner::new(
        "",
        &format!("sleep 300 &\necho $! > {}\nexit 0", pid_file.display()),
    );
    let started = std::time::Instant::now();
    let run = run_with(&MockApi::ok(), &runner.spec(), &ExecutionControl::new())
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "waited for the descendant"
    );
    assert_eq!(run.report.status, ExecutionStatus::Completed);
    assert_eq!(run.report.exit_code, Some(0));
    let pid = pid_of(&pid_file).expect("the descendant started");
    assert!(!alive(pid), "the descendant {pid} outlived the runner");
    assert_eq!(run.report.cleanup.workspace_removed, Some(true));
}

#[tokio::test]
async fn a_timeout_ends_the_runner_and_everything_it_started() {
    let probe = tempfile::tempdir().unwrap();
    let pid_file = probe.path().join("pid");
    let runner = FakeRunner::new("", &lingering(&pid_file));
    let mut spec = runner.spec();
    spec.timeout = Duration::from_secs(3);
    let run = run_with(&MockApi::ok(), &spec, &ExecutionControl::new())
        .await
        .unwrap();
    assert_eq!(run.report.status, ExecutionStatus::TimedOut);
    let pid = pid_of(&pid_file).expect("the descendant started");
    assert!(!alive(pid), "the descendant {pid} outlived the timeout");
    assert_eq!(run.report.stage, "run");
    assert_eq!(run.report.cleanup.workspace_removed, Some(true));
    let _ = &runner.descendant;
}

#[tokio::test]
async fn cancellation_ends_the_runner_and_everything_it_started() {
    let probe = tempfile::tempdir().unwrap();
    let pid_file = probe.path().join("pid");
    let runner = FakeRunner::new("", &lingering(&pid_file));
    let spec = runner.spec();
    let control = ExecutionControl::new();
    let canceller = control.clone();
    let watched = pid_file.clone();
    tokio::spawn(async move {
        for _ in 0..200 {
            if watched.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        canceller.cancel();
    });
    let run = run_with(&MockApi::ok(), &spec, &control).await.unwrap();
    assert_eq!(run.report.status, ExecutionStatus::Cancelled);
    let pid = pid_of(&pid_file).expect("the descendant started");
    assert!(
        !alive(pid),
        "the descendant {pid} outlived the cancellation"
    );
    assert_eq!(run.report.cleanup.workspace_removed, Some(true));
}

#[tokio::test]
async fn an_unknown_repository_fails_before_anything_is_provisioned() {
    let runner = FakeRunner::succeeding();
    let api = MockApi::failing(WorkerError::RepositoryNotFound("rkendel1/compute".into()));
    let error = match run_with(&api, &runner.spec(), &ExecutionControl::new()).await {
        Ok(_) => panic!("must fail"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "repository_not_found");
    assert!(!runner.config_args.exists() && !runner.ran.exists());
}

#[tokio::test]
async fn a_missing_credential_is_refused_before_github_is_contacted() {
    let runner = FakeRunner::succeeding();
    let api = MockApi::ok();
    let error = RunnerWorker::new(&api, Compute::new())
        .run(&runner.spec(), &Secret::new(""), &ExecutionControl::new())
        .await
        .err()
        .expect("must fail");
    assert_eq!(error.code(), "missing_credential");
    assert!(api.calls().is_empty());
}

#[tokio::test]
async fn an_invalid_specification_is_refused_before_github_is_contacted() {
    let runner = FakeRunner::succeeding();
    let api = MockApi::ok();
    for mutate in [
        |spec: &mut RunnerSpec| spec.labels = vec!["a;touch /tmp/x".into()],
        |spec: &mut RunnerSpec| spec.name = Some("$(id)".into()),
        |spec: &mut RunnerSpec| spec.runner_sha256 = "abc".into(),
        |spec: &mut RunnerSpec| spec.runner_version = "latest".into(),
        |spec: &mut RunnerSpec| spec.server_url = "http://github.example".into(),
        |spec: &mut RunnerSpec| spec.download_url = Some("ftp://x".into()),
        |spec: &mut RunnerSpec| spec.timeout = Duration::ZERO,
    ] {
        let mut spec = runner.spec();
        mutate(&mut spec);
        let error = run_with(&api, &spec, &ExecutionControl::new())
            .await
            .err()
            .expect("must fail");
        assert_eq!(error.code(), "invalid_runner_spec", "{error}");
    }
    assert!(api.calls().is_empty());
}

#[test]
fn the_default_download_url_is_the_pinned_release_asset() {
    let mut spec =
        RunnerSpec::new(Repository::parse("o/r").unwrap(), "2.331.0", "0".repeat(64)).unwrap();
    spec.os = compute_worker_github::RunnerOs::Linux;
    spec.arch = compute_worker_github::RunnerArch::X64;
    assert_eq!(
        spec.download_url(),
        "https://github.com/actions/runner/releases/download/v2.331.0/actions-runner-linux-x64-2.331.0.tar.gz"
    );
}

#[test]
fn scrub_is_available_to_callers() {
    let secret = Secret::new(REGISTRATION_TOKEN);
    assert_no_secret(
        "scrubbed",
        &scrub(&format!("x {REGISTRATION_TOKEN}"), &[&secret]),
    );
}
