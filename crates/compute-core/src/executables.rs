//! Running an executable Compute has just written.
//!
//! A file open for writing cannot be executed (`ETXTBSY`). Compute closes
//! what it writes before running it, but a process that forks in another
//! thread at that moment inherits the write descriptor until its own exec
//! closes it, and during that window running the file fails with "Text file
//! busy". The window is short; waiting it out is the only portable remedy.

use std::io;
use std::time::Duration;

/// Attempts before "Text file busy" is reported.
pub const BUSY_ATTEMPTS: u32 = 20;

/// How long to wait before attempt `attempt` (1-based): 10 ms, 20 ms, …,
/// about two seconds in all.
pub fn busy_backoff(attempt: u32) -> Duration {
    Duration::from_millis(10 * u64::from(attempt))
}

/// Whether an error is the transient "Text file busy".
pub fn is_busy(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::ExecutableFileBusy
}

/// `command.output()`, waiting out a transient "Text file busy".
pub fn output_when_not_busy(
    command: &mut std::process::Command,
) -> io::Result<std::process::Output> {
    let mut attempt = 0;
    loop {
        match command.output() {
            Err(error) if is_busy(&error) && attempt < BUSY_ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(busy_backoff(attempt));
            }
            result => return result,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// An executable still open for writing is busy; once it is closed, the
    /// same command runs.
    #[test]
    fn a_briefly_busy_executable_runs_once_it_is_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fresh");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"#!/bin/sh\necho ran\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Held open for writing: exec fails with "Text file busy".
        let busy = std::process::Command::new(&path).output().unwrap_err();
        assert!(is_busy(&busy), "{busy:?}");
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(file);
        });
        let output = output_when_not_busy(&mut std::process::Command::new(&path)).unwrap();
        closer.join().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "ran\n");
    }
}
