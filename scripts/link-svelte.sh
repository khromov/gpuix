#!/usr/bin/env bash
# Point `svelte` at the working copy of the Svelte repo instead of the snapshot
# `bun install` makes from a `file:` dependency.
#
# The POC exists to exercise the `custom-condition` branch, so edits to Svelte
# itself must show up here without a reinstall. Bun copies `file:` deps rather
# than symlinking them, so re-run this after every `bun install`.
set -euo pipefail

SVELTE_REPO="${SVELTE_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../../svelte/packages/svelte" && pwd)}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ ! -f "$SVELTE_REPO/package.json" ]; then
  echo "no Svelte checkout at $SVELTE_REPO (override with SVELTE_REPO=...)" >&2
  exit 1
fi

for dir in "$ROOT/node_modules" "$ROOT/packages/svelte/node_modules"; do
  mkdir -p "$dir"
  rm -rf "$dir/svelte"
  ln -sfn "$SVELTE_REPO" "$dir/svelte"
  echo "linked $dir/svelte -> $SVELTE_REPO"
done
