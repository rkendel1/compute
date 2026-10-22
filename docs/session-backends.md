# Session backends: what each one supports

Compute's lifecycle contract is backend-neutral. Each session provider
**declares** what it supports (`SessionCapabilities`, [sessions.md](sessions.md));
an operation it does not declare is refused with `operation_unsupported`
before anything is sent to it, and a requirement it cannot meet
(`--require NAME`) is refused at admission. A provider never substitutes
different semantics for an operation it declares.

This page states what is implemented and **how it is verified**. "CI" names
the job that exercises it; "local" means tests that run without that
substrate (fakes or skips). A skipped test is not verification.

## Capability matrix

| Capability | `workspace` (process) | `container` (docker/podman, optional gVisor) | `microvm` (Firecracker) | Cloudflare Workers |
| --- | --- | --- | --- | --- |
| Hosts sessions | yes | yes | yes | **no** (`sessions_unsupported`) |
| `exec` | yes | yes | yes | n/a |
| bounded stdin (`--stdin`) | yes | yes | yes | stdin per invocation |
| streaming stdin (`exec --interactive`) | **no** | **no** | yes; KVM verification pending | no |
| output while running, from an offset (`attach`, `POST /compute/jobs/{id}/output`) | yes | yes | yes | no |
| `terminal`: PTY, resize (`exec --tty`, `attach`) | **no** | **no** | yes; KVM verification pending | no |
| signals to a running command | cancel only | cancel only | any signal to its process group (`session input --signal`); cancel | no |
| workspace files (`session file`) | yes | yes | yes | n/a |
| `network` | host network or `none`* | engine network or `none` | **egress gateway only** (no NIC) | none |
| `suspend` / `resume` (storage kept) | yes | yes (container stop/start) | yes | n/a |
| `memory_suspend` (processes and memory kept) | **no** | **no** | yes, verified on KVM | n/a |
| `fork` (machine fork) | **no** | **no** | yes, verified on KVM | n/a |
| `process_tree_termination` | where an ownership scan is available | yes | yes (the VM ends) | n/a |
| receipt `substrate` evidence | no | engine, OCI runtime, image, limits | VMM version, kernel and rootfs digests, vCPUs, memory, network | n/a |

\* The process provider enforces `none` only where the host can.

**Environment fork** (`compute environment fork`, [fork.md](fork.md)) is not
machine fork: it moves *portable workspace state* into a new environment on
any backend; no memory, process, or machine identity is inherited.

## Status by backend

| Backend | Platform | Prerequisites | Verified by |
| --- | --- | --- | --- |
| `workspace` | Linux, macOS | none | local test suite |
| `container` | Linux (engine), macOS with Docker Desktop/Podman machine (untested) | a Docker or Podman engine; `runsc` for gVisor | CI `container-engine` (real Docker on Linux); gVisor is not exercised in CI |
| `microvm` | **Linux with KVM only** | `/dev/kvm`, Firecracker (CI: v1.11.0), an uncompressed guest kernel with virtio-vsock, a rootfs from `distribution/microvm/build-rootfs.sh` | CI `microvm` (GitHub-hosted runner with KVM) |
| Cloudflare Workers | any (remote) | a Cloudflare account and API token | fake API locally; live smoke test only with credentials ([cloudflare.md](cloudflare.md)) |

**macOS has no VM backend.** `--session-provider microvm` requires KVM;
nothing on macOS (Virtualization.framework, libkrun) is implemented. A
future backend would declare its own capabilities behind the same contract.

**Cloudflare Workers** runs short, synchronous WASI command modules only:
no sessions, jobs, files, processes, or outbound network, and Cloudflare's
own CPU and memory limits apply ([cloudflare.md](cloudflare.md)).

## microVM suspend and resume

`memory_suspend` is a contract, not a best effort:

- `stop` pauses the VM, writes a full Firecracker snapshot (memory, vCPU,
  devices), flushes the snapshot, memory file, and disk to stable storage,
  writes `suspension.json` atomically, and only then ends the VMM and
  reports `stopped`. If the snapshot cannot be written, the VM continues
  unchanged and the stop fails with Firecracker's own reason.
- `resume` loads only a VM whose suspension record is whole and written by
  the same VMM version. It loads the snapshot paused, retires the record
  (the snapshot must never be loaded over a disk the guest has since
  changed), continues the guest, and reports `ready` only once the guest
  agent answers.
- Without a valid record (the VMM ended some other way, the files are
  damaged, a different Firecracker wrote them) resume **fails**; the VM is
  not cold-booted in its place, and the session stays `stopped` with the
  reason.
- A stop or resume interrupted part way, including by a controller
  restart, is completed by the next attempt.

What survives: processes and their PIDs, guest memory (tmpfs, page cache),
and the disk. What does not: connections over vsock that were open at the
stop (a running `session exec` stream); the processes they started survive.
Integrity is checked by sizes and the device snapshot's digest, not a full
digest of guest memory.

**Status:** verified on KVM in CI (`microvm` job,
`crates/compute-cli/tests/microvm.rs`; first pass:
[run 39723092926](https://github.com/rkendel1/compute/actions/runs/39723092926)).
The test proves, through the real `compute serve` and CLI:

- a tmpfs file, a background process (same PID), and a counter it keeps
  in memory all survive stop and resume, and the counter continues;
- the controller restarting while the machine is suspended;
- egress reconnected after the resume;
- a snapshot that cannot be written fails the stop and loses nothing, and
  the retried stop succeeds;
- a repeated stop is a no-op, a resume of a running session a conflict;
- a damaged memory file is refused, with no cold boot in its place.

Lifecycle calls for one VM are serialized: a caller and the session sweep
can both drive one session, and two concurrent suspends of one VM were the
cause of every earlier failure of this job.

One run's timings, for orientation only (not a benchmark: one sample, a
debug build, a GitHub-hosted runner with nested KVM, 1 vCPU, 256 MiB):
create and boot 3.26 s, first command 137 ms, stop (suspend) 1.76 s,
resume to ready 136 ms.

## Machine fork

`compute session fork SESSION [--count N]` (`POST /compute/sessions/{id}/fork`,
1 to 16 children) on a provider that declares `fork`; anywhere else it is
refused with `operation_unsupported` and nothing is created.

| Question | Answer |
| --- | --- |
| What is forked | The running machine at one instant: guest memory, vCPU and device state, and the disk, all taken while the VM is paused. |
| The source | Must be running (`ready`/`running`); it is paused only for the checkpoint and continues, unchanged, afterwards. |
| What a child shares | Nothing mutable. Its own disk (a copy of the checkpoint's), its own VMM, vsock socket, and egress forward. Guest memory is mapped copy-on-write from the checkpoint's file, so a child's writes are its own. |
| Identity | Each child is a new session (new session, job, and VM identity) with its own owner-scoped authority. No execution, connection grant, or endpoint of the source is carried over. Processes inside the guest keep their PIDs: that is the point of a machine fork. |
| Randomness | Before a child is reported ready, its kernel's random generator is reseeded with fresh host entropy (`RNDADDENTROPY`, `RNDRESEEDCRNG`) through the guest agent; siblings otherwise continue from the same generator state. A child whose agent cannot reseed is failed. |
| Policy and admission | Every child is admitted under policy as a creation is, before the machine is touched; a refusal stops further children and is reported. Credentials are never in the guest (the egress gateway holds them), so nothing is copied that would need revoking. |
| Network | A child's network is its source's; with `network`, its only route out is its own forward to the gateway, which enforces the allowlist per connection. |
| Partial failure | Each child is provisioned on its own; one that cannot start is `failed` with its reason, and its siblings are unaffected. If the checkpoint cannot be taken, every child is failed with that reason and the source runs on. |
| Durability and restarts | Children are durable before the checkpoint is taken and name it in their `lineage`. After a restart, a child whose checkpoint exists is provisioned from it; one whose fork ended before the checkpoint existed is failed ("the fork ended before its checkpoint was taken"). The checkpoint is released once no child still needs it. |
| Evidence | A child's `lineage` names its parent session, the checkpoint, and the checkpoint's evidence (snapshot digest, sizes, VMM); its receipts' substrate names the checkpoint's snapshot digest (`artifacts.checkpoint`). |
| Limits | Only VMs booted with relocatable device paths (every VM created since fork existed) can be forked; an older VM is refused with that reason. A child has its source's resources and network. The disk is copied while the source is paused, so the pause grows with the disk's size. |

**Status:** verified. The manager's semantics (lineage, partial failure,
restart recovery, refusals, checkpoint release) are tested against a fake
provider; the Firecracker path by `crates/compute-cli/tests/microvm.rs` on
KVM, first passing in
[run 39741122138](https://github.com/rkendel1/compute/actions/runs/39741122138):
two children of a live machine with its memory, PIDs, counter, and disk;
one child's writes and kills reaching neither the source nor its sibling;
diverging `/dev/urandom`; egress per child; a receipt naming the checkpoint
that verifies; lineage across a controller restart; an unwritable
checkpoint failing the child with its reason while the source runs on; the
checkpoint released; nothing left after destroy. One sample (debug build,
nested KVM): two children ready in 6.8 s.

## Interactive commands

`compute session exec --tty` runs a command on a terminal (a PTY in the
guest) and attaches to it; `--interactive` keeps its standard input open
without a terminal. `--detach` returns the job, and `compute session attach
SESSION JOB` attaches; with `--tty` it drives the command from this
terminal (raw input, size changes forwarded, Ctrl-] detaches and leaves it
running). Terminal handling is always that explicit flag, never inferred
from whether standard input is a terminal;
`compute session input` sends data, a resize, a signal, or the end of input.

- **The job stays authoritative.** An interactive command is a durable job
  like any other: its output is the job's log (bounded as any output is),
  its receipt is the same, and cancellation and timeouts apply.
- **Output** is read from the job's log from a byte offset, with a long
  poll (`POST /compute/jobs/{id}/output`): a client that reconnects reads
  from where it was, `--from-start` replays from the beginning, and nothing
  is lost while no one is attached. Polling (`--follow`, `logs`) still works.
- **Input** (`POST /compute/sessions/{id}/input`, owner-scoped) goes to the
  running command over a node-local, owner-only socket the job's process
  listens on; it never starts a command, so attaching again reaches the
  same one. Input for an ended execution is refused.
- **Advertised only where implemented:** microVM sessions declare
  `terminal`; process and container sessions do not, and refuse `--tty` and
  `--interactive` before anything runs.

Tested: the guest agent with real PTYs (input, resize, end of input,
signals, a plain command taking no control); `compute guest-exec` with its
attach socket against the real agent (`tests/guest_terminal.rs`); the
output API and refusals on the session server; and through the CLI on KVM
(`microvm.rs`: a PTY in the guest, typed input, exit codes, a resize, a
detached command reattached with its earlier output, a signal, a receipt
that verifies). Not yet exercised: a microVM session created with a
`restricted` or `isolated` host profile (the job's client process must be
able to create its attach socket), and interactive commands in containers.

## Not implemented

- Interactive commands in process and container sessions.
- Measured startup and lifecycle latency: the KVM test prints one run's
  timings; there is no benchmark (distributions over repeated runs) yet.
- A VM backend on macOS.
