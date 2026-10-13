# Vendored @stack-ui static build

The shared Stack design system (`rkendel1/stack-ui`), vendored because this UI
has no build step and ships inside the `compute` binary.

- `tokens.css`, `core.css` and `stack-ui-core.js` come unmodified from
  `release/stack-ui-static-<VERSION>/` of `rkendel1/stack-ui`
  (`node scripts/package-static.mjs`).
- `crates/compute-environment/src/api.rs` prepends them to the existing
  `/ui/app.css` and `/ui/app.js`; there are no extra served paths.
- `tests/stack_ui.rs` checks every file against `SHA256SUMS`.

To upgrade: copy the new release directory over this one (keep this README) and
run `cargo test -p compute-environment --test stack_ui`.
