# Security model

demur reviews untrusted pull request content with an LLM, so the pipeline
is an attack surface by design. This page states the defenses and their
limits.

## Pull request content is data

Diff and pull request content enter prompts as delimited data, accompanied
by an instruction that content within the delimiters is never an
instruction to the bot. A hijacked model can at worst produce wrong
findings, which the human reviewer sees and dismisses. It cannot mutate
the repository, merge, or exfiltrate the key to a third party, because
egress goes only to the provider endpoints named in configuration and the
GitHub API.

## Model output is schema-validated

Every model response must satisfy a JSON schema before use. A
nonconforming response is retried within bounds, and on continued failure
the run fails with an error notice instead of publishing garbage. No
free-text model output triggers actions or is published as findings.

## No model-chosen execution

The bot never executes commands chosen by model output. No pass in this
change executes commands at all.

## Merge authority stays human

No pipeline outcome, model response, or configuration setting can cause a
merge. The bot's strongest actions are REQUEST_CHANGES and a failed check
run. It never commits, never edits code, and never auto-merges its own
suggestions.

## Keys

The provider key transits from your secret to the job environment to the
provider call. It is never written to disk, never logged, and never
included in any published artifact. Error messages from provider calls are
sanitized so the key cannot reach logs or check run output even when an
upstream error echoes request material. The bot holds no keys of its own
and has nowhere to send them.

## Least-privilege token

The job token needs exactly three permissions:

```yaml
permissions:
  pull-requests: write
  checks: write
  contents: read
```

When the supplied token lacks a required permission, the run fails naming
the missing permission rather than publishing partial results.

## Fork pull requests

Under the standard `pull_request` event, a fork pull request cannot read
repository secrets, so the bot cannot get the provider key. It writes an
actionable notice to the job summary and exits successfully instead of
failing contributor CI.

### Why `pull_request_target` is dangerous

The common workaround is to run the review under the `pull_request_target`
event, which does receive secrets. Checking out the pull request head
under that event executes untrusted code with repository secrets in scope:
anyone who can open a pull request can commit a modified workflow file and
exfiltrate every secret the job can read. That is why demur never
recommends this configuration, and why its documentation does not present
it as an option. If you must review forks, run the review on infrastructure
you control, or review the pull request locally with the CLI.

## What a compromised model can and cannot do

| Capability | Available to a hijacked model |
| ---------- | ----------------------------- |
| Produce wrong findings | Yes, visible to and dismissible by humans |
| Execute commands | No |
| Merge the pull request | No, no merge path exists |
| Reach the network beyond configured endpoints | No |
| Read or exfiltrate the provider key | No |
