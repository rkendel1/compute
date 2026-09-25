# The network control plane

Compute routes domains to services, keeps their DNS records at a DNS
provider, and issues and renews their TLS certificates with ACME. Every
network resource reports what is wanted, what is, its status, its last
error, and when it was last reconciled.

```text
client ─▶ DNS (provider) ─▶ ingress :80 / :443 ─▶ endpoint ─▶ serving instance
                              │   ACME HTTP-01, redirect, TLS by SNI
                              ╰── routes follow traffic assignments
```

Compute is not a DNS server, a certificate authority, or a general load
balancer. It drives a DNS provider, uses an ACME CA, and forwards each domain
to exactly one endpoint.

## Domains

```sh
compute domain add app.example.com --environment production --project feltdb
compute domain status
compute domain inspect app.example.com
compute domain remove app.example.com
```

A domain belongs to one environment and routes to one workload port of one
project there. `--workload` and `--port` pick the service and the port.
They default to the project's only service with ports, and to its first
port. A domain name is unique across the control plane.

Isolation is enforced when a domain is added: it can only name a project
that is in its environment. The ingress routes a host only to its own
endpoint, and it answers an unknown host with `404`. A domain follows its
endpoint through every release. The endpoint does not change, only the
instance behind it. A project or environment that a domain routes to cannot
be removed until the domain is removed.

Each domain has three parts, and each reports its state as `desired`,
`actual`, `status`, `last_error`, and `last_reconciled_at`:

| Part | `status` |
| --- | --- |
| `routing` | `healthy`: ingress routes it to a serving endpoint. `pending`: nothing serves it yet. `degraded`: this node has no ingress. |
| `dns` | `healthy`, `pending`, or `failed`. `unmanaged` when DNS is yours (`--dns-provider none`). |
| `tls` | `healthy`: a valid certificate is served. `pending`, `degraded` (valid, but this node lacks the key), `failed`, or `disabled`. |

The domain's own `status` is `failed` if any part failed, `healthy` if every
part is healthy (or unmanaged or disabled), and otherwise `degraded` or
`pending`.

## Ingress

```toml
[network]
ingress_http = "0.0.0.0:80"
ingress_https = "0.0.0.0:443"
```

Ingress is the node's public entry. It is off unless configured, and
`--ingress-http` and `--ingress-https` set it too.

- **HTTP** answers ACME HTTP-01 challenges at
  `/.well-known/acme-challenge/`, redirects (308) a host that has a
  certificate to HTTPS, and forwards any other routed host to its endpoint.
- **HTTPS** terminates TLS with the certificate for the SNI name, then
  forwards the plaintext stream to the endpoint.

Binding ports 80 and 443 needs `CAP_NET_BIND_SERVICE`. The
[Hetzner kit](../deploy/hetzner) grants it.

## DNS providers

```toml
[network]
public_ipv4 = "203.0.113.10"      # what A records point at
# public_ipv6 = "2001:db8::10"    # and AAAA records
dns_interval_seconds = 60         # how often records are read back

[network.dns.hetzner]
kind = "hetzner"                  # Hetzner DNS, through the Hetzner Cloud API
zone = "example.com"
token_env = "HETZNER_DNS_TOKEN"

[network.dns.cloudflare]
kind = "cloudflare"
zone = "example.org"
token_env = "CLOUDFLARE_API_TOKEN"

[network.dns.local]
kind = "file"                     # a JSON file: development and hand-managed zones
zone = "example.test"
path = "zone.json"
```

A domain uses the provider named by `--dns-provider`. Otherwise it uses the
provider whose zone holds it (the longest zone wins). With neither, DNS is
unmanaged. For each address, Compute keeps one `A` or `AAAA` record for the
domain, with a TTL of 300.

Reconciliation reads every record back from its provider. A healthy record
is read every `dns_interval_seconds`, and one that is not healthy every 5
seconds. When the provider holds anything other than exactly the desired
value, Compute writes it back and records `network.dns.drifted` with what it
found, then `network.dns.applied`. `compute dns reconcile` (`POST
/dns/reconcile`) reads and repairs every record now. Removing a domain
removes its records at the provider.

**Credentials never enter control state.** A provider's token is read from
the environment variable its configuration names. A missing variable is
reported by name, in `compute network status` and in the record's
`last_error`. It is never shown, and never written anywhere.

## Certificates

```toml
[network.acme]
directory = "https://acme-v02.api.letsencrypt.org/directory"
contact = "mailto:ops@example.com"
renew_before_days = 30
# ca_file = "pebble.minica.pem"   # a private CA for the ACME server's own TLS (testing)
```

With ACME configured, every domain gets a certificate unless it is added
with `--no-tls`. Issuance uses HTTP-01 through ingress HTTP, so the domain's
DNS must point at this node. The certificate is served by SNI as soon as it
is issued. Renewal starts `renew_before_days` before expiry. A renewal that
fails leaves the valid certificate in place (`renewal_status: failed`) and
is retried after `certificate_retry_seconds` (default 300).

```sh
compute certificate status
compute certificate renew app.example.com
```

**Keys never enter control state.** The certificate's private key, its
chain, and the ACME account key are written to the node's secret store: a
directory readable only by the daemon's user (`<state-dir>/secrets`, mode
0700, files 0600). The `Certificate` record holds public facts only:

- issuer
- status and renewal status
- validity and expiry
- SHA-256 fingerprint
- a `secret_reference` of the form `node:<node id>/tls/<domain>/…`

A node that does not hold the key, such as a replacement node, issues a new
certificate. Certification with Pebble scans every control-state collection
and checks that none holds key or certificate material.

## Releases and the network

At `network_ready`, a release verifies its endpoints and records the DNS
and TLS state of the domains that route to it (see
[releases](releases.md)). A domain's DNS or TLS problem is recorded in the
release's `network_result` as `degraded`, but it does not hold a release
back: the domain belongs to the environment, not to the revision. A switch
moves every domain of the endpoint at once, because the domain routes to the
endpoint and the endpoint to the serving instance.

## Status

```sh
compute network status     # endpoints, ingress, DNS providers, the node's ID
compute dns status         # every managed record: desired, actual, status, error
compute domain status      # routing, DNS, and TLS of every domain
```

The same information is in the UI under **Domains**, and in the API at
`/network`, `/dns`, `/certificates`, and `/domains`.

## Events

- **Domains:** `domain.created`, `domain.removed`
- **DNS:** `network.dns.applied`, `network.dns.drifted`, `network.dns.failed`
- **Certificates:** `network.certificate.issued`,
  `network.certificate.renewed`, `network.certificate.failed`
- **Routes:** `network.route.switched`

## Certification

- `crates/compute-network/tests/providers.rs` runs the Hetzner and
  Cloudflare clients against faithful fakes of their APIs. Records are
  created, corrected after drift, and deduplicated. A wrong token is
  reported without revealing it.
- `crates/compute-network/tests/ingress.rs` checks that hosts route only
  where their domain says, that ACME challenges are answered, that HTTP
  redirects once a certificate exists, and that TLS terminates by SNI.
- `crates/compute-environment/tests/network.rs` checks, end to end:
  - domains route within their environment only
  - a DNS drift is repaired and recorded
  - a release moves a domain with its endpoint
  - a missing token is reported by variable name
  - Pebble issues, serves, and renews a certificate over HTTP-01, and no
    key reaches control state

  The Pebble test needs `pebble` and `pebble-challtestsrv` on `PATH` (or
  `PEBBLE_BIN` and `PEBBLE_CHALLTESTSRV_BIN`), and Pebble's source for its
  test CA (`PEBBLE_SOURCE`, or the Go module cache). Without them it skips.

## Limitations

- **Route53 is not implemented yet.** Its provider can be added behind the
  same `DnsProvider` trait as Hetzner and Cloudflare.
- **Only HTTP-01 challenges.** Wildcard certificates, which need DNS-01, are
  not issued.
- **One node's ingress.** DNS points at `public_ipv4` and `public_ipv6`, the
  addresses of the node running the daemon.
