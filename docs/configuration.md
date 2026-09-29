# Environment configuration

**Status:** implemented. `compute environment config NAME`,
`compute environment config import NAME FILE...`,
`compute environment config discover NAME`; `GET`/`POST` under
`/environments/{environment}/config`. Code: `crates/compute-environment/src/configuration.rs`
(the model and the `.env` parser), `daemon/configuration.rs` (the operations).

```text
Environment
├── declared state          what should exist           (contents)
├── configuration inputs    what its processes are given   ← this
└── workspace               portable filesystem state
        ↓
     Computer ─ reconcile ─▶ process environment
```

| | Is |
| --- | --- |
| **Workspace** | portable filesystem state |
| **Configuration** | runtime inputs: named variables |
| **Checkpoint** | immutable captured *workspace* state |
| **Environment** | declared + configured + executable state |
| **`.env`** | one *source* of configuration; parsed and imported, not a special file |

## The model: no new record

Configuration was already a record: `EnvironmentRecord.config`, the variables
every process, build, and command in the environment sees, hashed into each
process's fingerprint so that **changing it restarts what depends on it, in
place**. This work adds what was missing beside it (`EnvironmentRecord.configuration`,
model generation 11, additive):

| For each variable | |
| --- | --- |
| **sensitivity** | sensitive or public |
| **source** | `.env`, `.env.local`, `cli`, `api`, or `declared` |
| and once | the **generation** the configuration last changed at |

Values stay in `config`. `settle_configuration` runs inside the one function
every environment change goes through (`change_environment_with`), so the
metadata and the generation cannot drift from the values: the generation moves
exactly when a value, a variable, or a treatment does. It is the environment's
contents generation at that change, so configuration changes are ordered with
declaration changes. A process records the generation it started with
(`config_generation`, shown in its reality): evidence says which configuration
it ran under without holding any of it.

## Sensitive unless known public

Every variable is sensitive unless its **name** says otherwise or the caller
does (`--public NAME`, `--secret NAME`). The name rule is short, explicit, and
fails closed: a name containing `SECRET`, `TOKEN`, `KEY`, `PASSWORD`, `AUTH`,
`CERT`... is never public; a name is public only if it is one of `PORT`, `HOST`,
`NODE_ENV`, `APP_ENV`, `LOG_LEVEL`, `DEBUG`, `TZ`, `LANG`, `REGION`, ... or ends
`_MODE`, `_ENV`, `_PORT`, `_HOST`, `_LEVEL`, `_REGION`, `_TIMEOUT`, `_ENABLED`.
It is a default for *display*, not detection: Compute cannot tell what a value
is. A record that predates the metadata has every variable sensitive.

A sensitive value is returned by **no** surface: not an API response, the CLI,
an event, a receipt, an error, or a checkpoint manifest. Only that it is
configured, its source, and the generation. Public values are shown. (The
environment's views carry only public values; `configuration` carries every
variable's metadata.) Tests read every view, every event, the import report,
and the target's job records and receipts for a planted secret.

## Import

`compute environment config import NAME .env [.env.local ...]` (or
`POST /environments/{environment}/config/import`) **parses and validates
everything, plans every variable, then commits one change, or none**: a
malformed line in the last file applies nothing. It imports *values*; it does
not copy the file into the workspace, so there is no second copy of any secret.
Files apply in the order given; a later file overrides an earlier one and the
override is reported. Names Compute owns (`PORT`, `COMPUTE_*`) are skipped, with
the reason: a process is given its declared port as `PORT`. Importing what is
already configured changes nothing and moves no generation.

The syntax is small, fixed, and documented in `configuration.rs`: `NAME=VALUE`
with an optional `export`, `#` comments, unquoted / `'literal'` / `"escaped"`
values, CRLF and a BOM tolerated, no `${}` expansion, no multi-line values, a
name twice in one file is an error, and **an error names the file, the line
number, and the reason, never any part of the line**. It is deterministic by
construction (no map iteration order).

`config --set K=V --unset K --public K --secret K` (`POST .../config/change`)
changes variables individually. Values on a command line are in shell history
and process listings: prefer `import` for secrets.

## Discover

`compute environment config discover NAME` lists the `.env`-style files in the
workspace root and in each checked-out repository (`.env`, `.env.local`,
`.env.development`, `.env.production` as *values*; `.env.example` as
*requirements*) and the variable names each mentions. The extraction runs in the
computer and prints **names only**; values never leave it. Each variable is
`configured`, `available` (a workspace file supplies it: not imported), or
`missing`. `.env.example` answers "what does this application expect" without
supplying anything.

## What configuration is not

* **Not workspace state.** It is never written into the workspace by Compute, so
  a checkpoint of the workspace does not hold it. A checkpoint's manifest records
  the *names* and treatment (`configuration`), never values.
* **Not inherited.** Fork never copies values (`--copy-config` opts in, and the
  names left behind are reported). Restore never restores them and reports
  `configuration_required`. Configuration belongs to one environment: changing
  one leaves every other untouched (tested).
* **Not a secret manager.** Values live in the environment record, in whatever
  the control state provides. No external secret manager, encryption service,
  or second store was added.
* **Not detection of secrets in files.** A `.env` file a workload or a person
  writes into the workspace is workspace data, and a checkpoint captures it.
  Compute cannot recognise secrets in arbitrary files and does not claim to.
* **Legacy node environments** (the bundle model) keep their own configuration
  and views; this covers environments with a computer.

## A client that cannot see a value cannot erase it

The views omit sensitive values, so a client that reads the view and writes it
back (the GO editor, `POST /config`) sends only what it saw. A replacement of
"the configuration" therefore keeps every sensitive variable the caller did not
send, and removes only what the caller could see and omitted; removing a
sensitive variable is explicit (`--unset`, `POST .../config/change`).

## Authorization

The existing model: the environment's owner, `compute.operate` to change,
`compute.execute` to discover (it runs a job in the computer), `compute.read` to
read. Another operator's environment is refused. AuthBoundry is not involved.
