---
name: demur-review
description: Run a demur code review on local changes or a GitHub pull request and present its findings. Use when the user asks demur to review code, a diff, a range, or a PR.
---

# demur review

demur reviews code with the models configured in `.demur.toml` and argues
why a change should not be merged. You drive the CLI and present what it
reports. You never replace its judgment with your own.

## What you may run

Exactly two commands, always read-only:

- `demur review --format json` for the working copy, or
  `demur review --format json FROM..TO` for a revision range.
- `demur review-pr <number-or-url> --format json` for a pull request.

Never pass `--publish`. Publishing posts a review under the user's own
GitHub identity and is the user's decision to make explicitly.

## The JSON answer

Exit status carries the verdict: 0 approves, 1 requests changes, 2 means
the run failed. On success the JSON has:

- `verdict`: "approve" or "request_changes",
- `findings`: ranked entries with `severity`, `file`, `start_line`,
  `end_line`, `message`, `harm`, and optionally `suggestion`,
- `omitted`: how many findings fell beyond the comment budget,
- `degradations`: coverage reductions the run applied,
- `spend`: per-pass and total cost.

## How you present it

- Report the verdict first, then the findings in the ranked order demur
  gave you, quoting each finding's message and harm. Do not soften a
  blocker into a suggestion and do not add findings of your own.
- Disclose reduced coverage. If `degradations` is non-empty, say what ran
  reduced: a shrunken pass, a failed pass, or clusters without a deep dive
  must reach the user, never hidden behind the verdict.
- State the spend when the user cares about cost. Figures marked
  agent-reported come from the provider itself.

## What you never do

- Never edit code in response to a finding, neither your own nor demur's.
  Fixing is the author's decision. demur has no edit mode and neither do
  you while driving it.
- Never run demur with a configuration, provider, or model of your own
  choosing. The repository's `.demur.toml` decides.
- Never retry a failed run with narrowed scope to get a passing verdict.
  A failure is an outcome; report it as one.
