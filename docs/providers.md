# Compute providers

A provider decides where a portable workload is executed. It does not change
what the workload means. `LocalProvider` calls the local Compute engine;
`RemoteProvider` transports the same canonical artifact to a Compute server.

```sh
compute provider capabilities local
compute provider capabilities http://127.0.0.1:8080
compute provider inspect http://127.0.0.1:8080 --json
```

Capability discovery is descriptive. Compute never substitutes a runtime,
weakens isolation, removes a network requirement, or falls back to host
dependencies based on discovery results. An incompatible request fails before
runtime selection wherever its requirement can be evaluated.

Provider identity is included in every execution result and sealed into its
`compute.receipt@1` receipt. Remote receipts also bind
`compute.remote@1`. Receipt verification is local and never contacts the
provider.

AppPort keeps `compute.inspect@1` and `compute.run@1` and additionally exposes
`compute.provider.inspect@1`, `compute.provider.capabilities@1`, and
`compute.provider.run@1`. These capabilities select a provider without an
AppBoundry dependency.

