# Egress gateway

`compute egress serve` runs the gateway a session uses to reach the network.
It allows only the destinations its policy names, connects only to addresses
it resolved and checked itself, and adds credentials for chosen hosts on the
way out, so those credentials never enter the session.

```sh
export GITHUB_TOKEN=…            # in the gateway's environment, not the session's
compute egress serve --rules egress.json --listen 127.0.0.1:3128 --ca-out egress-ca.pem
```

```json
{
  "allow": [
    { "host": "api.github.com" },
    { "host": "*.pypi.org" },
    { "host": "registry.npmjs.org", "ports": [443] }
  ],
  "credentials": [
    { "host": "api.github.com", "header": "authorization", "prefix": "Bearer ", "value_env": "GITHUB_TOKEN" },
    { "host": "api.openai.com", "header": "authorization", "prefix": "Bearer ", "value_file": "/home/me/.config/compute/openai.token" }
  ]
}
```

A session then runs with `HTTPS_PROXY=http://127.0.0.1:3128` (and
`HTTP_PROXY`) and trusts `egress-ca.pem` (for example `SSL_CERT_FILE`,
`NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`) for the hosts that have a
credential.

## What it enforces

| | |
| --- | --- |
| **Allowlist** | A host is reached only when a rule names it exactly, or `*.suffix` for its subdomains (never the apex), on an allowed port (443 unless `ports` says otherwise). Everything else is `403`. |
| **DNS pinning** | The gateway resolves the name once and connects to that address. Every answer must be a public address unless `allow_private` is set, so an allowed name cannot be pointed at the node, its metadata service, or the private network. |
| **Name binding** | A tunnel carries only what it was opened for: a TLS ClientHello must name the `CONNECT` host (no domain fronting to another tenant at the same address), a tunnel on 443 must be TLS, and a plain-HTTP request's `Host` must be the host in its URL. |
| **Credential brokering** | For a host with a credential, the gateway terminates TLS with a leaf from its own CA, requires the request's `Host` to be that host, replaces the header (a value the session sent is discarded), allows one request per connection, and re-encrypts to the upstream with full certificate verification. |
| **A constrained CA** | The gateway's CA is generated at start, lives only in memory, and carries X.509 name constraints for the credentialed hosts: even trusted by a session, it cannot vouch for any other name. |
| **No cleartext credentials** | Plain `http://` requests to allowed hosts are forwarded, but never with a credential. |
| **Evidence** | Every connection is a decision (host, port, allowed, brokered, reason); `serve` prints them as JSON lines when it stops. |
| **Fails closed** | Unreadable requests, unresolvable names, upstreams that fail verification, and anything unexpected end the connection. |

Other allowed hosts are tunnelled untouched: end-to-end TLS between the
session and the upstream, which the gateway cannot read.

## Where the boundary is

The gateway is the policy; the session's network is the enforcement. A
session whose only route out is the gateway (a container on a network with
no other egress, a VM whose only interface reaches the gateway) cannot go
around it. A session with ordinary network access can ignore
`HTTPS_PROXY`. A microVM session (`compute serve --session-provider microvm
--vm-egress ADDR`, [sessions.md](sessions.md)) is the first kind with no
other route: its VM has no network interface, and its only way out is the
gateway, over vsock. For containers and workspaces, pair the gateway with a
network that has no other egress.

## Credential custody: the local trust boundary

The rules file names where each credential is kept and never holds it: a
rules file with a `value` is refused. A credential is `value_env` (a variable
in the gateway's own environment) or `value_file` (a file readable only by
its owner; anything looser is refused), with an optional `prefix` such as
`Bearer `. Values are read once, at start, and live only in the gateway's
memory: never in a session, a rules file, or a job request.

That is **local custody**, and the gateway says so when it starts
(`credential custody: local …`). It keeps credentials out of what a session
can reach; it does not protect them from the account that runs the gateway,
and it is not production-grade custody. Custody mediated by AuthBoundry, the
system that already holds credentials for AppPort services, is a separate
integration: the gateway would resolve a binding through it instead of
reading a value itself, and nothing in the rules format would change but
the reference.
