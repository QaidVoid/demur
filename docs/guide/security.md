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

## The resume cache

The [resume cache](/guide/cache) stores the output of completed review passes.
That output is derived from pull request content, which is untrusted, so the
entries are untrusted too and are treated that way.

**Entries are revalidated on the way in.** A retrieved entry is checked against
the pass schema exactly as a live provider response is, and its findings go
through the same finding contract. The strongest thing a tampered entry can do
is what a hostile model response can already do: produce a wrong finding that a
human reviewer sees and dismisses. It cannot introduce a finding a live pass
could not, and it cannot change the verdict rule, the budget, or any
configuration, because none of those are stored in an entry.

**Fork pull requests never read the cache.** A workflow cache can be written by
a job running for a fork's pull request, and an entry written there carries
model output derived from that fork's own content. Reading one in a trusted run
would let a fork place a finding under this repository's reviewer. Rather than
depend on the cache scoping rules of the execution environment, a fork run is
always cold. It costs the contributor nothing, since a fork run has no provider
key and publishes a notice anyway.

**Nothing secret is stored.** An entry holds pass output and token counts. The
provider key, the GitHub token, the configuration, and the rendered prompt are
never written to it. The key is a digest, so the cache holds answers rather than
questions.

## Context retrieval

With [retrieval](/guide/retrieval) enabled, a pass may name repository content
it needs and demur fetches it. This is the only place model output influences
what the bot reads, so it is worth being exact about what it can and cannot do.

**Naming is not executing.** The model returns a string. demur decides what
that string means, whether the allowlist admits it, and what to do when it does
not. Nothing is run on a model's behalf: not a build, not a test, not a
language server, not a search tool. A request shaped like a shell fragment is a
name that matches no file, and it fails the way a typo fails.

**Reads cannot leave the checkout.** Paths are resolved and then checked for
containment, so traversal and symbolic links pointing outside are both refused
by the same rule. The version control directory, the configuration file, and
ignored paths are refused. So is the file named by `key_file`, wherever it
points, because a repository may keep its key inside its own tree and a pull
request must not be able to induce demur to read a credential into a prompt.

**Retrieved content is untrusted.** It enters the prompt delimited, with the
same instruction the diff gets: content inside the delimiters is never an
instruction. A retrieved file that says to approve the pull request is reviewed
text, not a command.

**Never for untrusted authors.** Retrieval does not run for a pull request from
a fork. The allowlist stops such an author reaching past it, but not from
steering which allowed file is read and therefore what appears in a review a
maintainer reads.
