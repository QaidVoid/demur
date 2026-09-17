#!/usr/bin/env bash
# The full verification gate: formatting, lints, tests, and the docs build.
# The docs build fails when the configuration reference goes stale, because
# the docs freshness test enforces that every schema key is documented.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --check
cargo clippy -- -D warnings
cargo test

if command -v npm >/dev/null 2>&1; then
  if [ ! -d docs/node_modules ]; then
    (cd docs && npm install --no-audit --no-fund)
  fi
  (cd docs && npm run docs:build)
else
  echo "npm not found: skipping the docs build" >&2
fi
