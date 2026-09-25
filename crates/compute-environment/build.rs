//! Records the source revision this Compute was built from, for
//! `compute node info`, `/info`, and upgrade receipts.

fn main() {
    let commit = std::env::var("COMPUTE_GIT_COMMIT").ok().or_else(|| {
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
    });
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| !output.stdout.is_empty());
    let commit = match commit {
        Some(commit) if dirty => format!("{commit}-dirty"),
        Some(commit) => commit,
        None => "unknown".into(),
    };
    println!("cargo:rustc-env=COMPUTE_GIT_COMMIT={commit}");
    println!(
        "cargo:rustc-env=COMPUTE_BUILD_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_default()
    );
    println!("cargo:rerun-if-env-changed=COMPUTE_GIT_COMMIT");
    for path in ["../../.git/HEAD", "../../.git/index", "../../.git/refs"] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
