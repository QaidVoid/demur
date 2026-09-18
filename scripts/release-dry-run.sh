#!/usr/bin/env bash
# Dry run of the release packaging for the host target: builds both
# binaries in release mode and produces the tarballs the release workflow
# attaches, under the exact names the composite action downloads.
set -euo pipefail
cd "$(dirname "$0")/.."

target="$(rustc -vV | awk '/^host:/ {print $2}')"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
out="demur-v${version}-${target}"

echo "building for host target $target"
cargo build --release -p demur -p demur-action

mkdir -p "$out"
cp target/release/demur "$out/"
cp target/release/demur-action "$out/"
tar -czf "$out.tar.gz" "$out"
sha256sum "$out.tar.gz" > "$out.tar.gz.sha256"

for binary in demur demur-action; do
  test -x "$out/$binary"
done

# The stable schema describes the current release; the versioned copy is an
# immutable snapshot, so a pinned binary can be matched by a pinned schema.
schema_dir="docs/public/schema/v${version}"
mkdir -p "$schema_dir"
"$out/demur" schema > "$schema_dir/demur.schema.json"
diff -q "$schema_dir/demur.schema.json" docs/public/demur.schema.json >/dev/null

echo "artifacts:"
ls -l "$out.tar.gz" "$out.tar.gz.sha256"
ls -l docs/public/demur.schema.json "$schema_dir/demur.schema.json"
