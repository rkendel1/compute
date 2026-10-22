//! The guest agent: the one process Compute runs inside a microVM, and the
//! only way into it.
//!
//! ```text
//!   host job ──vsock (port 52)──▶ compute-guest ──spawn──▶ the command
//!            ◀── stdout / stderr / exit, as frames ───┘
//! ```
//!
//! **Protocol (`compute.guest@1`).** Each connection carries one request and
//! its answer, as frames: a 4-byte big-endian length, then JSON. A request is
//! `ping` or `exec` (argv, environment, working directory, standard input);
//! an exec answers with `stdout` and `stderr` frames as the command writes,
//! then exactly one `exit`. A connection that drops ends its command.
//!
//! **In the VM** the agent is PID 1: it mounts the kernel filesystems, brings
//! up loopback, reaps orphans, and runs the server in a child it restarts if
//! it dies. It can also forward a TCP port inside the guest to a vsock port
//! on the host, which is how a VM with no network interface reaches the
//! egress gateway and nothing else.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL: &str = "compute.guest@1";

/// The vsock port the agent listens on.
pub const GUEST_PORT: u32 = 52;

/// The vsock CID of the host, as a guest addresses it.
pub const HOST_CID: u32 = 2;

/// The largest frame either side accepts.
const MAX_FRAME: usize = 16 * 1024 * 1024;

/// How long output is still collected after a command exits, from processes
/// it left holding its streams.
const EXIT_DRAIN: Duration = Duration::from_millis(250);

/// The `PATH` a command starts with unless it sets its own.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

mod bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(serde::de::Error::custom)
    }
}

/// A terminal's size, in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Ping,
    Exec {
        argv: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        cwd: String,
        #[serde(default, with = "bytes")]
        stdin: Vec<u8>,
        /// Run the command on a terminal of this size (a PTY): its output
        /// is the terminal's, and `Input`, `Resize`, `Signal`, and
        /// `CloseInput` may follow on the same connection.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal: Option<TerminalSize>,
        /// Keep standard input open after `stdin`: `Input` and
        /// `CloseInput` may follow on the same connection.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        interactive: bool,
    },
    /// More input for an interactive command.
    Input {
        #[serde(with = "bytes")]
        data: Vec<u8>,
    },
    /// The terminal changed size.
    Resize {
        size: TerminalSize,
    },
    /// Send a signal to the command's process group.
    Signal {
        signal: i32,
    },
    /// The end of the command's input (on a terminal: end-of-file, ^D).
    CloseInput,
    /// Mix the host's fresh entropy into the guest kernel's pool and force
    /// its random number generator to reseed. A forked machine starts with
    /// its siblings' generator state; this makes their output diverge.
    Reseed {
        #[serde(with = "bytes")]
        entropy: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Pong {
        protocol: String,
    },
    Stdout {
        #[serde(with = "bytes")]
        data: Vec<u8>,
    },
    Stderr {
        #[serde(with = "bytes")]
        data: Vec<u8>,
    },
    /// The command could not start, or the request was not understood.
    Error {
        message: String,
    },
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// The kernel took the entropy and reseeded.
    Reseeded,
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    writer.write_all(&(body.len() as u32).to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await
}

/// The next frame, or `None` when the stream ended cleanly between frames.
pub async fn read_frame<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> io::Result<Option<T>> {
    let mut length = [0_u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(io::Error::other)
}

/// Serve one connection: one request, its answer.
pub async fn serve_connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: S,
) -> io::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let Some(request) = read_frame::<_, Request>(&mut reader).await? else {
        return Ok(());
    };
    match request {
        Request::Ping => {
            write_frame(
                &mut writer,
                &Event::Pong {
                    protocol: PROTOCOL.into(),
                },
            )
            .await
        }
        Request::Exec {
            argv,
            env,
            cwd,
            stdin,
            terminal,
            interactive,
        } => {
            // Whatever the client sends next, read whole frames on their
            // own task (a frame read is not safe to abandon half way).
            let (sender, controls) = tokio::sync::mpsc::channel(64);
            let reading = tokio::spawn(async move {
                loop {
                    match read_frame::<_, Request>(&mut reader).await {
                        Ok(Some(request)) => {
                            if sender.send(Some(request)).await.is_err() {
                                break;
                            }
                        }
                        // The client closed, or broke the protocol.
                        _ => {
                            let _ = sender.send(None).await;
                            break;
                        }
                    }
                }
            });
            let ran = exec(
                Command {
                    argv,
                    env,
                    cwd,
                    stdin,
                    terminal,
                    interactive,
                },
                controls,
                &mut writer,
            )
            .await;
            reading.abort();
            ran
        }
        Request::Input { .. }
        | Request::Resize { .. }
        | Request::Signal { .. }
        | Request::CloseInput => {
            write_frame(
                &mut writer,
                &Event::Error {
                    message: "control for a command that is not running".into(),
                },
            )
            .await
        }
        Request::Reseed { entropy } => {
            let answer = match reseed_kernel(&entropy) {
                Ok(()) => Event::Reseeded,
                Err(message) => Event::Error { message },
            };
            write_frame(&mut writer, &answer).await
        }
    }
}

/// The most entropy one reseed carries.
pub const MAX_RESEED_BYTES: usize = 512;

/// `RNDADDENTROPY` (credit the entropy) then `RNDRESEEDCRNG` on
/// `/dev/urandom`. Needs CAP_SYS_ADMIN, which the agent has as PID 1.
fn reseed_kernel(entropy: &[u8]) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    if entropy.is_empty() || entropy.len() > MAX_RESEED_BYTES {
        return Err(format!("reseed takes 1 to {MAX_RESEED_BYTES} bytes"));
    }
    // _IOW('R', 0x03, int[2]) and _IO('R', 0x07).
    const RNDADDENTROPY: libc::c_ulong = 0x4008_5203;
    const RNDRESEEDCRNG: libc::c_ulong = 0x5207;
    let device = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/urandom")
        .map_err(|error| format!("/dev/urandom: {error}"))?;
    // struct rand_pool_info { int entropy_count; int buf_size; u32 buf[]; }
    let mut info = Vec::with_capacity(8 + entropy.len());
    info.extend_from_slice(&((entropy.len() * 8) as i32).to_ne_bytes());
    info.extend_from_slice(&(entropy.len() as i32).to_ne_bytes());
    info.extend_from_slice(entropy);
    // SAFETY: both ioctls on an open /dev/urandom; the buffer is the
    // rand_pool_info the first expects, and outlives the call.
    unsafe {
        if libc::ioctl(device.as_raw_fd(), RNDADDENTROPY as _, info.as_ptr()) != 0 {
            return Err(format!(
                "RNDADDENTROPY: {}",
                std::io::Error::last_os_error()
            ));
        }
        if libc::ioctl(device.as_raw_fd(), RNDRESEEDCRNG as _) != 0 {
            return Err(format!(
                "RNDRESEEDCRNG: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

struct Command {
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    stdin: Vec<u8>,
    terminal: Option<TerminalSize>,
    interactive: bool,
}

/// A pseudo-terminal of `size`: (master, slave).
fn open_pty(size: TerminalSize) -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (0, 0);
    let mut window = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty(3) with valid out-pointers and a winsize; no name or
    // termios.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut window,
        )
    };
    if opened != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just opened and are owned here.
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

fn resize_pty(master: &OwnedFd, size: TerminalSize) {
    use std::os::fd::AsRawFd;
    let window = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ on the master of a PTY this process opened; the
    // kernel signals the terminal's foreground group (SIGWINCH).
    unsafe {
        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &window);
    }
}

/// Where an interactive command's further input goes.
enum Input {
    None,
    Pipe(Option<tokio::process::ChildStdin>),
    Terminal(std::fs::File),
}

async fn exec<W: AsyncWrite + Unpin>(
    request: Command,
    mut controls: tokio::sync::mpsc::Receiver<Option<Request>>,
    writer: &mut W,
) -> io::Result<()> {
    let Command {
        argv,
        env,
        cwd,
        stdin,
        terminal,
        interactive,
    } = request;
    let Some((program, arguments)) = argv.split_first() else {
        write_frame(
            writer,
            &Event::Error {
                message: "a command needs a program".into(),
            },
        )
        .await?;
        return write_frame(
            writer,
            &Event::Exit {
                code: Some(127),
                signal: None,
            },
        )
        .await;
    };
    let mut command = tokio::process::Command::new(program);
    command
        .args(arguments)
        .env_clear()
        .env("PATH", DEFAULT_PATH)
        .env("HOME", &cwd)
        .envs(&env)
        .current_dir(&cwd)
        .kill_on_drop(true);
    let mut master = None;
    match terminal {
        Some(size) => {
            let (pty, slave) = open_pty(size)?;
            command
                .env(
                    "TERM",
                    env.get("TERM").map_or("xterm-256color", String::as_str),
                )
                .stdin(std::process::Stdio::from(slave.try_clone()?))
                .stdout(std::process::Stdio::from(slave.try_clone()?))
                .stderr(std::process::Stdio::from(slave));
            // SAFETY: only async-signal-safe calls between fork and exec:
            // a new session (so ending it ends what it started), with the
            // terminal as its controlling terminal.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            master = Some(pty);
        }
        None => {
            command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                // Its own process group, so ending it ends what it started.
                .process_group(0);
        }
    }
    let spawned = command.spawn();
    // The parent's copies of the terminal's slave end go with the command,
    // so the terminal reports its end when the command's processes close it.
    drop(command);
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            write_frame(
                writer,
                &Event::Error {
                    message: format!("{program}: {error}"),
                },
            )
            .await?;
            return write_frame(
                writer,
                &Event::Exit {
                    code: Some(127),
                    signal: None,
                },
            )
            .await;
        }
    };
    let (sender, mut events) = tokio::sync::mpsc::channel::<Event>(16);
    let mut pumps = vec![];
    let mut input = Input::None;
    match &master {
        Some(master) => {
            // A terminal is one stream; read it on a thread (its reads
            // block), until the last process closes its side.
            let mut reading = std::fs::File::from(master.try_clone()?);
            let sender = sender.clone();
            std::thread::spawn(move || {
                let mut buffer = vec![0_u8; 64 * 1024];
                loop {
                    match std::io::Read::read(&mut reading, &mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let data = buffer[..read].to_vec();
                            if sender.blocking_send(Event::Stdout { data }).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            let mut terminal = std::fs::File::from(master.try_clone()?);
            if !stdin.is_empty() {
                let _ = std::io::Write::write_all(&mut terminal, &stdin);
            }
            input = Input::Terminal(terminal);
        }
        None => {
            let pump = |mut source: Box<dyn AsyncRead + Unpin + Send>, stdout: bool| {
                let sender = sender.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0_u8; 64 * 1024];
                    loop {
                        match source.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => {
                                let data = buffer[..read].to_vec();
                                let event = if stdout {
                                    Event::Stdout { data }
                                } else {
                                    Event::Stderr { data }
                                };
                                if sender.send(event).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })
            };
            pumps.push(pump(
                Box::new(child.stdout.take().expect("piped stdout")),
                true,
            ));
            pumps.push(pump(
                Box::new(child.stderr.take().expect("piped stderr")),
                false,
            ));
            let mut pipe = child.stdin.take().expect("piped stdin");
            if interactive {
                pipe.write_all(&stdin).await.ok();
                input = Input::Pipe(Some(pipe));
            } else {
                tokio::spawn(async move {
                    let _ = pipe.write_all(&stdin).await;
                    let _ = pipe.shutdown().await;
                });
            }
        }
    }
    drop(sender);
    let controllable = terminal.is_some() || interactive;
    let group = child.id().map(|pid| pid as i32);
    let signal_group = |signal: i32| {
        if let Some(group) = group {
            // SAFETY: kill(2) of the command's own process group.
            unsafe {
                libc::kill(-group, signal);
            }
        }
    };
    // Output is forwarded until the command exits and its streams end. A
    // process it left running may hold its streams open (a shell's forked
    // in-process applet keeps a copy of them), so once the command has
    // exited, what is left is drained for a moment and the exit reported:
    // the command ended, whatever it left behind.
    let mut status = None;
    let mut drained = std::pin::pin!(tokio::time::sleep(Duration::from_secs(86400 * 365)));
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    if let Err(error) = write_frame(writer, &event).await {
                        signal_group(libc::SIGKILL);
                        return Err(error);
                    }
                }
                None => break,
            },
            exited = child.wait(), if status.is_none() => {
                status = Some(exited?);
                drained
                    .as_mut()
                    .reset(tokio::time::Instant::now() + EXIT_DRAIN);
            }
            () = &mut drained => break,
            control = controls.recv() => match control {
                Some(Some(Request::Input { data })) if controllable => match &mut input {
                    Input::Terminal(terminal) => {
                        let _ = std::io::Write::write_all(terminal, &data);
                    }
                    Input::Pipe(Some(pipe)) => {
                        let _ = pipe.write_all(&data).await;
                    }
                    Input::Pipe(None) | Input::None => {}
                },
                Some(Some(Request::CloseInput)) if controllable => match &mut input {
                    // End-of-file on a terminal is its EOF character.
                    Input::Terminal(terminal) => {
                        let _ = std::io::Write::write_all(terminal, &[0x04]);
                    }
                    Input::Pipe(pipe) => {
                        if let Some(mut pipe) = pipe.take() {
                            let _ = pipe.shutdown().await;
                        }
                    }
                    Input::None => {}
                },
                Some(Some(Request::Resize { size })) if terminal.is_some() => {
                    if let Some(master) = &master {
                        resize_pty(master, size);
                    }
                }
                Some(Some(Request::Signal { signal })) if controllable && (1..=64).contains(&signal) => {
                    signal_group(signal);
                }
                // The client is gone, or sent what this command does not
                // take: either ends the command.
                _ => {
                    signal_group(libc::SIGKILL);
                    return Ok(());
                }
            },
        }
    }
    for pump in pumps {
        pump.abort();
    }
    let status = match status {
        Some(status) => status,
        None => child.wait().await?,
    };
    use std::os::unix::process::ExitStatusExt;
    write_frame(
        writer,
        &Event::Exit {
            code: status.code(),
            signal: status.signal(),
        },
    )
    .await
}

/// Run a command through a connected agent, writing its output as it
/// arrives. Returns the exit status a shell would report (128 + signal for
/// a signalled command).
pub async fn exec_client<S, O, E>(
    stream: S,
    request: &Request,
    stdout: &mut O,
    stderr: &mut E,
) -> io::Result<i32>
where
    S: AsyncRead + AsyncWrite + Unpin,
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(&mut writer, request).await?;
    while let Some(event) = read_frame::<_, Event>(&mut reader).await? {
        match event {
            Event::Stdout { data } => {
                stdout.write_all(&data).await?;
                stdout.flush().await?;
            }
            Event::Stderr { data } => {
                stderr.write_all(&data).await?;
                stderr.flush().await?;
            }
            Event::Error { message } => {
                stderr
                    .write_all(format!("compute-guest: {message}\n").as_bytes())
                    .await?;
            }
            Event::Exit { code, signal } => {
                return Ok(code.unwrap_or_else(|| 128 + signal.unwrap_or(0)));
            }
            Event::Pong { .. } | Event::Reseeded => {}
        }
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "the guest ended the connection before the command exited",
    ))
}

/// Whether an agent answers on `stream`, and with which protocol.
pub async fn ping<S: AsyncRead + AsyncWrite + Unpin>(stream: S) -> io::Result<String> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(&mut writer, &Request::Ping).await?;
    match read_frame::<_, Event>(&mut reader).await? {
        Some(Event::Pong { protocol }) => Ok(protocol),
        _ => Err(io::Error::other("the guest did not answer a ping")),
    }
}

/// [`exec_client`] for an interactive command: frames from `controls`
/// (`Input`, `Resize`, `Signal`, `CloseInput`) are forwarded to the guest
/// while the command runs.
pub async fn exec_client_controlled<S, O, E>(
    stream: S,
    request: &Request,
    stdout: &mut O,
    stderr: &mut E,
    mut controls: tokio::sync::mpsc::Receiver<Request>,
) -> io::Result<i32>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(&mut writer, request).await?;
    // Writes on their own task: the event reads below are never abandoned.
    let forwarding = tokio::spawn(async move {
        while let Some(control) = controls.recv().await {
            if write_frame(&mut writer, &control).await.is_err() {
                break;
            }
        }
        // Held until the command ends: closing it would end the command.
        writer
    });
    let ended = async {
        while let Some(event) = read_frame::<_, Event>(&mut reader).await? {
            match event {
                Event::Stdout { data } => {
                    stdout.write_all(&data).await?;
                    stdout.flush().await?;
                }
                Event::Stderr { data } => {
                    stderr.write_all(&data).await?;
                    stderr.flush().await?;
                }
                Event::Error { message } => {
                    stderr
                        .write_all(format!("compute-guest: {message}\n").as_bytes())
                        .await?;
                }
                Event::Exit { code, signal } => {
                    return Ok(code.unwrap_or_else(|| 128 + signal.unwrap_or(0)));
                }
                Event::Pong { .. } | Event::Reseeded => {}
            }
        }
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the guest ended the connection before the command exited",
        ))
    }
    .await;
    forwarding.abort();
    ended
}

/// Give the guest fresh entropy and have it reseed; an error says why it
/// could not (an older agent does not understand the request).
pub async fn reseed<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    entropy: &[u8],
) -> io::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(
        &mut writer,
        &Request::Reseed {
            entropy: entropy.to_vec(),
        },
    )
    .await?;
    match read_frame::<_, Event>(&mut reader).await {
        Ok(Some(Event::Reseeded)) => Ok(()),
        Ok(Some(Event::Error { message })) => Err(io::Error::other(message)),
        _ => Err(io::Error::other("the guest did not reseed")),
    }
}

/// Connect to a guest port through Firecracker's vsock bridge: a Unix
/// socket on the host, then `CONNECT <port>` and its `OK` line.
pub async fn connect_firecracker(
    uds: &std::path::Path,
    port: u32,
) -> io::Result<tokio::net::UnixStream> {
    let mut stream = tokio::net::UnixStream::connect(uds).await?;
    stream
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .await?;
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    while !line.ends_with(b"\n") {
        if stream.read(&mut byte).await? == 0 || line.len() > 64 {
            return Err(io::Error::other("the vsock bridge refused the connection"));
        }
        line.push(byte[0]);
    }
    if !line.starts_with(b"OK ") {
        return Err(io::Error::other(format!(
            "the vsock bridge answered {:?}",
            String::from_utf8_lossy(&line).trim()
        )));
    }
    Ok(stream)
}

fn vsock_address(cid: u32, port: u32) -> libc::sockaddr_vm {
    // SAFETY: an all-zero sockaddr_vm is a valid starting value.
    let mut address: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    address.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    address.svm_cid = cid;
    address.svm_port = port;
    address
}

fn vsock_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket(2); the result is checked.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a new descriptor this function owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A vsock stream as a tokio stream: reads and writes are the same
/// system calls as for any stream socket.
fn stream_from(fd: OwnedFd) -> io::Result<tokio::net::UnixStream> {
    let stream = std::os::unix::net::UnixStream::from(fd);
    stream.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(stream)
}

/// Listen on a vsock port; each accepted connection is handed to `handle`
/// on the current runtime.
pub fn serve_vsock<F, Fut>(port: u32, handle: F) -> io::Result<()>
where
    F: Fn(tokio::net::UnixStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    use std::os::fd::AsRawFd;
    let socket = vsock_socket()?;
    let address = vsock_address(libc::VMADDR_CID_ANY, port);
    // SAFETY: address is a valid sockaddr_vm of the size given.
    let bound = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            &address as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    // SAFETY: listen(2) on a socket this function owns.
    if bound < 0 || unsafe { libc::listen(socket.as_raw_fd(), 64) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        loop {
            // SAFETY: accept4(2) without an address; the result is checked.
            let fd = unsafe {
                libc::accept4(
                    socket.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if fd < 0 {
                continue;
            }
            // SAFETY: fd was just accepted and is owned here.
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            let _guard = runtime.enter();
            if let Ok(stream) = stream_from(owned) {
                runtime.spawn(handle(stream));
            }
        }
    });
    Ok(())
}

/// Connect from the guest to a vsock port (on the host, `HOST_CID`).
pub fn connect_vsock(cid: u32, port: u32) -> io::Result<tokio::net::UnixStream> {
    use std::os::fd::AsRawFd;
    let socket = vsock_socket()?;
    let address = vsock_address(cid, port);
    // SAFETY: address is a valid sockaddr_vm of the size given.
    let connected = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            &address as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    if connected < 0 {
        return Err(io::Error::last_os_error());
    }
    stream_from(socket)
}

/// Inside the guest: accept TCP on `127.0.0.1:listen` and carry each
/// connection to the host's vsock `port`.
pub async fn forward_to_host(listen: u16, port: u32) -> io::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", listen)).await?;
    loop {
        let (mut inbound, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Ok(mut outbound) = connect_vsock(HOST_CID, port) {
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            }
        });
    }
}

/// PID 1 duties in a VM with nothing else in it: kernel filesystems, a
/// loopback interface, and a workspace.
pub fn init_guest() {
    let mount = |source: &str, target: &str, kind: &str| {
        let _ = std::fs::create_dir_all(target);
        let c = |text: &str| std::ffi::CString::new(text).expect("no NUL");
        let (source, target, kind) = (c(source), c(target), c(kind));
        // SAFETY: mount(2) with valid C strings; failure is tolerated (the
        // filesystem may already be mounted).
        unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                kind.as_ptr(),
                0,
                std::ptr::null(),
            );
        }
    };
    mount("proc", "/proc", "proc");
    mount("sysfs", "/sys", "sysfs");
    mount("devtmpfs", "/dev", "devtmpfs");
    mount("tmpfs", "/tmp", "tmpfs");
    mount("tmpfs", "/run", "tmpfs");
    let _ = std::fs::create_dir_all("/workspace");
    loopback_up();
}

fn loopback_up() {
    // SAFETY: an ioctl on a datagram socket with a zeroed, named ifreq.
    unsafe {
        let socket = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if socket < 0 {
            return;
        }
        let mut request: libc::ifreq = std::mem::zeroed();
        for (at, byte) in b"lo".iter().enumerate() {
            request.ifr_name[at] = *byte as libc::c_char;
        }
        if libc::ioctl(socket, libc::SIOCGIFFLAGS as _, &mut request) == 0 {
            request.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            libc::ioctl(socket, libc::SIOCSIFFLAGS as _, &request);
        }
        libc::close(socket);
    }
}
