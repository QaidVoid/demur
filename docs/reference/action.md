# GitHub Action reference

The composite action downloads the prebuilt `demur-action` binary for the
runner's platform and runs one review against the pull request that triggered
the workflow.

## Minimal workflow

```yaml
name: demur review

on:
  pull_request:
    types: [opened, synchronize, ready_for_review]

permissions:
  pull-requests: write
  checks: write
  contents: read

concurrency:
  group: demur-${{ github.event.pull_request.number }}
  cancel-in-progress: true

jobs:
  review:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: qaidvoid/demur@v0
        with:
          github_token: ${{ secrets.GITHUB_TOKEN }}
        env:
          # Name this exactly as key_env in your .demur.toml.
          OPENAI_API_KEY: ${{ secrets.DEMUR_PROVIDER_KEY }}
```

## Inputs

| Input | Default | Meaning |
| --- | --- | --- |
| `github_token` | the job token | Token used for the review event, the check run, and reading the diff. |
| `demur_version` | current release tag | Release to download the binary from. |
| `profile` | unset | Overrides `profile` in `.demur.toml` for this run: `quick`, `standard`, or `deep`. |
| `cache` | `false` | Keep completed review passes between attempts of the same job, so a retried run does not pay twice. It can only change what a run costs, never what it concludes, and it is never read on a pull request from a fork. |

The provider key is not an input. It is passed through `env` under whatever
name your configuration's `key_env` declares, so the key never becomes an
action argument that could land in a log line.

## Permissions

The permissions block above is exactly what demur uses and nothing more:

| Permission | Why |
| --- | --- |
| `pull-requests: write` | Publish one review event with inline comments. |
| `checks: write` | Publish one check run carrying the verdict. |
| `contents: read` | Read the diff and repository content. |

demur never requests `secrets`, `administration`, or write access to code. If
the supplied token is missing a permission, the run fails naming the missing
one rather than publishing a partial result.

## Concurrency

The `concurrency` group is not optional decoration. Without it, two pushes in
quick succession start two runs that both spend money, and the slower one can
publish a stale verdict over the newer head. With `cancel-in-progress`, the
superseded run is cancelled before it publishes and records no spend.

## Triggers

demur reviews on `opened`, `synchronize`, and `ready_for_review`. Draft pull
requests are skipped unless configuration says otherwise.

## Fork pull requests

Under the standard `pull_request` event, a pull request from a fork cannot read
repository secrets, so no provider key is available. demur writes an actionable
notice to the job summary, exits successfully, and makes no provider or review
API calls. A contributor's CI is never broken by a maintainer's key setup.

::: danger pull_request_target
The usual workaround is to switch to `pull_request_target`, which does have
secrets. Combining that event with a checkout of the pull request head runs
untrusted contributor code in a job that holds your provider key and a
write-scoped token. That is remote code execution against your repository, not
a configuration nuance. demur does not ship it as a default and does not
recommend it.
:::

## Approving reviews

Where the job identity is not permitted to submit an approving review, demur
publishes the same body as a comment review and lets the check run carry the
verdict. The review body says the approval could not be submitted under the
current identity, so a clean run never looks like a failed one.
