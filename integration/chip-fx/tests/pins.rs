//! Dependencies are immutable revisions, and FX is one version.

fn manifest() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap()
}

#[test]
fn every_git_dependency_is_pinned_to_a_full_commit_never_a_branch_or_tag() {
    let mut git_dependencies = 0;
    for line in manifest().lines().filter(|l| l.contains("git = ")) {
        git_dependencies += 1;
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
            rev.len() == 40
                && rev
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "not a full commit: {line}"
        );
    }
    assert!(git_dependencies >= 5);
}

/// chip-rs builds against one FX revision. If a chip-rs bump moves it, a second FX version would
/// enter the graph silently; this fails instead, and the pins are moved together (docs/chip-fx-integration.md).
#[test]
fn the_resolved_graph_has_exactly_one_fx_revision() {
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock")).unwrap();
    let mut revs = std::collections::BTreeSet::new();
    for line in lock
        .lines()
        .filter(|l| l.contains("source = \"git+https://github.com/rkendel1/fx"))
    {
        revs.insert(
            line.split('#')
                .nth(1)
                .unwrap_or("")
                .trim_end_matches('"')
                .to_string(),
        );
    }
    assert_eq!(
        revs.len(),
        1,
        "more than one FX revision resolved: {revs:?}"
    );
    assert!(
        manifest().contains(revs.iter().next().unwrap()),
        "Cargo.toml's fx rev is not the resolved one"
    );
}
