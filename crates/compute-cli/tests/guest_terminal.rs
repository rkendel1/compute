//! `compute guest-exec` with an interactive command, end to end without a
//! VM: the real guest agent, served in this process behind a stand-in for
//! Firecracker's vsock bridge (the `CONNECT <port>` / `OK` handshake), and
//! the real attach socket a job's clients send control frames to.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use compute_guest::{Request, TerminalSize};

/// Accept connections on `uds` as Firecracker's vsock bridge does, and
/// serve the guest agent on each.
fn bridge(uds: &Path) {
    let listener = tokio::net::UnixListener::bind(uds).unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut line = Vec::new();
                let mut byte = [0_u8; 1];
                while !line.ends_with(b"\n") {
                    if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    line.push(byte[0]);
                }
                assert!(line.starts_with(b"CONNECT "));
                stream.write_all(b"OK 1073741824\n").await.unwrap();
                let _ = compute_guest::serve_connection(stream).await;
            });
        }
    });
}

async fn attach(path: &Path) -> tokio::net::UnixStream {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(stream) = tokio::net::UnixStream::connect(path).await {
            return stream;
        }
        assert!(Instant::now() < deadline, "the command never listened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct Run {
    child: tokio::process::Child,
    attach: PathBuf,
    _directory: tempfile::TempDir,
}

fn start(arguments: &[&str], command: &[&str]) -> Run {
    let directory = tempfile::tempdir().unwrap();
    let uds = directory.path().join("vsock.sock");
    bridge(&uds);
    let attach = directory.path().join("attach").join("job.sock");
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_compute"))
        .arg("guest-exec")
        .arg("--uds")
        .arg(&uds)
        .args(["--cwd", "/"])
        .arg("--attach")
        .arg(&attach)
        .args(arguments)
        .arg("--")
        .args(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    Run {
        child,
        attach,
        _directory: directory,
    }
}

async fn finish(run: Run) -> (i32, String) {
    let output = tokio::time::timeout(Duration::from_secs(30), run.child.wait_with_output())
        .await
        .expect("the command ended")
        .unwrap();
    assert!(
        !run.attach.exists(),
        "the attach socket goes with the command"
    );
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

async fn send(stream: &mut tokio::net::UnixStream, request: Request) {
    compute_guest::write_frame(stream, &request).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn input(text: &str) -> Request {
    Request::Input {
        data: text.as_bytes().to_vec(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_command_is_driven_through_its_attach_socket() {
    let run = start(&["--tty", "24x80"], &["sh"]);
    let mut control = attach(&run.attach).await;
    send(&mut control, input("tty; stty size\n")).await;
    send(
        &mut control,
        Request::Resize {
            size: TerminalSize {
                rows: 30,
                cols: 100,
            },
        },
    )
    .await;
    // A second attachment reaches the same command: attaching again
    // never starts it again.
    drop(control);
    let mut again = attach(&run.attach).await;
    send(&mut again, input("stty size; echo $((6*7)); exit 5\n")).await;
    let (code, output) = finish(run).await;
    assert_eq!(code, 5, "{output}");
    assert!(output.contains("/dev/pts/"), "{output}");
    assert!(output.contains("24 80"), "{output}");
    assert!(output.contains("30 100"), "{output}");
    assert!(output.contains("42"), "{output}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interactive_command_streams_input_until_it_is_closed() {
    let run = start(&["--interactive"], &["cat"]);
    let mut control = attach(&run.attach).await;
    send(&mut control, input("one ")).await;
    send(&mut control, input("two")).await;
    send(&mut control, Request::CloseInput).await;
    let (code, output) = finish(run).await;
    assert_eq!(code, 0);
    assert_eq!(output, "one two");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attach_socket_relays_only_control() {
    let run = start(&["--interactive"], &["sh", "-c", "cat; echo end"]);
    let mut stray = attach(&run.attach).await;
    // Not a control frame: that connection is ended, the command is not.
    compute_guest::write_frame(&mut stray, &Request::Ping)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut control = attach(&run.attach).await;
    send(&mut control, input("kept\n")).await;
    send(&mut control, Request::Signal { signal: 15 }).await;
    let (code, output) = finish(run).await;
    assert_eq!(code, 128 + 15, "{output}");
    assert!(output.contains("kept"), "{output}");
}
