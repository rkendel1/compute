//! Attaching to an interactive session execution: this terminal's input
//! goes to the running command (`POST /compute/sessions/{id}/input`), and
//! its output is read from the job's durable log from an offset
//! (`POST /compute/jobs/{id}/output`). Nothing here starts a command, so
//! detaching (Ctrl-]) and attaching again reaches the same one.

use std::sync::Arc;

use compute_core::{SessionInput, TerminalSize};
use compute_provider::RemoteProvider;

/// Ctrl-]: leave the command running and return.
const DETACH: u8 = 0x1d;

/// How the attachment ended.
pub enum Attached {
    /// The command ended; its exit code.
    Ended(i32),
    /// Detached; the command runs on.
    Detached,
}

fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty has no preconditions.
    unsafe { libc::isatty(fd) == 1 }
}

/// This terminal's size, if standard output is one.
pub fn terminal_size() -> Option<TerminalSize> {
    let mut window: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ into a zeroed winsize.
    let read = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut window) };
    (read == 0 && window.ws_row > 0 && window.ws_col > 0).then_some(TerminalSize {
        rows: window.ws_row,
        cols: window.ws_col,
    })
}

/// Raw mode on standard input while attached; restored on drop.
struct Raw(Option<libc::termios>);

impl Raw {
    fn enter() -> Self {
        if !is_tty(libc::STDIN_FILENO) {
            return Self(None);
        }
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: tcgetattr/tcsetattr on standard input, a terminal.
        unsafe {
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return Self(None);
            }
            let mut raw = original;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        }
        Self(Some(original))
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(original) = &self.0 {
            // SAFETY: restoring the attributes read on entry.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, original);
            }
        }
    }
}

fn input(job_id: &str) -> SessionInput {
    SessionInput {
        job_id: compute_core::JobId(job_id.to_owned()),
        data: vec![],
        resize: None,
        signal: None,
        close: false,
    }
}

/// Attach this process's terminal (or pipes) to a running interactive
/// execution, from output `offset` (`u64::MAX`: from now).
pub async fn attach(
    remote: Arc<RemoteProvider>,
    session_id: &str,
    job_id: &str,
    offset: u64,
) -> compute_core::Result<Attached> {
    use tokio::io::AsyncWriteExt;
    let tty = is_tty(libc::STDIN_FILENO);
    let raw = Raw::enter();
    let (detach_tx, mut detached) = tokio::sync::mpsc::channel::<()>(1);

    // Input: this process's standard input, as it arrives.
    let (bytes_tx, mut bytes) = tokio::sync::mpsc::channel::<Option<Vec<u8>>>(64);
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match std::io::Read::read(&mut std::io::stdin(), &mut buffer) {
                Ok(0) | Err(_) => {
                    let _ = bytes_tx.blocking_send(None);
                    break;
                }
                Ok(read) => {
                    if bytes_tx
                        .blocking_send(Some(buffer[..read].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    let sending = {
        let remote = remote.clone();
        let (session_id, job_id) = (session_id.to_owned(), job_id.to_owned());
        tokio::spawn(async move {
            if tty && let Some(size) = terminal_size() {
                let mut resize = input(&job_id);
                resize.resize = Some(size);
                let _ = remote.session_input(&session_id, &resize).await;
            }
            let mut window = tty
                .then(|| {
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                        .ok()
                })
                .flatten();
            loop {
                let resized = async {
                    match &mut window {
                        Some(window) => window.recv().await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    chunk = bytes.recv() => match chunk {
                        Some(Some(mut data)) => {
                            if tty && let Some(at) = data.iter().position(|byte| *byte == DETACH) {
                                data.truncate(at);
                                if !data.is_empty() {
                                    let mut sent = input(&job_id);
                                    sent.data = data;
                                    let _ = remote.session_input(&session_id, &sent).await;
                                }
                                let _ = detach_tx.send(()).await;
                                return;
                            }
                            let mut sent = input(&job_id);
                            sent.data = data;
                            if remote.session_input(&session_id, &sent).await.is_err() {
                                return;
                            }
                        }
                        // The end of this input is the end of the command's.
                        Some(None) | None => {
                            let mut close = input(&job_id);
                            close.close = true;
                            let _ = remote.session_input(&session_id, &close).await;
                            return;
                        }
                    },
                    Some(()) = resized => {
                        if let Some(size) = terminal_size() {
                            let mut resize = input(&job_id);
                            resize.resize = Some(size);
                            let _ = remote.session_input(&session_id, &resize).await;
                        }
                    }
                }
            }
        })
    };

    // Output: the job's durable log, both streams, until it is complete.
    let reading = async {
        let mut stdout = tokio::io::stdout();
        let mut stderr = tokio::io::stderr();
        let (mut out_at, mut err_at) = (offset, offset);
        let (mut out_done, mut err_done) = (false, false);
        while !(out_done && err_done) {
            if !out_done {
                let chunk = remote
                    .job_output(job_id, "stdout", out_at, 1000)
                    .await
                    .map_err(crate::session_cmd::session_error)?;
                stdout.write_all(&chunk.data).await?;
                stdout.flush().await?;
                out_at = chunk.next_offset;
                out_done = chunk.complete;
            }
            if !err_done {
                let chunk = remote
                    .job_output(job_id, "stderr", err_at, if out_done { 1000 } else { 0 })
                    .await
                    .map_err(crate::session_cmd::session_error)?;
                stderr.write_all(&chunk.data).await?;
                stderr.flush().await?;
                err_at = chunk.next_offset;
                err_done = chunk.complete;
            }
        }
        Ok::<_, compute_core::ComputeError>(())
    };
    let ended = tokio::select! {
        read = reading => { read?; true }
        _ = detached.recv() => false,
    };
    sending.abort();
    drop(raw);
    if !ended {
        eprintln!(
            "\r\ndetached; the command runs on (compute session attach {session_id} {job_id})"
        );
        return Ok(Attached::Detached);
    }
    let result = crate::wait_for_remote_result(&remote, job_id).await?;
    Ok(Attached::Ended(result.result.exit_code.unwrap_or(1)))
}
