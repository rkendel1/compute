# Running Compute on a Hetzner host

This is a runbook for a single dedicated host (any Linux server works; nothing
here is Hetzner-specific) running the Compute daemon, with Managed FeltDB as
the durable control plane, and `preprod` and `production` environments.

```text
Hetzner host
└── Compute daemon (systemd)
    ├── Managed FeltDB  ← desired state, evidence, bundles
    ├── preprod
    └── production
```

Compute has no Fly-specific assumptions. Existing Fly apps can keep running
while you prove the new deployment, and be removed once DNS points here.

## 1. Install

```sh
useradd --system --home /var/lib/compute --shell /usr/sbin/nologin compute
install -d -o compute -g compute /var/lib/compute
install -d -m 0750 /etc/compute
install -m 0755 compute /usr/local/bin/compute       # a built Compute binary
compute doctor                                        # runtimes available on this host
```

Workloads that need a runtime (Python, Node, …) need it installed on the
host, or a Compute distribution that includes it.

## 2. Provision the control plane in Managed FeltDB

Use a FeltDB API key with the scopes listed in
[docs/control-plane.md](../../docs/control-plane.md#provisioning-managed-feltdb):

```sh
export COMPUTE_FELTDB_TOKEN=fdb_live_...
compute control-plane provision --feltdb-url https://feltdb.example.com
```

It prints the application ID. Running it again changes nothing.

## 3. Configure

```sh
install -m 0640 -g compute compute.toml /etc/compute/compute.toml   # set url and application
install -m 0600 compute.env.example /etc/compute/compute.env        # set both tokens
install -m 0644 compute.service /etc/systemd/system/compute.service
systemctl daemon-reload
systemctl enable --now compute
compute status --daemon http://127.0.0.1:8787
```

`compute status` must show `Control state: feltdb (…)`. If FeltDB is
unreachable, the daemon refuses to start and systemd retries. It never falls
back to local state.

The API and UI listen on localhost. To reach the UI, use an SSH tunnel
(`ssh -L 8787:127.0.0.1:8787 host`, then open `http://127.0.0.1:8787/ui/`)
until the network control plane (domains, TLS, routing) lands.

## 4. Environments and first deployment

```sh
export COMPUTE_DAEMON_TOKEN=...          # the same token as compute.env
compute environment create preprod
compute environment create production
compute deploy feltdb --environment preprod --source ./feltdb --revision v0.11.7 --wait
# verify preprod, then:
compute promote feltdb --from preprod --to production --set DATABASE_URL=... --wait
```

## 5. Operating

| Situation | What happens |
| --- | --- |
| A service crashes or is killed | It restarts with backoff (default `on_failure`). |
| The daemon crashes | systemd restarts it. It reaps the orphaned services and restores them on the same ports. |
| The host reboots | The daemon starts and restores desired state from FeltDB. |
| The host is replaced | Install and configure a new host (steps 1 and 3). It restores everything from FeltDB, bundles included. |
| FeltDB is briefly unreachable | Running services keep running. Changes return `503` until FeltDB is back. |

## 6. Cutting over from Fly

1. Deploy each project to `preprod` and `production` here, and verify it on
   its host port (through the tunnel or the host's firewall rules).
2. Point DNS for each domain at this host, behind whatever TLS terminator
   you use today. First-class domains, TLS, and routing are the next
   Compute release.
3. Watch `compute events --follow` and the UI through the change.
4. Remove the Fly apps.
