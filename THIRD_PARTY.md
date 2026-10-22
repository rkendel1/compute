# Third-party components

Compute itself is MIT licensed (LICENSE). The configured distribution (`compute-configured`)
also installs the exact npm packages pinned in `compatibility/published-stack/package-lock.json`
and the stack-agent executable built from `github:rkendel1/stack-agent` (MIT). Each keeps its
own license.

## Not open source

`@authboundry/core` and its four platform packages are **UNLICENSED**: proprietary, with their
own terms of service and acceptable-use policy shipped in the package (`legal/`). The package
runs a closed native binary (`bin/authboundry` from `@authboundry/core-<platform>`). They are
present because `@appport/services` depends on them; Compute does not run them itself.

## Licenses in the pinned set

| License | Packages |
| --- | ---: |
| MIT | 120 |
| Apache-2.0 | 18 |
| ISC | 9 |
| UNLICENSED | 5 |
| BSD-3-Clause | 1 |

Regenerate this table from the lockfile when the pinned set changes;
`compute-configured-verify` fails if a package outside these licenses appears.
