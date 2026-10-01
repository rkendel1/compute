# Recipe examples

The authoritative starter catalog is [`recipes/starters`](../../recipes/starters).
Discover it with `compute recipe starters` and copy a template into a durable,
user-owned recipe with `compute recipe create NAME --from STARTER`.

## Optional recipes

Recipes that are not starters live here and are used with `--file`:

| File | For |
| --- | --- |
| [`github-actions-runner.json`](github-actions-runner.json) | The environment of one ephemeral GitHub Actions runner job ([docs/github-actions-runner.md](../../docs/github-actions-runner.md)) |

```sh
compute recipe validate --file examples/recipes/github-actions-runner.json
compute recipe create github-actions-runner --file examples/recipes/github-actions-runner.json
```

A recipe describes an environment. It never contains a repository, a token,
or any other credential.
