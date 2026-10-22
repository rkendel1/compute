//! `compute-guest [--unix PATH] [--forward LISTEN:PORT]...`
//!
//! In a VM it is the init process (PID 1): it prepares the guest, then runs
//! the agent in a child it restarts if it dies, and reaps every orphan.
//! Elsewhere (`--unix`, for tests) it is just the agent.

use std::path::PathBuf;

fn main() {
    let mut unix = None::<PathBuf>;
    let mut forwards = Vec::<(u16, u32)>::new();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--unix" => unix = arguments.next().map(PathBuf::from),
            "--forward" => {
                let Some((listen, port)) = arguments
                    .next()
                    .and_then(|value| {
                        value
                            .split_once(':')
                            .map(|(a, b)| (a.to_owned(), b.to_owned()))
                    })
                    .and_then(|(listen, port)| Some((listen.parse().ok()?, port.parse().ok()?)))
                else {
                    eprintln!("compute-guest: --forward takes LISTEN:PORT");
                    std::process::exit(2);
                };
                forwards.push((listen, port));
            }
            other => {
                eprintln!("compute-guest: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    // Forwards may also come from the kernel command line, where an init
    // takes no arguments of its own: compute.forward=3128:3128.
    if let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") {
        for word in cmdline.split_whitespace() {
            if let Some((listen, port)) = word
                .strip_prefix("compute.forward=")
                .and_then(|value| value.split_once(':'))
                .and_then(|(listen, port)| Some((listen.parse().ok()?, port.parse().ok()?)))
            {
                forwards.push((listen, port));
            }
        }
    }
    if std::process::id() == 1 {
        compute_guest::init_guest();
        supervise(unix, forwards);
    }
    agent(unix, forwards);
}

/// PID 1: keep one agent running, and reap everything.
fn supervise(unix: Option<PathBuf>, forwards: Vec<(u16, u32)>) -> ! {
    loop {
        // SAFETY: fork(2) while single-threaded; the child only runs the agent.
        let agent_pid = unsafe { libc::fork() };
        if agent_pid == 0 {
            agent(unix, forwards);
        }
        loop {
            let mut status = 0;
            // SAFETY: wait(2) for any child.
            let reaped = unsafe { libc::wait(&mut status) };
            if reaped == agent_pid || (reaped < 0 && agent_pid < 0) {
                break;
            }
            if reaped < 0 {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn agent(unix: Option<PathBuf>, forwards: Vec<(u16, u32)>) -> ! {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async move {
        for (listen, port) in forwards {
            tokio::spawn(async move {
                if let Err(error) = compute_guest::forward_to_host(listen, port).await {
                    eprintln!("compute-guest: forward {listen}:{port}: {error}");
                }
            });
        }
        match unix {
            Some(path) => {
                let _ = std::fs::remove_file(&path);
                let listener = tokio::net::UnixListener::bind(&path).expect("bind the socket");
                loop {
                    if let Ok((stream, _)) = listener.accept().await {
                        tokio::spawn(async move {
                            let _ = compute_guest::serve_connection(stream).await;
                        });
                    }
                }
            }
            None => {
                compute_guest::serve_vsock(compute_guest::GUEST_PORT, |stream| async move {
                    let _ = compute_guest::serve_connection(stream).await;
                })
                .expect("listen on vsock");
                std::future::pending::<()>().await;
            }
        }
    });
    std::process::exit(1);
}
