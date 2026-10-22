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

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL: &str = "compute.guest@1";

/// The vsock port the agent listens on.
pub const GUEST_PORT: u32 = 52;

/// The vsock CID of the host, as a guest addresses it.
pub const HOST_CID: u32 = 2;

/// The largest frame either side accepts.
const MAX_FRAME: usize = 16 * 1024 * 1024;

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
pub async fn serve_connection<S: AsyncRead + AsyncWrite + Unpin>(stream: S) -> io::Result<()> {
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
        } => exec(argv, env, cwd, stdin, &mut reader, &mut writer).await,
    }
}

async fn exec<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    stdin: Vec<u8>,
    reader: &mut R,
    writer: &mut W,
) -> io::Result<()> {
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
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Its own process group, so ending it ends what it started.
        .process_group(0)
        .kill_on_drop(true);
    let mut child = match command.spawn() {
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
    let mut input = child.stdin.take().expect("piped stdin");
    tokio::spawn(async move {
        let _ = input.write_all(&stdin).await;
        let _ = input.shutdown().await;
    });
    let (sender, mut events) = tokio::sync::mpsc::channel::<Event>(16);
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
    let out = pump(Box::new(child.stdout.take().expect("piped stdout")), true);
    let err = pump(Box::new(child.stderr.take().expect("piped stderr")), false);
    drop(sender);
    let group = child.id().map(|pid| pid as i32);
    let end_group = || {
        if let Some(group) = group {
            // SAFETY: kill(2) of the command's own process group.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    };
    // Output is forwarded as it arrives; the exit follows the last of it.
    // The client sends nothing after its request, so anything it does now
    // (it closed, or it broke the protocol) ends the command.
    let mut probe = [0_u8; 1];
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    if let Err(error) = write_frame(writer, &event).await {
                        end_group();
                        return Err(error);
                    }
                }
                None => break,
            },
            _ = reader.read(&mut probe) => {
                end_group();
                return Ok(());
            }
        }
    }
    let _ = (out.await, err.await);
    let status = child.wait().await?;
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
            Event::Pong { .. } => {}
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
