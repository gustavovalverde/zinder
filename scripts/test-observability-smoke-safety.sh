#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
smoke_script="$repository_root/scripts/observability-smoke.sh"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/zinder-observability-safety.XXXXXX")"
trap 'rm -rf -- "$fixture_root"' EXIT

fail() {
  echo >&2 "observability smoke safety test failed: $*"
  exit 1
}

prepare_fixture() {
  local work_dir="$1"
  ZINDER_OBSERVABILITY_WORK_DIR="$work_dir" \
    bash -c 'source "$1"; prepare_work_dir' _ "$smoke_script"
}

mkdir -p "$fixture_root/nonempty"
touch "$fixture_root/nonempty/operator-sentinel"
if prepare_fixture "$fixture_root/nonempty" >/dev/null 2>&1; then
  fail "an unmarked non-empty work directory was accepted"
fi
[[ -f "$fixture_root/nonempty/operator-sentinel" ]] ||
  fail "the rejected work directory was modified"

mkdir -p "$fixture_root/empty"
prepare_fixture "$fixture_root/empty"
[[ -f "$fixture_root/empty/.zinder-observability-smoke-workdir" ]] ||
  fail "an accepted work directory was not marked"

mkdir -p "$fixture_root/empty/wallet"
touch "$fixture_root/empty/operator-sentinel"
prepare_fixture "$fixture_root/empty"
[[ ! -e "$fixture_root/empty/wallet" ]] ||
  fail "a harness-owned reset target survived reset"
[[ -f "$fixture_root/empty/operator-sentinel" ]] ||
  fail "reset removed an unrelated marked-directory entry"

ln -s "$fixture_root/empty" "$fixture_root/symlink"
if prepare_fixture "$fixture_root/symlink" >/dev/null 2>&1; then
  fail "a symbolic-link work directory was accepted"
fi

if prepare_fixture / >/dev/null 2>&1; then
  fail "the filesystem root was accepted as a work directory"
fi

echo "observability smoke safety tests passed"
