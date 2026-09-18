# Editor schema

demur publishes a JSON Schema for `.demur.toml`, so an editor can complete keys
and flag a typo while you type rather than on your next run.

## Pointing a file at it

Most TOML editor tooling reads a `#:schema` directive on the first line:

```toml
#:schema https://demur.dev/demur.schema.json

profile = "standard"

[review.title]
pattern = '^(feat|fix): .+'
```

That URL always describes the current release. Write it once and the file keeps
validating against demur as demur changes.

## Pinning

If you pin a release, pin the schema with it. Every release publishes an
immutable copy at its own version:

```toml
#:schema https://demur.dev/schema/v0.1.0/demur.schema.json
```

A versioned copy is never rewritten, so a pinned binary and a pinned schema
always agree. The stable URL is the right default for everyone else, because a
versioned URL written into a file once is a URL nobody goes back to update, and
it would quietly stop describing the tool.

## Generating it yourself

The binary prints the same schema it was built with:

```bash
demur schema > demur.schema.json
```

This is the schema for the exact binary you are running, which makes it the
right one to use when running an unreleased build.

## What it covers

Every configuration key, its type, the closed sets of values where one exists,
and the defaults. It also rejects unknown keys, exactly as demur's own
validation does, so a misspelled key is flagged in the editor rather than
accepted there and rejected at run time.

What it cannot cover are the cross-field rules: that `per_pr_usd` and
`unlimited` are mutually exclusive, that `reasoning_effort` belongs only to the
openai family, or that a `pattern` has to compile. Those still fail at
validation with the field named. The guarantee is only that the schema never
contradicts the validator, not that it replaces it.

The schema is generated from the configuration types themselves and the
verification gate fails if the published copy drifts from them, so it cannot
silently describe a shape demur would reject.
