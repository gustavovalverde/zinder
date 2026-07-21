#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
validator="$repository_root/scripts/validate-release-tag.sh"
workspace_version="$(
  cargo metadata \
    --format-version 1 \
    --no-deps \
    --manifest-path "$repository_root/Cargo.toml" \
    | jq -r '
        .workspace_members as $workspace_members
        | [
            .packages[]
            | select(.id as $id | $workspace_members | index($id))
            | select((.manifest_path | contains("/vendor/")) | not)
            | .version
          ]
        | unique
        | if length == 1 then .[0] else error("workspace versions diverged") end
      '
)"

fail() {
  echo >&2 "release tag test failed: $*"
  exit 1
}

expect_rejected() {
  local candidate="$1"
  if bash "$validator" "$candidate" >/dev/null 2>&1; then
    fail "$candidate was admitted"
  fi
}

validation_output="$(bash "$validator" "v${workspace_version}")"
grep -Fqx "version=${workspace_version}" <<< "$validation_output" \
  || fail "validator did not emit the workspace version"

expected_stable=true
[[ "$workspace_version" != *-* ]] || expected_stable=false
grep -Fqx "stable=${expected_stable}" <<< "$validation_output" \
  || fail "validator emitted the wrong stable-release classification"

base_version="${workspace_version%%-*}"
IFS=. read -r major minor patch <<< "$base_version"
wrong_patch_version="${major}.${minor}.$((patch + 1))"

expect_rejected "$workspace_version"
expect_rejected "v${wrong_patch_version}"
expect_rejected "v${base_version}+build.1"
expect_rejected "v01.2.3"
expect_rejected "v1.02.3"
expect_rejected "v1.2.03"
expect_rejected "v1.2.3-01"
expect_rejected "v1.2"
expect_rejected "vlatest"
