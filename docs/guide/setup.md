# Setup

demur is a code review bot for GitHub that runs with your own LLM provider
key. It reviews a pull request by triaging the changed hunks, deep diving
the risky clusters, cross-examining the pull request adversarially on the
deep profile, and publishing one review event plus one check run whose
conclusion gates the merge.

## Install the GitHub Action

1. Copy the example workflow from this repository at
   `docs/example-workflow.yml` into `.github/workflows/demur.yml`.
2. Commit a `.demur.toml` file at the repository root. The
   [configuration reference](/reference/configuration) lists every field
   and the [providers guide](/guide/providers) covers credentials.
3. Add your provider key as a repository secret. The example workflow maps
   the secret `DEMUR_PROVIDER_KEY` to the environment variable named by
   `key_env` in your configuration.

```yaml
permissions:
  pull-requests: write
  checks: write
  contents: read

concurrency:
  group: demur-${{ github.event.pull_request.number }}
  cancel-in-progress: true
```

The permissions block above is exactly what the bot needs: write access to
pull requests and check runs, read access to contents, and nothing else.
The concurrency group cancels an in-flight review when a new push arrives,
so a superseded verdict can never land on a newer head.

## What the review looks like

A completed run publishes exactly one review event carrying the verdict
synthesis computed, findings as inline comments anchored to the cited
lines, and one check run: `failure` when the verdict is REQUEST_CHANGES,
`success` when it is APPROVE. The review body always reports the money
spent per pass and the cumulative spend for the pull request.

## Fork pull requests

A fork pull request cannot read repository secrets, so the bot cannot read
your provider key. The job writes an actionable notice to the job summary
and exits successfully rather than breaking contributor CI. The
`pull_request_target` workaround is dangerous and is not recommended; the
[security model](/guide/security) explains why.
