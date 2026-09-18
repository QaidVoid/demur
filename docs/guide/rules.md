# Review rules

<p class="verdict">A repository can state what it requires of a pull request
title and description, and demur enforces it on every review at no token cost.
These rules enforce form. They cannot judge intent, and they do not claim to.</p>

## What you can require

```toml
[review.title]
required = true
min_length = 10
max_length = 68
pattern = '^(feat|fix|docs|chore)(\(.+\))?: .+'
severity = "warning"

[review.description]
required = true
min_length = 40
required_sections = ["## Why", "## Testing"]
severity = "blocker"
```

Everything is optional. A repository with no `[review]` section gets exactly the
review it got before, and nothing about the pull request is ever reported.

## Why these rules are mechanical

Every rule above is evaluated in process, before any provider call. That buys
three things a model-judged rule cannot give you:

- **They are free.** Turning rules on does not change your bill.
- **They are the same every run.** A title that passed yesterday cannot fail
  today, and a contributor can reproduce the result locally with the CLI.
- **They cannot be argued with.** No model output participates in deciding
  them, so a pull request body that says "ignore the title rules" is just text.
  That matters precisely because the body is the thing being judged, and on a
  fork it is written by someone you do not know.

The cost is real and worth stating plainly: a pattern can tell a compliant
title from a non-compliant one, and nothing more. It cannot tell a useful title
from a useless one that happens to start with `feat:`.

## Patterns

`pattern` is a regular expression, anchored only where you anchor it:

| Pattern | Matches |
| --- | --- |
| `^feat:` | titles starting with `feat:` |
| `feat` | titles containing `feat` anywhere |
| `^(feat\|fix\|docs\|chore)(\(.+\))?: .+` | conventional commits |

Lookaround is not supported. The engine is guaranteed linear time, which is
what stops a pattern in a fork's configuration from burning the whole job, and
lookaround is the feature that guarantee costs.

A pattern that does not compile fails the run at configuration validation, with
the field named. It is never silently ignored, because a rule you think is
running and is not is worse than no rule.

## Required sections

```toml
[review.description]
required_sections = ["## Why", "## Testing"]
```

A section counts as present when a line of the description is that heading,
compared ignoring case and runs of whitespace. `##   why` satisfies `## Why`,
because markdown renders them identically and reporting one as missing would
read as a bug. The heading has to be its own line: mentioning the words in a
sentence does not satisfy the rule.

## Severity, and what a violation does

A violation is an ordinary finding carrying the severity its rule declared,
defaulting to `warning`. It is deduplicated and ranked with every other
finding, and your existing `block_on` list decides whether it fails the check
run. There is no second gate and no separate conclusion to learn.

```toml
[block_on]
severities = ["blocker"]

[review.title]
pattern = '^(feat|fix): .+'
severity = "blocker"     # now a bad title fails the check
```

Violations are named in the review body rather than posted as inline comments,
because a title has no diff line to anchor to. Findings about your code are
still posted inline and threaded, exactly as before.

## Rules outlive a degraded run

Rules are evaluated before the budget is consulted. A run whose budget cannot
fund a single model pass still reports its rule violations and still fails the
check if one of them blocks. This is the one part of a review that cannot be
cut for cost.

## Clearing a violation

There are two honest ways: change the metadata, or change the rule.

A violation cannot be dismissed for one pull request the way a model finding
can. Model findings are judgment calls and dismissing one is a reviewer
disagreeing with a judgment. A rule is something this repository declared about
every pull request, and letting whoever is merging wave it away would make
declaring it pointless.

Nothing about a violation is remembered between runs. It is re-derived from the
current title and description every time, so fixing the title clears it with no
state involved.
