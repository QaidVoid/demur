# Local CLI

The `demur` CLI runs the same pipeline, profiles, lenses, finding contract,
severity taxonomy, and budget machinery as the GitHub Action, reading the
same `.demur.toml` from the repository root. It is the fastest loop for
tuning profiles, lenses, and prompts, because a local review never pushes
to a pull request.

## Review local changes

```bash
# The working copy, including uncommitted changes
demur review

# A revision range
demur review main..HEAD
```

History is read through jj when the repository is jj-based and through git
otherwise. Both produce the same review for the same diff. No GitHub API
call is made for a local review.

## Review a pull request

```bash
demur pr 42
demur pr https://github.com/owner/repo/pull/42
```

The target may be a pull request number inside a repository checkout or a
full URL. The GitHub token comes from `GITHUB_TOKEN`, then `GH_TOKEN`, and
finally from the GitHub CLI when it is authenticated (`gh auth login`). The
pull request diff is fetched and reviewed in full: the CLI keeps no state,
so it never reads or writes the bot's review markers.

## Output formats and exit status

`--format markdown` (the default) prints the review ranked exactly as the
pipeline ranked it. `--format json` prints one machine-readable document
carrying the findings, severities, locations, verdict, and spend.

The exit status encodes the verdict so the CLI can gate a local workflow:

| Status | Meaning |
| ------ | ------- |
| 0 | APPROVE |
| 1 | REQUEST_CHANGES |
| 2 | The run failed to complete |

A failed run never reports as a clean review.

## Publishing

The CLI never writes to GitHub by default. `demur pr --publish` posts the
review, and it says so first: the findings will appear under the identity
that owns the token, not under a bot identity. A locally published review
carries no bot state, so it does not affect delta reviews.

## Budgets apply locally

Budget estimation, the cap, and the degradation ladder apply to local runs
exactly as they do to Action runs, and the printed review reports spend and
any degradation that applied.
