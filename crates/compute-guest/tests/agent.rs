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
    // Listening, not merely bound: the socket's file exists from bind(2),
    // before connections are accepted.
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
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
        terminal: None,
        interactive: false,
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
            terminal: None,
            interactive: false,
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

#[tokio::test]
async fn the_agent_reseeds_the_kernel_generator_or_says_why_not() {
    let directory = tempfile::tempdir().unwrap();
    let agent = start(directory.path());
    let connect = || tokio::net::UnixStream::connect(&agent.1);

    let reseeded = compute_guest::reseed(connect().await.unwrap(), &[7_u8; 64]).await;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        reseeded.expect("as root, the kernel takes the entropy and reseeds");
    } else {
        let error = reseeded.expect_err("only a privileged agent may credit entropy");
        assert!(error.to_string().contains("RNDADDENTROPY"), "{error}");
    }
    for bounds in [vec![], vec![0_u8; compute_guest::MAX_RESEED_BYTES + 1]] {
        let error = compute_guest::reseed(connect().await.unwrap(), &bounds)
            .await
            .expect_err("out of bounds");
        assert!(error.to_string().contains("reseed takes"), "{error}");
    }
}

/// Run an interactive command, feeding it `controls` (each after `pause`),
/// and return its exit code and output.
async fn interactive(
    socket: &Path,
    request: Request,
    controls: Vec<Request>,
    pause: Duration,
) -> (i32, String, String) {
    let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        for control in controls {
            tokio::time::sleep(pause).await;
            if sender.send(control).await.is_err() {
                break;
            }
        }
        // Hold the channel open until the command ends.
        sender.closed().await;
    });
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let code = tokio::time::timeout(
        Duration::from_secs(30),
        compute_guest::exec_client_controlled(stream, &request, &mut stdout, &mut stderr, receiver),
    )
    .await
    .expect("the command ended")
    .unwrap();
    (
        code,
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

fn exec_request(argv: &[&str], terminal: Option<(u16, u16)>, interactive: bool) -> Request {
    Request::Exec {
        argv: argv.iter().map(|part| part.to_string()).collect(),
        env: BTreeMap::new(),
        cwd: "/".into(),
        stdin: vec![],
        terminal: terminal.map(|(rows, cols)| compute_guest::TerminalSize { rows, cols }),
        interactive,
    }
}

fn input(text: &str) -> Request {
    Request::Input {
        data: text.as_bytes().to_vec(),
    }
}

#[tokio::test]
async fn a_command_on_a_terminal_takes_input_resizes_and_exits() {
    let directory = tempfile::tempdir().unwrap();
    let agent = start(directory.path());
    let (code, output, _) = interactive(
        &agent.1,
        exec_request(&["sh"], Some((24, 80)), false),
        vec![
            input("tty; stty size\n"),
            Request::Resize {
                size: compute_guest::TerminalSize {
                    rows: 30,
                    cols: 100,
                },
            },
            input("stty size; echo $((6*7)); exit 3\n"),
        ],
        Duration::from_millis(300),
    )
    .await;
    assert_eq!(code, 3, "{output}");
    assert!(output.contains("/dev/pts/"), "a real terminal: {output}");
    assert!(
        output.contains("24 80"),
        "the size it started with: {output}"
    );
    assert!(output.contains("30 100"), "the size it was given: {output}");
    assert!(output.contains("42"), "{output}");
}

#[tokio::test]
async fn end_of_input_on_a_terminal_ends_a_reader() {
    let directory = tempfile::tempdir().unwrap();
    let agent = start(directory.path());
    let (code, output, _) = interactive(
        &agent.1,
        exec_request(&["sh", "-c", "cat; echo done"], Some((24, 80)), false),
        vec![input("typed\n"), Request::CloseInput],
        Duration::from_millis(300),
    )
    .await;
    assert_eq!(code, 0, "{output}");
    assert!(
        output.contains("typed") && output.contains("done"),
        "{output}"
    );
}

#[tokio::test]
async fn standard_input_streams_until_it_is_closed() {
    let directory = tempfile::tempdir().unwrap();
    let agent = start(directory.path());
    let (code, output, stderr) = interactive(
        &agent.1,
        exec_request(&["cat"], None, true),
        vec![input("first "), input("second"), Request::CloseInput],
        Duration::from_millis(200),
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(output, "first second");
}

#[tokio::test]
async fn a_signal_reaches_the_command() {
    let directory = tempfile::tempdir().unwrap();
    let agent = start(directory.path());
    for terminal in [None, Some((24, 80))] {
        let (code, output, _) = interactive(
            &agent.1,
            exec_request(&["sleep", "1000"], terminal, true),
            vec![Request::Signal { signal: 15 }],
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(code, 128 + 15, "{terminal:?}: {output}");
    }
}

#[tokio::test]
async fn a_command_that_is_not_interactive_takes_no_control() {
    let directory = tempfile::tempdir().unwrap();
    let agent = start(directory.path());
    // Anything a client sends after a plain command's request ends it, as
    // it always has: no exit is reported, the connection just ends.
    let stream = tokio::net::UnixStream::connect(&agent.1).await.unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    sender.send(input("ignored")).await.unwrap();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let ended = tokio::time::timeout(
        Duration::from_secs(10),
        compute_guest::exec_client_controlled(
            stream,
            &exec_request(&["sleep", "1000"], None, false),
            &mut stdout,
            &mut stderr,
            receiver,
        ),
    )
    .await
    .expect("the command was ended, not left to run");
    assert_eq!(ended.unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
    drop(sender);
}
