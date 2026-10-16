//! Preparation for the Chip + FX integration (docs/chip-fx-integration.md): the dependency on Rust
//! Chip is an immutable revision, and Compute's Rust Chip adapter reaches effects only through the
//! supported capability boundary. Source checks, no network, no Compute target.

const MANIFEST: &str = include_str!("../Cargo.toml");
const LIB: &str = include_str!("../src/lib.rs");
const MAIN: &str = include_str!("../src/main.rs");

fn code(source: &str) -> String {
    source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A branch name moves, and this one no longer exists upstream. A full commit does not move.
#[test]
fn rust_chip_is_pinned_to_a_full_commit_never_a_branch_or_tag() {
    let mut pinned = 0;
    for line in MANIFEST.lines().filter(|l| l.contains("rkendel1/chip-rs")) {
        pinned += 1;
        assert!(
            !line.contains("branch") && !line.contains("tag"),
            "mutable pin: {line}"
        );
        let rev = line
            .split("rev = \"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .unwrap_or_else(|| panic!("no rev: {line}"));
        assert!(
            rev.len() == 40 && rev.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "not a full commit: {line}"
        );
    }
    assert!(pinned >= 5);
}

#[test]
fn every_rust_chip_crate_is_pinned_to_the_same_commit() {
    let revs: std::collections::BTreeSet<&str> = MANIFEST
        .lines()
        .filter(|l| l.contains("rkendel1/chip-rs"))
        .filter_map(|l| l.split("rev = \"").nth(1)?.split('"').next())
        .collect();
    assert_eq!(revs.len(), 1, "Chip crates must move together: {revs:?}");
}

/// Compute reaches effects through the environment contract (`exec` in a Compute session) and
/// nothing else. The adapter library opens no process, file or socket of its own; the launcher may
/// read its token file and nothing more.
#[test]
fn the_adapter_reaches_effects_only_through_the_supported_boundary() {
    let lib = code(LIB);
    for forbidden in [
        "std::process",
        "tokio::process",
        "std::fs",
        "tokio::fs",
        "std::net",
        "tokio::net",
        "Command::new",
    ] {
        assert!(
            !lib.contains(forbidden),
            "lib.rs bypasses the capability boundary: {forbidden}"
        );
    }
    let main = code(MAIN);
    for forbidden in [
        "std::process::Command",
        "tokio::process",
        "Command::new",
        "std::net",
        "tokio::net",
    ] {
        assert!(
            !main.contains(forbidden),
            "main.rs bypasses the capability boundary: {forbidden}"
        );
    }
    // The launcher's only filesystem access is reading the operator's token file.
    assert_eq!(
        main.matches("std::fs::").count(),
        1,
        "unexpected filesystem access in main.rs"
    );
    assert!(main.contains("read_to_string(&file)"));
}
