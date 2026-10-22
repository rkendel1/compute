//! The agent as a VM runs it, over a Unix socket instead of vsock: the same
//! protocol, the same process handling.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use compute_guest::{Request, exec_client, ping};

struct Agent(std::process::Child, PathBuf);

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(directory: &Path) -> Agent {
    let socket = directory.join("agent.sock");
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_compute-guest"))
        .arg("--unix")
        .arg(&socket)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "the agent never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    Agent(child, socket)
}

async fn run(socket: &Path, request: Request) -> (i32, Vec<u8>, Vec<u8>) {
    let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let code = exec_client(stream, &request, &mut stdout, &mut stderr)
        .await
        .unwrap();
    (code, stdout, stderr)
}

fn exec(argv: &[&str], cwd: &Path) -> Request {
    Request::Exec {
        argv: argv.iter().map(|part| part.to_string()).collect(),
        env: BTreeMap::new(),
        cwd: cwd.display().to_string(),
        stdin: vec![],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_runs_commands_with_their_input_environment_and_status() {
    let directory = tempfile::tempdir().unwrap();
    let agent = start(directory.path());
    let stream = tokio::net::UnixStream::connect(&agent.1).await.unwrap();
    assert_eq!(ping(stream).await.unwrap(), compute_guest::PROTOCOL);

    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let (code, stdout, stderr) = run(
        &agent.1,
        Request::Exec {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "read line; echo \"$line $GREETING $HOME\"; pwd; echo oops >&2; exit 3".into(),
            ],
            env: BTreeMap::from([("GREETING".to_owned(), "hello".to_owned())]),
            cwd: workspace.display().to_string(),
            stdin: b"from-stdin\n".to_vec(),
        },
    )
    .await;
    assert_eq!(code, 3);
    let workspace = workspace.display().to_string();
    assert_eq!(
        String::from_utf8(stdout).unwrap(),
        format!("from-stdin hello {workspace}\n{workspace}\n")
    );
    assert_eq!(stderr, b"oops\n");

    // Only what the request names reaches the command.
    // SAFETY: the variable is set before the agent could read it, in a test.
    let (_, stdout, _) = run(
        &agent.1,
        exec(&["sh", "-c", "echo ${CARGO:-unset}"], directory.path()),
    )
    .await;
    assert_eq!(stdout, b"unset\n");

    // A signalled command reports 128 + signal; a missing program, 127.
    let (code, _, _) = run(
        &agent.1,
        exec(&["sh", "-c", "kill -9 $$"], directory.path()),
    )
    .await;
    assert_eq!(code, 137);
    let (code, _, stderr) = run(&agent.1, exec(&["no-such-program"], directory.path())).await;
    assert_eq!(code, 127);
    assert!(String::from_utf8_lossy(&stderr).contains("no-such-program"));

    // Large output arrives whole, in order.
    let (code, stdout, _) = run(
        &agent.1,
        exec(
            &["sh", "-c", "head -c 3000000 /dev/zero | tr '\\0' x"],
            directory.path(),
        ),
    )
    .await;
    assert_eq!(code, 0);
    assert_eq!(stdout.len(), 3_000_000);
    assert!(stdout.iter().all(|byte| *byte == b'x'));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_connection_ends_its_command() {
    let directory = tempfile::tempdir().unwrap();
    let agent = start(directory.path());
    let marker = directory.path().join("finished");
    let stream = tokio::net::UnixStream::connect(&agent.1).await.unwrap();
    let request = exec(
        &["sh", "-c", &format!("sleep 2; touch {}", marker.display())],
        directory.path(),
    );
    let running = tokio::spawn(async move {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let _ = exec_client(stream, &request, &mut stdout, &mut stderr).await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    running.abort();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!marker.exists(), "the command outlived its connection");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_leaves_a_process_behind_still_ends() {
    let directory = tempfile::tempdir().unwrap();
    let agent = start(directory.path());
    let started = std::time::Instant::now();
    let (code, _, _) = tokio::time::timeout(
        Duration::from_secs(10),
        run(
            &agent.1,
            exec(
                &[
                    "busybox",
                    "sh",
                    "-c",
                    "sleep 1000 > /dev/null 2>&1 & echo $! > sleeper",
                ],
                directory.path(),
            ),
        ),
    )
    .await
    .expect("the command's exit is reported while its child runs on");
    assert_eq!(code, 0);
    assert!(started.elapsed() < Duration::from_secs(5));
    let pid = std::fs::read_to_string(directory.path().join("sleeper")).unwrap();
    // SAFETY: probing, then ending, the process the command left behind.
    unsafe {
        assert_eq!(
            libc::kill(pid.trim().parse().unwrap(), 0),
            0,
            "it is still running"
        );
        libc::kill(pid.trim().parse().unwrap(), libc::SIGKILL);
    }
}
