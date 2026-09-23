# CLI reference

The `demur` binary reviews a local revision range or a GitHub pull request with
the same pipeline the Action runs, reading the same `.demur.toml` from the
repository root. It is read-only unless you explicitly ask it to publish.

## `demur review [RANGE]`

Review local changes. With no range, the working copy is reviewed against its
parent.

```bash
demur review                 # working copy
demur review main..HEAD      # a revision range
demur review --format json   # machine-readable output
```

| Option | Default | Meaning |
| --- | --- | --- |
| `RANGE` | working copy | Revision range written `FROM..TO`. |
| `--repo <PATH>` | current directory | Repository root to review. |
| `--format <FMT>` | `markdown` | `markdown` or `json`. |
| `--cache-dir <PATH>` | off | Reuse completed passes from this directory when a run is retried. Nothing is cached unless it names one. |

History is read through jj when the repository has a `.jj` directory, and
through git otherwise. The commit messages in the range stand in for a pull
request description, so the reviewer sees what the change claims to do.

No GitHub call is made and no state file is written. A local review always
covers its full input; incremental scope comes only from the range you name.

## `demur review-pr <TARGET>`

Review a GitHub pull request. `demur pr` is a visible alias.

```bash
demur pr 128
demur pr https://github.com/owner/repo/pull/128
demur pr 128 --publish
```

| Option | Default | Meaning |
| --- | --- | --- |
| `TARGET` | required | Pull request number or full URL. |
| `--repo <PATH>` | current directory | Repository root, used to resolve the remote and load configuration. |
| `--format <FMT>` | `markdown` | `markdown` or `json`. |
| `--publish` | off | Post the review to GitHub under your own identity. |
| `--cache-dir <PATH>` | off | Reuse completed passes from this directory when a run is retried. With `--publish`, it shares the Action's cache layout. |

Without `--publish` nothing is written to GitHub. With it, the review is posted
under the identity owning the token you supplied, not under a bot account, and
the output says so before it posts.

The GitHub token is read from the environment, falling back to the `gh` CLI's
credentials when one is available.

## Exit status

| Status | Meaning |
| --- | --- |
| `0` | APPROVE. No finding at a blocking severity survived. |
| `1` | REQUEST_CHANGES. At least one blocking finding stands. |
| `2` | The run failed to complete. |

A failed run is never reported as a clean review, which is what makes the exit
status safe to use as a local gate:

```bash
demur review main..HEAD || echo "demur objects"
```

## Logging

Progress goes to stderr, so `--format json` on stdout stays pipeable. Verbosity
follows `RUST_LOG`:

```bash
RUST_LOG=debug demur review main..HEAD
RUST_LOG=warn demur review main..HEAD --format json > review.json
```
