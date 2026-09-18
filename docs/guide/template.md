# Shaping the review body

<p class="verdict">A repository decides the order of a review and the prose
around it. What it does not decide is whether the review admits what it did
not cover.</p>

## The default

With no configuration, a review renders in this order:

```
verdict, summary, findings, omitted, beyond_budget, coverage, spend
```

## Changing it

```toml
[review.template]
header = """
### Automated review
Run by the platform team's demur configuration.
"""
sections = [
    "verdict", "findings", "omitted", "beyond_budget",
    "coverage", "spend", "models",
]
footer = "Disagree with a finding? Resolve the thread and say why."
```

`header` and `footer` are rendered exactly as written. **Nothing is
substituted** — a `$HEAD` or a `{verdict}` in your prose is published as those
characters.

That is deliberate. Interpolation does not stay small: first the head commit,
then the verdict so the opening can vary, then a condition so it varies only
sometimes, and a configuration field has become a language with a
specification. Everything dynamic in a review is already a section, so a
repository that wants the commit in its opening line is really telling us a
section is missing, which is better evidence than a guess.

## The sections

| Section | What it renders |
| --- | --- |
| `verdict` | The verdict heading and the stance sentence. |
| `summary` | The paragraph the verdict model drafted. |
| `findings` | The ranked findings. **Required.** |
| `omitted` | How many findings the comment budget cut. **Required.** |
| `beyond_budget` | Verdict-setting findings the budget cut. **Required.** |
| `coverage` | What ran, and every degradation applied. **Required.** |
| `spend` | What each pass cost, and the totals. **Required.** |
| `models` | Which model each pass actually used. |

A section with nothing to say renders nothing. A run that omitted no findings
shows no omitted heading rather than an empty one.

## Why five are required

Those five are how a review admits what it did not do. If a template could drop
`coverage`, a run that skipped half the files for budget could publish a review
that looks complete, with a green check run behind it. That is the exact thing
[cost controls](/guide/cost) exist to prevent, and a formatting option is not a
good enough reason to open a hole in it.

So they are movable, not optional. Put `spend` at the very bottom, put
`coverage` next to the findings, but a template that leaves one out fails the
run naming it:

```
review.template.sections: omits `coverage`, which a review cannot be
published without because it states what the run did not cover; reorder
them anywhere, but they must be present
```

Naming a section that does not exist, or the same section twice, fails the
same way. A review that reports its spend twice is a mistake rather than a
preference.

## Which model did what

The `models` section is not in the default body and is worth adding:

```markdown
### Models

- gpt-4o: deep dive security, deep dive correctness
- gpt-4o-mini: triage, verdict summary
```

Passes are grouped by the model that actually ran them. If the budget
downgraded a deep dive to the cheaper model, the model named is the one it
used, not the one you configured — a run that quietly dropped to a smaller
model is exactly the run whose findings deserve a second look.

Without this, the spend report tells you a deep dive cost eleven cents and
leaves you to guess whether that was a large model on a small diff or the
reverse.

## What a template cannot change

The verdict, the check run conclusion, which findings are published, their
ranking, and the inline comments anchored to the diff. The same pull request
under two templates produces the same review, arranged differently. There is a
test that asserts exactly that.
