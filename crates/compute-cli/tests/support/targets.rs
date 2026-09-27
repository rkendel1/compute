#![allow(dead_code)]
//! A `compute serve` target that trusts one control plane, set up the way
//! an operator does it: `compute target credential issue` writes the
//! target's trust file and the control plane's token file.

use std::path::{Path, PathBuf};

pub struct TargetCredential {
    /// The target's trust file: `compute serve --credentials`.
    pub credentials: PathBuf,
    /// The control plane's token: a pool member's `token_file`.
    pub token_file: PathBuf,
}

impl TargetCredential {
    pub fn token(&self) -> String {
        std::fs::read_to_string(&self.token_file)
            .unwrap()
            .trim()
            .to_owned()
    }

    /// The pool-member line that presents this credential.
    pub fn pool_line(&self) -> String {
        format!("token_file = {:?}\n", self.token_file.display().to_string())
    }
}

/// Issue a credential for `control_plane` on the target whose files live
/// under `root/name`.
pub fn issue(root: &Path, name: &str, control_plane: &str) -> TargetCredential {
    let credentials = root.join(format!("{name}-credentials.json"));
    let token_file = root.join(format!("{name}-{control_plane}.token"));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_compute"))
        .args(["target", "credential", "issue", "--credentials"])
        .arg(&credentials)
        .args(["--control-plane", control_plane, "--token-file"])
        .arg(&token_file)
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    TargetCredential {
        credentials,
        token_file,
    }
}
