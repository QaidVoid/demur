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
max_tokens = 2000 # output token ceiling per pass. Default: 2000.
concurrency = 4   # deep dives in flight at once. Default: 4. Use 1 for serial.
```

When the deep call ceiling binds, the highest-risk clusters are reviewed
first and the review states how many clusters went unreviewed. The comment
budget never changes the verdict: a verdict-setting finding cut for space
is still named in the review body.

Raise `max_tokens` when your provider runs reasoning-style models that
spend output tokens before writing an answer: a response that comes back
truncated is retried automatically at 4x and then 16x the configured
ceiling, and the tokens spent on truncated attempts still count against
the budget.

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

## `[review]`

Rules the repository declares about the pull request itself. Everything here
is optional; a section left out declares nothing and nothing is ever reported.

Rules are evaluated in process before any provider call. They cost nothing,
they produce the same answer every run, and no model response participates in
deciding them, so text in a pull request body cannot argue one away.

```toml
[review.title]
required = true              # a non-empty title is required
min_length = 10              # characters, not bytes
max_length = 68
pattern = '^(feat|fix|docs|chore)(\(.+\))?: .+'
severity = "warning"         # blocker | warning | note; defaults to warning

[review.description]
required = true
min_length = 40
max_length = 5000
pattern = '...'
required_sections = ["## Why", "## Testing"]
severity = "blocker"
```

`pattern` is a regular expression, and it is anchored only where you anchor it:
`^feat:` matches at the start, `feat` matches anywhere. Lookaround is not
supported, because the engine is guaranteed linear time so that a pattern from
a fork's configuration cannot burn the whole job.

`required_sections` matches whole lines, ignoring case and runs of whitespace,
so `##   why` satisfies `## Why`. Words mentioned inside a sentence do not.

A violation becomes an ordinary finding at the severity its rule declares, so
`block_on` alone decides whether it fails the check run. Violations are named
in the review body rather than posted as inline comments, because a title has
no diff line to anchor to.

A `pattern` that is not a valid regular expression fails the run at validation
with the field named, rather than being ignored.

## `[cache]`

The resume cache keeps completed passes so a retried run does not pay twice
for work an earlier attempt finished. It is off unless you turn it on.

```toml
[cache]
enabled = true
dir = "/tmp/demur-cache"   # where entries live
max_age_hours = 24         # entries older than this are dropped
max_mb = 256               # total size before oldest-first eviction
```

A cache can only change what a run costs. An absent, disabled, unreadable,
corrupt, or evicted cache produces exactly the review a cold run produces, so
nothing about your findings, verdict, or check run depends on it existing.

An entry is reusable only by a run that would have asked the same question of
the same model: the key covers the head commit, the pass and its lens, the
rendered prompt, the model and its parameters, and the output ceiling. A
changed model or lens is a miss, not a reuse.

A resumed run reports what it paid and what it inherited separately, and the
pull request's cumulative spend keeps counting what the earlier attempt
actually paid.

The cache is never read on a pull request from a fork. In the Action, set the
`cache` input rather than this section; the workflow supplies the directory.

## `[retrieval]`

A review pass sees the diff and little else. With retrieval on, a pass may
name repository content it needs before it can argue, and demur fetches it.

```toml
[retrieval]
enabled = true
max_rounds = 1    # times one pass may ask for more
max_kb = 64       # total retrieved content per run
```

The model never reaches anything. It returns names, written `file:<path>` or
`symbol:<name>`, and demur resolves each one itself against an allowlist. A
request shaped like a command is a name that does not match a file, so it fails
the way a typo fails.

A request resolves only inside the checkout, minus the paths you ignore. demur
refuses anything that escapes the checkout by traversal or through a symbolic
link, the version control directory, this configuration file, and the file
named by `key_file`, wherever it points.

Symbol lookup is a textual search for a definition, not a semantic index. It is
approximate on purpose, and a request it cannot answer is reported back to the
pass rather than dropped.

A retrieval round is a pass: estimated, authorized against the budget, and
recorded. A round the budget cannot fund is skipped and disclosed.

Retrieval is off unless you turn it on, and it is never performed for a pull
request from a fork.

## `[app]`

demur can act as an application you have authorized, so a review you publish
is attributed to you and carries demur's mark beside your name.

```toml
[app]
client_id = "Iv1.abc123def456"
token_file = "/home/you/.config/demur/authorization.json"
```

Paths are used literally: `~` is not expanded, so write them out in full or a
directory named `~` appears where you ran demur. The same is true of `key_file`
and `cache.dir`.

`client_id` names which application you are authorizing. It is not a secret.
`token_file` is where to keep the authorization so later publications do not
ask again; leave it out and demur authorizes afresh each time and writes
nothing. A kept authorization is readable only by you, never logged, and never
appears in a published review.

Configuring `token_file` without `client_id` fails validation: a place to keep
an authorization means nothing without an application to authorize.

This affects `demur pr --publish` only. Workflows keep publishing with the
token they already have. See the [setup guide](/guide/app).

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
