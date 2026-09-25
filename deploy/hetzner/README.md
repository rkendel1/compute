# Running Compute on a Hetzner host

This is a runbook for a single dedicated host (any Linux server works; nothing
here is Hetzner-specific) running the Compute daemon, with Managed FeltDB as
the durable control plane, and `preprod` and `production` environments.

```text
Internet ─▶ :80 / :443 ingress (ACME, TLS by SNI)
Hetzner host
└── Compute daemon (systemd)
    ├── Managed FeltDB  ← desired state, evidence, bundles
    ├── Hetzner DNS     ← A records for your domains
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
install -m 0640 -g compute compute.toml /etc/compute/compute.toml   # set url, application, public_ipv4, zone
install -m 0600 compute.env.example /etc/compute/compute.env        # set the tokens
install -m 0644 compute.service /etc/systemd/system/compute.service
systemctl daemon-reload
systemctl enable --now compute
compute status --daemon http://127.0.0.1:8787
```

`compute status` must show `Control state: feltdb (…)`. If FeltDB is
unreachable, the daemon refuses to start and systemd retries. It never falls
back to local state.

The API and UI listen on localhost. To reach the UI, use an SSH tunnel
(`ssh -L 8787:127.0.0.1:8787 host`, then open `http://127.0.0.1:8787/ui/`).
Open ports 80 and 443 in the host's firewall for ingress. The unit grants
`CAP_NET_BIND_SERVICE` so the daemon binds them without running as root.
The Hetzner DNS token is a Hetzner Cloud API token with read and write
access to the project that holds the zone.

## 4. Environments and first deployment

```sh
export COMPUTE_DAEMON_TOKEN=...          # the same token as compute.env
compute environment create preprod
compute environment create production
compute deploy feltdb --environment preprod --source ./feltdb --revision v0.11.7 --wait
# verify preprod, then release the exact revision to production:
compute deploy feltdb --from preprod --to production --set DATABASE_URL=... --wait
compute domain add feltdb.example.com --environment production --project feltdb
compute domain status feltdb.example.com       # DNS, TLS, and routing
```

Every later release has zero downtime ([docs/releases.md](../../docs/releases.md)):

```sh
compute deploy feltdb --environment preprod --source ./feltdb --revision v0.11.8 --wait
compute deploy feltdb --from preprod --to production --wait
compute deployment rollback dep_…              # if it must be undone
```

## 5. Operating

| Situation | What happens |
| --- | --- |
| A service crashes or is killed | It restarts with backoff (default `on_failure`). |
| The daemon crashes | systemd restarts it. It reaps the orphaned services and restores them on the same ports. |
| The host reboots | The daemon starts and restores desired state from FeltDB. |
| The host is replaced | Install and configure a new host (steps 1 and 3). It restores everything from FeltDB, bundles included. |
| FeltDB is briefly unreachable | Running services keep running. Changes return `503` until FeltDB is back. |
| The daemon restarts mid-release | The next daemon reloads the release from FeltDB and finishes it. |
| A release fails or never becomes ready | It fails; the current revision keeps serving. |
| A DNS record is changed at Hetzner | Compute restores it within `dns_interval_seconds` and records `network.dns.drifted`. |
| A certificate nears expiry | Compute renews it 30 days before expiry; the valid one keeps serving if renewal fails. |

## 6. Cutting over from Fly

1. Deploy each project to `preprod` and `production` here, and verify it on
   its endpoint (through the tunnel).
2. Add each domain with `compute domain add`. Compute points its DNS at this
   host and issues its certificate once DNS resolves here. If the zone
   stays elsewhere for now, add it with `--dns-provider none` and move the
   record yourself.
3. Watch `compute events --follow`, `compute domain status`, and the UI
   through the change.
4. Remove the Fly apps.
