# Chip release gate: `@appport/chip` 0.54.5

Status: **release candidate built and dry-run; not published; the full stack is not certified.**
Supersedes nothing: `chip-outcomes-certification.md` still holds the per-scenario evidence.

## Source of truth

- `@appport/chip` is built from **`rkendel1/eve`**, `packages/eve` (package name `eve`, renamed at staging by
  `scripts/prepare-chip-package.mjs`). That is the repository to change; chip-rs and any mirror do not produce the artifact.
- Release path: changeset on a branch → `pnpm version-packages` → PR to `main` → the Release workflow publishes when the
  version is not yet on npm (needs `NPM_TOKEN`). Commits carry a DCO `Signed-off-by`.
- Branches: fixes `rkendel1/eve@claude/confident-mayer-r8pw7b` (`90b93b2b`); release prep
  `claude/release-prep-0.54.5` (`02d354ee`, version 0.54.5, changeset consumed). No PR was opened and nothing was merged.

## Candidate artifact (dry run only)

| | |
|---|---|
| Command | `node scripts/publish-packages.mjs --dry-run` (exit 0, `npm publish --dry-run` for `@appport/chip@0.54.5`) |
| File | `appport-chip-0.54.5.tgz`, 8,599,220 bytes, 3,595 files |
| sha256 | `d95cb3bcc60c47723c9ab10fc3d79d6ffcecc286adf9f7f829b99a5edafaa0ce` |
| integrity | `sha512-Nih9hXIMx2VPbOH7C8tmk/Lwp2xGG4YYFYTqktD2JQgptx8AeKhfgL1GobQksH5rJpAZGw3WSAQyMz0mqqtc3g==` |
| shasum | `c772d40bb2d63c2dc23c5f735d0cf29297846131` |

## Passed

| Check | Command | Result |
|---|---|---|
| 19 scenarios against the exact candidate tarball | `APPBOUNDRY_DIR=… CHIP_TARBALL=<tgz> node --test scenarios/outcomes.test.mjs` | exit 0, 19/19 |
| Stack verification | `npm run verify` (Node 24) | exit 0, 11/11 |
| Installer | `distribution/tests/installer.sh` | exit 0 |
| Homebrew | `distribution/tests/homebrew.sh` | exit 0 |
| Product contract | `cargo test -p compute-cli --test product --locked` | exit 0 |
| Attn | `bun run appport:test` / `bun run types` | 234 pass, 0 fail / exit 0 |
| eve | unit 802 files / 8,798; integration 106 files / 816; scenario 95 files / 518 (17 skipped); `docs:check` | pass |
| AppBoundry CLI | full suite at `d96e3a9` | 362 pass; 2 pre-existing FastAPI failures |

## Failed, not caused by this work

- `cargo test -p compute-cli --test recovery an_execution_that_ends_while_the_controller_is_down_is_recorded_after_recovery`:
  expects one execution record, finds two. It fails identically on an untouched `origin/main` worktree and no crate
  code changed on this branch, so it predates this work. Python 3.13 is present, so the test is not skipped here.
  Not investigated further.
- Pre-existing and unrelated: Attn `test-browser-world`, flaky `test-browser-compute`, eve guard (9 violations), workspace
  typecheck (needs network; the `eve` package's own `tsc` exits 0).

## Unrun

Attn live Tier 3; Level B Ollama smoke; FeltDB ignored tests (`FELTDB_SERVER_BIN`); distribution-certification.

## Blocked

1. **Publication and pins.** `@appport/chip@0.54.5` is not on npm. Compute's stack pins and Attn's `CHIP_VERSION` stay on
   the published version until the owner merges the release-prep branch with `NPM_TOKEN` configured. Pinning an unpublished
   version would break installs. After publishing: re-run the 19 scenarios without `CHIP_TARBALL`, then bump
   `package.json`, lockfile, `stack.json`, `stack.toml`, distribution references and Attn's `runtime-versions.env`.
2. **PAX.** No contract for AppBoundry invocations; Attn reports `pax: not_supported`. See `appboundry-persistence-and-pax.md`.
3. **AppBoundry persistence.** The host writes JSON, not FeltDB. Not relabelled.
4. **AuthBoundry enforcement** was not exercised.
5. **AppBoundry release.** The published `appport@1.1.20` lacks the MCP host, so the scenarios pin commit `d96e3a9`.
   The CI workflow needs an `APPBOUNDRY_READ_TOKEN` secret.

## Conclusion

The four Chip contract fixes, the tool removal and the AppBoundry scope-header fix are implemented and tested. The stack is
**not certified**: items 1–4 are open.
