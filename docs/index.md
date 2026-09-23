---
layout: home

hero:
  name: demur
  text: The reviewer that argues against you
  tagline: Even granting every fact in the pull request, there is still no case for merging it.
  image:
    light: /logo.svg
    dark: /logo-dark.svg
    alt: demur
  actions:
    - theme: brand
      text: Get started
      link: /guide/setup
    - theme: alt
      text: What demur is
      link: /guide/what-demur-is
    - theme: alt
      text: Configuration
      link: /reference/configuration

features:
  - title: Bring your own key
    details: Your provider key, your endpoint, your bill. demur never holds or proxies a key, and there is no hosted service to send one to.
  - title: Prosecutor stance
    details: Every finding cites a file and line range and argues the concrete harm of merging. Praise does not exist. Silence is approval.
  - title: Spend is capped in money
    details: Budgets are denominated in dollars, not tokens. Every pass is priced before it runs, and anything the budget cut is named in the review.
  - title: Delta re-reviews
    details: A second push reviews only what it added. An unresolved finding from an earlier run never quietly disappears from the verdict.
  - title: One core, three front ends
    details: The GitHub Action, the local CLI, and a future daemon run the same pipeline over the same configuration file.
  - title: Untrusted by construction
    details: Diff content enters prompts as delimited data, model output is schema validated, and no model response can run a command or merge anything.
---

## The shape of a review

demur publishes one review event and one check run per completed run. The body
leads with the verdict, then the ranked findings, then exactly what the run
covered and what it cost.

```markdown
## demur: changes requested

Granting every fact in this pull request, there is still no case for merging it.

The token issuer writes a live credential into a log that ships to a third
party, and the migration it adds has no rollback path.

### Findings (ranked)

1. **[blocker]** `src/auth/token.rs:41-44`: the issued token is logged at info
   Merging ships live credentials to the log sink, where anyone with read
   access to logs can replay them.

2. **[warning]** `migrations/0007_add_index.sql:1-9`: no down migration
   A failed deploy cannot be rolled back without manual database surgery.

### Coverage and spend

- Full coverage: every pass ran without degradation.

### Spend

- triage spend: $0.0021
- deep dive security on src/auth/token.rs spend: $0.1087
- cross-examination spend: $0.1099
- verdict summary spend: $0.1211
- Total spend this run: $0.3418
- Cumulative spend for this pull request: $0.4106 (earlier runs: $0.0688)
```

Two minutes of setup is on the [setup page](/guide/setup). If you would rather
try it on one pull request without installing anything, start with the
[local CLI](/guide/local-cli).
