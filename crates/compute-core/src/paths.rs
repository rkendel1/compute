//! Process-wide filesystem locations.
//!
//! `COMPUTE_DISTRIBUTION_ROOT` names an immutable installed distribution.
//! `COMPUTE_HOME` names mutable per-user state. They are deliberately
//! independent: replacing either tree must never replace the other.

use std::path::{Path, PathBuf};

use crate::{ComputeError, Result};

/// Locate the installed distribution, preferring an explicit override and
/// otherwise resolving it relative to the running executable.
pub fn installation_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("COMPUTE_DISTRIBUTION_ROOT") {
        return absolute(PathBuf::from(root));
    }
    let executable = std::env::current_exe().ok()?;
    let root = executable.parent()?.parent()?.to_path_buf();
    root.join("runtime-manifest.json").is_file().then_some(root)
}

fn absolute(path: PathBuf) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path)
    } else {
        Some(std::env::current_dir().ok()?.join(path))
    }
}

/// Locate mutable per-user Compute state.
pub fn state_root() -> Result<PathBuf> {
    let state = if let Some(home) = std::env::var_os("COMPUTE_HOME") {
        PathBuf::from(home)
    } else {
        let base = std::env::var_os("HOME")
            .ok_or_else(|| ComputeError::Runtime("set HOME or COMPUTE_HOME".into()))?;
        Path::new(&base).join(".compute")
    };
    if let Some(installation) = installation_root()
        && (state == installation
            || state.starts_with(&installation)
            || installation.starts_with(&state))
    {
        return Err(ComputeError::Runtime(format!(
            "Compute state ({}) and installation ({}) must be separate",
            state.display(),
            installation.display()
        )));
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_process_paths_are_made_absolute() {
        let path = absolute(PathBuf::from("compute-distribution")).unwrap();
        assert!(path.is_absolute());
        assert_eq!(
            path,
            std::env::current_dir()
                .unwrap()
                .join("compute-distribution")
        );
    }
}
