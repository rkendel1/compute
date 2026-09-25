# Getting started

Run an existing script directly:

```sh
compute run script.py
```

Compute deterministically resolves this into a versioned `WorkloadSpec`, then
uses the same planner, execution engine, runtime adapter, output collector, and
receipt implementation as an explicit workload. Direct execution is a CLI
convenience, not a second execution path.

JavaScript and TypeScript can target several runtimes, so select one unless a
`compute.toml` or recognized project manifest resolves it:

```sh
compute run --runtime node app.js
compute run --runtime bun app.ts
compute run --runtime deno app.mts
```

Workload arguments must follow `--`. This keeps Compute options unambiguous:

```sh
compute run script.py --env MODE=test -- --hello world
compute run --runtime node app.js -- --port 8080
```

The host environment is cleared. Add values explicitly with repeated `--env`
options or an environment file. File values load first and explicit options
win; neither values nor environment-file contents are copied into receipts.

```sh
compute run script.py --env-file .env.compute --env MODE=production
```

Inputs and outputs use the ordinary portable workload declarations:

```sh
compute run script.py --input config.json --output result.json
```

Networking is disabled by default. Isolation and resource options map directly
to the existing policy model and unsupported combinations fail rather than
downgrade:

```sh
compute run script.py --network disabled --timeout 30s --memory 512MB
compute run module.wasm --isolation strict
```

By default, a file's containing directory is its logical workload root. Use
`--cwd` to select a different root; the entrypoint and all declared files must
remain within it. The `process` profile uses a Compute-managed workspace but is
a process boundary, not a filesystem security sandbox. Sandboxed and strict
profiles expose only the materialized execution workspace.

## Inspect, make portable, verify

See the generated portable contract without executing anything:

```sh
compute run script.py --explain
compute inspect script.py --json
compute run script.py --dry-run --json
```

Create the same deterministic bundle produced from an equivalent explicit
specification:

```sh
compute bundle create script.py --output script.compute
```

Run it and save canonical execution evidence:

```sh
compute run script.py --receipt receipt.json
compute receipt verify receipt.json
```

## Optional project defaults

Directories can define a deliberately small `compute.toml`:

```toml
[run]
runtime = "node"
version = ">=24,<25"
entrypoint = "src/index.js"
isolation = "process"

[resources]
timeout = "30s"
memory = "512MB"

[network]
mode = "disabled"
```

Resolution precedence is CLI, `compute.toml`, recognized project defaults,
then Compute defaults. Ambiguity fails closed. Compute does not install
packages, create language environments, download dependencies, or fall back to
an arbitrary host runtime.
