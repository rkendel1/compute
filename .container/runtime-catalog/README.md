# Runtime catalog acceptance fixture

The five applications under `apps/` exercise the same placement, runtime
resolution, preparation, verification, execution, and receipt path. Run each
against both `auto` and an explicit Apple Container provider:

```sh
for runtime in wasm node python deno bun; do
  compute run ".container/runtime-catalog/apps/$runtime" --provider auto \
    --pool-config .container/runtime-catalog/pool.toml --json
  compute run ".container/runtime-catalog/apps/$runtime" --provider remote \
    --pool-config .container/runtime-catalog/pool.toml --json
done
```

The catalog currently supplies both `linux-x86_64` and `linux-aarch64`
artifacts for Node, Python, Deno, and Bun. WASM is embedded and portable on
both architectures. A successful receipt records the requested constraint and
the exact resolved distribution, artifact digest, executable identity,
provider, placement, admission, and execution evidence.
