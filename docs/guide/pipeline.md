# The review pipeline

<p class="verdict">A review is five ordered passes. Each one is priced before it
runs, and the verdict is computed mechanically at the end rather than asked of
a model.</p>

## The passes

### 1. Triage

One call on the triage model over the whole diff. It ranks the changed hunks by
risk, assigns review lenses per file cluster, and may already report findings it
can anchor. Content your configuration ignores never reaches this pass, and
neither do generated files or whitespace-only churn.

### 2. Deep dives

One call per file cluster per active lens, on the deep model. A cluster that
triage assigned `security` and `correctness` costs two calls. This is the pass
that dominates the bill, which is why two separate dials bound it: the
`limits.deep_calls` ceiling bounds the call count, and the budget bounds the
money. Whichever binds first is disclosed in the review.

### 3. Cross-examination

Deep profile only. One call over the whole change that tries to defeat it as a
whole rather than file by file: adversarial inputs, rollback safety, concurrency
hazards, migration safety, and breaking interface changes. Findings from it are
held to the same contract as every other pass.

### 4. Verdict synthesis

Findings are deduplicated and force ranked by the harm they argue. Anything
missing a location or arguing no concrete harm is dropped here. Findings that
cite the same location are then reconciled into one: a line yields one comment
carrying every concern raised about it, and nothing is dropped in the merge.
The comment budget bounds locations to read, not arguments made, and the body
states how many locations were omitted when the budget binds.

The verdict itself is not a model output. It is computed from the severities
that survive synthesis against your `block_on` list. A model cannot approve a
pull request by saying so, and prompt injection cannot change the verdict
because nothing reads a verdict out of model text.

The verdict model is asked for one thing only: a two sentence summary of the
strongest case against merging. If that call fails, the review publishes
without the paragraph. It never costs you the findings the deep dives paid for.

### 5. Publication

One review event with inline comments and suggestion blocks where a fix can be
expressed, one comment per location, plus one check run whose conclusion is the
verdict. Publication reports the verdict and never recomputes it, so the review
and the check can never disagree.

Publication is atomic. Nothing is posted until synthesis completes, so a
cancelled job costs tokens but never a partial verdict.

## Profiles

| Profile | Triage | Deep dives | Cross-examination |
| --- | --- | --- | --- |
| `quick` | yes | no | no |
| `standard` (default) | yes | yes | no |
| `deep` | yes | yes | yes |

## The finding contract

Every published finding carries:

- a file path and a line range that exist in the diff,
- exactly one severity: `blocker`, `warning`, or `note`,
- the concrete harm merging would cause,
- optionally a suggestion, rendered as a GitHub suggestion block.

Findings that fail any of the first three are dropped at synthesis rather than
published with a hedge. This is deliberate: a review padded with unanchored
concerns trains people to skim it.

## Lenses

| Lens | What it argues |
| --- | --- |
| `correctness` | The change does not do what it claims, or breaks something that worked. |
| `security` | The change exposes data, credentials, or an attack surface. |
| `performance` | The change costs time, memory, or queries that matter at real volume. |
| `style` | Off by default. Naming and form, only when you ask for it. |

Lenses multiply deep dive calls. Enabling all four roughly quadruples the deep
pass cost of a wide pull request, which is what `limits.deep_calls` is for.
