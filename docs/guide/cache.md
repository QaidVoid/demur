# The resume cache

<p class="verdict">A retried run can reuse the passes an earlier attempt
finished instead of buying them twice. The cache changes what a review costs
and nothing else, and a missing cache is always safe.</p>

## What it is for

Most single failures no longer cost you a run: a failed deep dive, a failed
cross-examination, and a failed verdict summary all degrade and the review
publishes what it has. What remains is the class of failure that kills the
process rather than one call, and for that there is nothing to degrade into
because the process is gone: a job timeout, a cancelled job, or a provider
outage that outlasts the retries.

When that happens on a large pull request, minutes of deep dives and real money
go with it. The cache lets the next attempt recognize the passes it already
paid for.

## Turning it on

In a workflow, set the Action input. The workflow supplies the directory and
keeps it between attempts of the same job:

```yaml
      - uses: qaidvoid/demur@v0
        with:
          github_token: ${{ secrets.GITHUB_TOKEN }}
          cache: true
```

Locally, name a directory. Nothing is cached unless you do:

```bash
demur review main..HEAD --cache-dir .demur-cache
```

## The one guarantee

A cache can only change what a run costs. Findings, verdict, check run
conclusion, and the disclosed coverage are identical whether every pass came
from cache, some did, or none did.

Everything that could go wrong with a cache therefore has the same outcome as
not having one:

| Situation | Result |
| --- | --- |
| Not enabled | Cold run. Nothing is read or written. |
| Directory unusable | Cold run. The run does not fail. |
| Entry corrupt or truncated | Discarded. The pass runs. |
| Entry evicted | The pass runs. |
| Entry from a different model or configuration | Miss. The pass runs. |

This is also why the cache is never consulted for anything else. What was
already reviewed, what a human dismissed, and what the verdict is all still come
from demur's own review markers on GitHub. The cache answers exactly one
question: has this pass already been paid for.

## Why you get fewer hits than you expect

An entry is keyed by a digest of everything that decides what the pass would
send: the head commit, the pass and its lens, the rendered prompt, the model and
its parameters, and the output ceiling. Change any of them and it is a miss.

That is deliberate, and it is the difference between a cache and a bug. A
coarser key would hit more often and would sometimes answer a question this run
did not ask, with no way to notice, because a hit looks exactly like a success.
A miss costs money. A wrong hit costs trust.

In particular:

- **A new commit misses everything.** The head commit is in the prompt.
- **Changing a model, a lens, an ignore path, or a limit misses** the passes it
  affects.
- **A degraded pass is keyed as degraded**, so a pass that ran on a shrunk
  context cannot be reused by a later run with a full budget. Otherwise a run
  could publish reduced coverage as full coverage with no disclosure, because
  that run never degraded.

## What a resumed run reports

```markdown
### Spend

- triage spend: $0.0021 (resumed from cache)
- deep dive security on src/big.rs spend: $0.1087 (resumed from cache)
- deep dive correctness on src/util.rs spend: $0.1107
- Paid by this run: $0.1107
- Inherited from an earlier attempt: $0.1108 (triage, deep dive security on src/big.rs)
- Total spend this run: $0.2215
```

A resumed run is not a cheap review. It is a review that had to be paid for
across more than one attempt, and the numbers say so. The cumulative figure for
the pull request keeps counting every dollar actually spent, so enabling a cache
can never make spend appear to fall.

## Fork pull requests

The cache is never read on a pull request from a fork, and such a run is always
cold. See the [security model](/guide/security) for why.

## What is stored

The parsed output of a completed pass and the token usage that call reported.
Nothing else: no verdict, no configuration, no prompt, and no key. The key is a
digest, so the cache holds answers rather than questions.

Failed calls, truncated responses, and responses that failed schema validation
are never stored, so a retry always re-runs what actually went wrong.

Entries expire by age and the cache evicts oldest-first past its size bound. No
cleanup is ever required, because eviction is invisible except in cost.
