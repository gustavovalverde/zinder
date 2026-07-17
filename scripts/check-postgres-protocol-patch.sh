#!/usr/bin/env bash
set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
repository_root="$(dirname -- "$script_dir")"
cd "$repository_root"

if ! shasum -a 256 --check vendor/postgres-protocol/ZINDER-SHA256SUMS; then
  echo >&2 'postgres-protocol compatibility patch differs from its reviewed source manifest'
  echo >&2 'inspect the vendor diff and update ZINDER-SHA256SUMS only after review'
  exit 1
fi

workspace_tree="$(cargo tree --workspace --locked --prefix none)"
if ! grep -Fq 'bip32 v0.6.0-pre.1' <<<"$workspace_tree"; then
  echo >&2 'postgres-protocol compatibility patch is no longer justified: bip32 0.6.0-pre.1 left the workspace graph'
  echo >&2 'remove vendor/postgres-protocol and the root [patch.crates-io] entry, then test the upstream protocol crate unchanged'
  exit 1
fi

benchmark_tree="$(cargo tree -p zinder-bench --locked)"
if ! grep -Fq 'tokio-postgres v0.7.18' <<<"$benchmark_tree"; then
  echo >&2 'zinder-bench must resolve the reviewed tokio-postgres 0.7.18 driver'
  exit 1
fi
if ! grep -Fq 'postgres-protocol v0.6.12+zinder.1 (' <<<"$benchmark_tree"; then
  echo >&2 'tokio-postgres must resolve the reviewed local postgres-protocol 0.6.12+zinder.1 patch'
  exit 1
fi
if grep -Fq 'sqlx' <<<"$benchmark_tree"; then
  echo >&2 'zinder-bench must use tokio-postgres directly; SQLx is not part of this driver contract'
  exit 1
fi
