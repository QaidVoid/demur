# Troubleshooting

## The run failed with `provider key problem`

demur resolves the key from `key_env` first, then `key_file`. The error names
which one it tried.

- The environment variable is unset or empty in the job. In the Action, the key
  is passed through `env:`, not through `with:`, and the variable name must
  match `key_env` exactly.
- `key_file` names a path that cannot be read. demur reports the read error
  rather than reporting a missing key, so check the path and its permissions.
- The provider rejected the key. This fails fast with no retries, because
  retrying a rejected key only makes the failure slower.

## `provider rejected the request`

A permanent rejection from the provider or a gateway in front of it. The error
carries the size of the request and an excerpt of its start and end, so the
content that triggered it is identifiable without paying for the run twice.

Gateways that translate between dialects are the usual source. If the excerpt
shows ordinary review content, the rejection is the gateway's interpretation of
it rather than anything demur constructed. Try the same model through its
native endpoint to confirm.

A rejection on the verdict summary pass no longer costs you the run: the review
publishes without the summary paragraph and says so.

## `prompt exceeds the model context window`

demur shrinks the context to the highest-risk hunk and retries once. If that
still overflows, the pass is skipped and disclosed. Narrow the input with
`ignore.paths`, or raise the deep model to one with a larger window.

## `provider hit the output token ceiling`

The model spent its whole output budget without finishing the JSON. demur
raises the ceiling and retries, up to twice and never past a limit any current
model will grant. If it persists, raise `limits.max_tokens` toward a value your
model actually supports, or reduce `limits.comments` so less output is needed.

## The review says coverage was reduced

That is the budget working. The review names the rung that fired:

- *context was shrunk* means the pass ran on the highest-risk hunks only.
- *ran on the triage model* means the pass was downgraded to the cheaper model.
- *deep call ceiling left N clusters* means `limits.deep_calls` bound before the
  budget did.
- *failed and was skipped* means that one pass failed against the provider and
  the rest of the run continued without it.

Raise `budget.per_pr_usd` or `limits.deep_calls` depending on which one is
named.

## The check is green but I expected findings

- Every finding may be below your `block_on` severities. The check reflects the
  verdict, not the finding count.
- The findings may have been dismissed on an earlier run. Resolved threads stay
  silent by design. Unresolve the thread to bring the finding back.
- The pull request may be a draft, which is skipped by default.

## The check went red and I cannot see why in the delta

A blocker from an earlier run carries forward until it is resolved. Pushing
unrelated commits does not clear it. Resolve the thread if you disagree, or fix
the cited lines.

## Nothing happened on a pull request from a fork

Expected. Fork pull requests cannot read secrets under the `pull_request`
event, so there is no provider key. The job summary explains it and the job
exits successfully. See the [Action reference](/reference/action).

## The docs build fails after I changed configuration

The configuration reference is enforced by a test: every schema key must appear
in `docs/reference/configuration.md`. Add the key to the reference page and the
build passes.
