# Configuration reference

demur reads one file, `.demur.toml`, at the repository root. All
distributions (GitHub Action, local CLI) honor the same file and format.
Unknown keys fail the run rather than being ignored, so a typo cannot
silently disable a setting.

## Minimal working example

```toml
[providers.openai]
family = "openai"
base_url = "https://api.openai.com/v1"
key_env = "OPENAI_API_KEY"

[models.triage]
provider = "openai"
name = "gpt-4o-mini"
input_price = 0.15
output_price = 0.60

[models.deep]
provider = "openai"
name = "gpt-4o"
input_price = 2.50
output_price = 10.00

[models.verdict]
provider = "openai"
name = "gpt-4o-mini"
input_price = 0.15
output_price = 0.60
```

Every setting below this example has a working default.

## Top level

```toml
profile = "standard"   # "quick", "standard", or "deep". Default: standard.
```

Profiles select the pass set: quick runs triage and verdict only, standard
adds deep dives, deep adds the adversarial cross-examination pass.

## `[lenses]`

Lens toggles for deep dives. All default on except style.

```toml
[lenses]
correctness = true   # logic errors, edge cases, broken contracts
security = true      # injection, authorization, secret handling
performance = true   # regressions, complexity, allocation churn
style = false        # style and idiom; off unless explicitly enabled
```

## `[block_on]`

The severities whose findings force REQUEST_CHANGES.

```toml
[block_on]
severities = ["blocker"]   # any of "blocker", "warning", "note"
```

Default: blocker alone. Findings carried forward from earlier runs count
toward the verdict on equal terms with findings from the current run.

## `[ignore]`

Path patterns excluded from every pass.

```toml
[ignore]
paths = ["**/Cargo.lock", "dist/**"]
```

## `[budget]`

The per-pull-request spending cap, denominated in money, applied to the
pull request's cumulative recorded spend across runs.

```toml
[budget]
per_pr_usd = 5.0   # cap in USD. Default: the built-in cap of 5.0.
# unlimited = true # remove the cap entirely; explicit only
```

Set `per_pr_usd` or `unlimited`, never both. When the cap is reached the
degradation ladder applies in a fixed order: shrink context to the
highest-risk hunks, run remaining deep passes with the triage model,
publish a summary-only review, or skip with an explanatory notice. Every
degradation is disclosed in the published review.

## `[limits]`

```toml
[limits]
deep_calls = 12   # maximum deep dive calls per run. Default: 12.
comments = 10     # maximum published findings per review. Default: 10.
```

When the ceiling binds, the highest-risk clusters are reviewed first and
the review states how many clusters went unreviewed. The comment budget
never changes the verdict: a verdict-setting finding cut for space is
still named in the review body.

## `[providers.<name>]`

```toml
[providers.openai]
family = "openai"                  # "openai" or "anthropic"
base_url = "https://api.openai.com/v1"
key_env = "OPENAI_API_KEY"
key_file = "/home/me/.secrets/openai"   # optional, local runs
```

Passthrough parameters are forwarded unmodified:

```toml
[providers.openai.extra_body]
top_p = 0.9

[providers.openai.extra_headers]
"X-Custom-Header" = "value"
```

## `[models.<role>]`

Roles are `triage`, `deep`, and `verdict`; every role is required.

```toml
[models.triage]
provider = "openai"          # name from the providers table
name = "gpt-4o-mini"
input_price = 0.15           # USD per million input tokens
output_price = 0.60          # USD per million output tokens
cached_input_price = 0.075   # optional; defaults to half the input price

reasoning_effort = "high"    # openai family: minimal, low, medium, high
thinking_budget = 8000       # anthropic family: tokens

[models.triage.extra_body]   # optional passthrough
custom_sampling_control = 42
```

`reasoning_effort` applies to the openai family only, `thinking_budget` to
the anthropic family only, and the wrong combination fails validation.
A model without a price fails the run closed.
