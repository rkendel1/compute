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

/// Chip and FX are one stack-agent commit, and it is the one the shipped product builds: the
/// harness never certifies a different Chip from the one Compute ships.
#[test]
fn the_harness_and_the_product_pin_the_same_stack_agent_commit() {
    let revs = |manifest: &str| -> std::collections::BTreeSet<String> {
        manifest
            .lines()
            .filter(|l| l.contains("rkendel1/stack-agent"))
            .filter_map(|l| Some(l.split("rev = \"").nth(1)?.split('"').next()?.to_string()))
            .collect()
    };
    let harness = revs(&manifest());
    let product = revs(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/compute-stack-agent/Cargo.toml"
        ))
        .unwrap(),
    );
    assert_eq!(harness.len(), 1, "harness pins: {harness:?}");
    assert_eq!(
        harness, product,
        "the harness and the product pin different commits"
    );
    for line in manifest().lines().filter(|l| l.contains("git = ")) {
        assert!(
            line.contains("rkendel1/stack-agent"),
            "Chip and FX come from stack-agent only: {line}"
        );
    }
}
