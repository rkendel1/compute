# Running Compute on a Hetzner host

This is a runbook for a single dedicated host (any Linux server works; nothing
here is Hetzner-specific) running the Compute daemon, with Managed FeltDB as
the durable control plane, and `preprod` and `production` environments.

```text
Internet ─▶ :80 / :443 ingress (ACME, TLS by SNI)
Operators ─▶ :8787 Compute API (TLS, operator credentials)
Hetzner host
├── Compute controller (systemd)   ← API, reconciliation, ingress
│   ├── Managed FeltDB  ← desired state, evidence, bundles
│   └── Hetzner DNS     ← A records for your domains
└── Compute supervisor             ← the data plane: workloads and endpoints
    ├── preprod                      keep running when the controller
    └── production                   restarts, crashes, or is upgraded
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
install -d -m 0750 -g compute /etc/compute/tls                      # the API's certificate and key:
install -m 0640 -g compute api.crt api.key /etc/compute/tls/        # renew by replacing them; no restart
install -m 0644 compute.service /etc/systemd/system/compute.service
systemctl daemon-reload
systemctl enable --now compute
```

The unit runs the controller in production mode: the API is served only
over TLS, and every request, reads included, needs an operator
credential. Without a certificate the controller refuses to start; it
never falls back to plaintext. On first start it writes a bootstrap admin
token to `/var/lib/compute/bootstrap-admin.token` (mode 0600). Use it once
to create credentials for operators, then revoke it:

```sh
export COMPUTE_DAEMON=https://compute.example.com:8787
export COMPUTE_CA_CERT=/path/to/ca.pem          # only for a private CA
export COMPUTE_DAEMON_TOKEN=$(sudo cat /var/lib/compute/bootstrap-admin.token)
compute auth create --operator alice --scope compute.read --scope compute.deploy --scope compute.operate --expires-in 90d
compute auth list
compute auth revoke <bootstrap credential id>
compute doctor                                   # controller, data plane, FeltDB, TLS, auth, isolation
```

`compute doctor` must show the state as `feltdb … (connected)`. If FeltDB
is unreachable when the controller starts, it starts in
`degraded_control_plane`: workloads and endpoints keep running, reads are
served from its last snapshot and say how fresh they are, and changes
return `503 state_unavailable` until FeltDB is back. It never falls back to
local state. Pass `--require-state-at-start` to refuse to start instead.

Open ports 80 and 443 in the host's firewall for ingress, and 8787 only to
the addresses operators use. The unit grants
`CAP_NET_BIND_SERVICE` so the daemon binds them without running as root.
The Hetzner DNS token is a Hetzner Cloud API token with read and write
access to the project that holds the zone.

## 4. Environments and first deployment

```sh
export COMPUTE_DAEMON_TOKEN=cmpt_...     # your operator credential
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
| The controller crashes or is restarted | Workloads and endpoints keep serving on the supervisor. systemd restarts the controller, which reattaches to them without restarting any. Domains on ports 80/443 pause until it is back (ingress lives in the controller). |
| The supervisor dies | The controller starts a new one and restarts the workloads it lost. |
| The host reboots | The controller starts and restores desired state from FeltDB. |
| The host is replaced | Install and configure a new host (steps 1 and 3). It restores everything from FeltDB, bundles included. |
| FeltDB is unreachable | Running services keep running. Reads are served from the last snapshot (`X-Compute-State: stale`); changes return `503 state_unavailable` until FeltDB is back, then the controller reconciles. |
| The controller restarts mid-release | The next controller reloads the release from FeltDB and finishes it. |
| A credential leaks | `compute auth revoke <id>` (or `rotate`); it is refused on the next request. Every change is in `compute auth audit`. |
| A release fails or never becomes ready | It fails; the current revision keeps serving. |
| A DNS record is changed at Hetzner | Compute restores it within `dns_interval_seconds` and records `network.dns.drifted`. |
| A certificate nears expiry | Compute renews it 30 days before expiry; the valid one keeps serving if renewal fails. |

## 6. Upgrading Compute

Stop the controller with the unit and start the new binary; the workloads
keep running on the supervisor throughout:

```sh
install -m 0755 compute.new /usr/local/bin/compute
systemctl restart compute        # SIGTERM detaches; the new controller reattaches
compute doctor
```

`compute node upgrade --artifact <path> [--sha256 <digest>]` hands the node
over to a new build from a running controller and rolls back to the
previous build if the new one does not reattach every workload in time.
It has been exercised with the controller started directly (`compute
start`), not under systemd: when the old process exits after the
hand-over, systemd considers the unit stopped. Under systemd, use the
restart above until that integration is verified.

## 7. Cutting over from Fly

1. Deploy each project to `preprod` and `production` here, and verify it on
   its endpoint (through the tunnel).
2. Add each domain with `compute domain add`. Compute points its DNS at this
   host and issues its certificate once DNS resolves here. If the zone
   stays elsewhere for now, add it with `--dns-provider none` and move the
   record yourself.
3. Watch `compute events --follow`, `compute domain status`, and the UI
   through the change.
4. Remove the Fly apps.
