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
| streaming stdin | **no** | **no** | **no** | no |
| output streamed while running | polling (`--follow`) | polling | polling | no |
| `terminal` (PTY, resize) | **no** | **no** | **no** | no |
| signals to a running command | cancel only | cancel only | cancel only | no |
| workspace files (`session file`) | yes | yes | yes | n/a |
| `network` | host network or `none`* | engine network or `none` | **egress gateway only** (no NIC) | none |
| `suspend` / `resume` (storage kept) | yes | yes (container stop/start) | yes | n/a |
| `memory_suspend` (processes and memory kept) | **no** | **no** | yes, verified on KVM | n/a |
| machine fork / branch | **no** | **no** | **no** | n/a |
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

## Not implemented

- Machine fork or branch (from memory or from a disk snapshot).
- Interactive terminals: PTY allocation, resize, streaming stdin, signals
  other than cancellation, reattaching to a running command's stream.
- Measured startup and lifecycle latency: the KVM test prints one run's
  timings; there is no benchmark (distributions over repeated runs) yet.
- A VM backend on macOS.
