#!/usr/bin/env bash
# The full verification gate: formatting, lints, tests, and the docs build.
# The docs build fails when the configuration reference goes stale, because
# the docs freshness test enforces that every schema key is documented.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --check
cargo clippy -- -D warnings
cargo test

# The published schema is generated from the configuration types. The test
# above fails when it drifts; this says what to run when it does.
if ! cargo run --quiet -p demur -- schema | diff -q - docs/public/demur.schema.json >/dev/null; then
  echo "docs/public/demur.schema.json is stale." >&2
  echo "regenerate it: cargo run -p demur -- schema > docs/public/demur.schema.json" >&2
  exit 1
fi

if command -v bun >/dev/null 2>&1; then
  if [ ! -d docs/node_modules ]; then
    (cd docs && bun install --frozen-lockfile)
  fi
  (cd docs && bun run docs:build)
else
  echo "bun not found: skipping the docs build" >&2
fi
