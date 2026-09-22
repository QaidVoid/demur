# Cost and budgets

<p class="verdict">Budgets are denominated in money, not tokens, because a
triage token and a deep token differ in price by an order of magnitude. demur
prices every pass before it runs and refuses to run into debt.</p>

## Why money and not tokens

A token cap is only honest when every pass uses one model at one price. demur
gives each role its own model on purpose, so a single token budget would
under-count exactly the passes that cost the most. Each model therefore carries
its own prices in configuration:

```toml
[models.deep]
provider = "ajam"
name = "your-deep-model"
input_price = 3.00          # USD per million input tokens
output_price = 15.00        # USD per million output tokens
cached_input_price = 0.30   # optional; defaults to half of input_price
```

No price table ships with demur. A bundled table goes stale silently, and a
stale table under-reports spend, which is the failure a cost control tool can
least afford. A configured model with no price fails the run with an error
naming the model.

## The cap

```toml
[budget]
per_pr_usd = 5.00
```

The cap applies to the pull request's **cumulative** spend across runs, not to
a single run, so pushing ten times cannot spend the cap ten times. Prior spend
is read from the markers demur wrote on its own earlier reviews.

Omitting `[budget]` selects a built-in default cap. Unlimited spending happens
only when you ask for it explicitly.

## The degradation ladder

When the remaining budget will not cover a pass at full size, demur walks a
fixed ladder rather than failing:

1. **Shrink the context** to the highest-risk hunks and run the pass.
2. **Downgrade the model** for that pass to the triage model.
3. **Stand down** the remaining passes and synthesize from what completed.
4. **Skip** the review entirely, with an explanatory notice.

Every rung that fires is named in the published review. A green check run never
conceals that coverage was reduced, and a skipped run never clears a blocker an
earlier run established.

## Bounding the call count

The budget bounds money. It does not bound wall clock or call count, and a wide
pull request with four lenses enabled can run dozens of deep dives inside a
generous budget. That is what the separate ceiling is for:

```toml
[limits]
deep_calls = 12   # maximum deep dive calls per run
comments = 10     # maximum findings published inline
max_tokens = 2000 # output ceiling per call
```

When the ceiling binds, the highest-risk clusters are reviewed first and the
review names the clusters that received no deep dive.

## Concurrency changes the clock, not the bill

Deep dives are independent: each takes one file cluster under one lens and
reads nothing another produced. They run several at a time.

```toml
[limits]
concurrency = 4   # deep dives in flight at once; 1 runs them serially
```

A deep call costs roughly the same wall clock whatever the diff size, because
the time goes on the model thinking rather than on reading the diff. Twelve
calls at a minute each is a twelve minute review when run one at a time, and
about three at a concurrency of four.

What does not change is anything else. The same passes send the same requests
to the same models, so the spend is identical, the findings are identical, and
the order they are ranked and published in is identical. A run at
`concurrency = 1` and the same run at `concurrency = 8` differ only in how long
you waited.

Raising it past a handful usually stops helping. The limit that binds is your
provider's rate limiting, not your machine: past some point the extra calls
come back as rate limit responses, the retry backoff waits them out, and the
run gets slower without getting cheaper. On a shared endpoint the cost of being
greedy also lands on whoever else is using it. Start at the default and raise
it only if you measure an improvement.

Setting it to `1` restores exactly the serial behavior, which is the right
first move if you ever suspect concurrency of anything.

## Reading the spend report

Every review ends with its own accounting:

```markdown
### Coverage and spend

- deep dive (security) on src/big.rs: context was shrunk to the highest-risk content
- triage spend: $0.0021
- deep dive security on src/big.rs spend: $0.1087
- deep dive correctness on src/util.rs spend: $0.1107
- Total spend this run: $0.2215
- Cumulative spend for this pull request: $0.6321 (earlier runs: $0.4106)
```

Cached input is priced at the cached rate where the provider reports it
separately, which matters because demur sends the same cluster context to
several lenses in a row and marks the stable part cacheable.
