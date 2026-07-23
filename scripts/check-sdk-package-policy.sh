#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
metadata="$(cargo metadata --format-version 1 --no-deps --manifest-path "$repository_root/Cargo.toml")"
dependency_requirement_validator="$repository_root/scripts/check-sdk-dependency-requirement.sh"
workspace_rust_version="$(
  sed -n 's/^rust-version = "\([^"]*\)"$/\1/p' "$repository_root/Cargo.toml" \
    | head -n 1
)"

fail() {
  echo >&2 "SDK package policy rejected: $*"
  exit 1
}

expected_catalog='["zinder-client","zinder-core","zinder-proto"]'
[[ -n "$workspace_rust_version" ]] \
  || fail "workspace rust-version is missing"
workspace_version="$(
  jq -r '.packages[] | select(.name == "zinder-core") | .version' <<< "$metadata"
)"
actual_catalog="$({
  jq -c '
    [
      .packages[]
      | select(.publish == ["crates-io"])
      | .name
    ]
    | sort
  ' <<< "$metadata"
})"
[[ "$actual_catalog" == "$expected_catalog" ]] \
  || fail "publishable catalog is $actual_catalog, expected $expected_catalog"

unexpected_publishers="$({
  jq -r --argjson expected "$expected_catalog" '
    [
      .packages[]
      | select((.name as $name | $expected | index($name)) | not)
      | select(.publish != [])
      | .name
    ]
    | join(", ")
  ' <<< "$metadata"
})"
[[ -z "$unexpected_publishers" ]] \
  || fail "non-SDK crates are publishable: $unexpected_publishers"

for package_name in zinder-core zinder-proto zinder-client; do
  rust_version="$(jq -r --arg name "$package_name" '.packages[] | select(.name == $name) | .rust_version' <<< "$metadata")"
  [[ "$rust_version" == "$workspace_rust_version" ]] \
    || fail "$package_name rust-version is $rust_version, expected workspace $workspace_rust_version"

  manifest_path="$(jq -r --arg name "$package_name" '.packages[] | select(.name == $name) | .manifest_path' <<< "$metadata")"
  grep -Fq 'readme = "README.md"' "$manifest_path" \
    || fail "$package_name must use a crate-local README.md"
  grep -Fq 'include = [' "$manifest_path" \
    || fail "$package_name must declare an explicit package include list"
done

require_public_edge() {
  local package_name="$1"
  local dependency_name="$2"
  local edge
  edge="$({
    jq -c \
      --arg package_name "$package_name" \
      --arg dependency_name "$dependency_name" '
        [
          .packages[]
          | select(.name == $package_name)
          | .dependencies[]
          | select(.name == $dependency_name and .kind == null)
          | { path, req }
        ]
      ' <<< "$metadata"
  })"
  [[ "$(jq 'length' <<< "$edge")" == 1 ]] \
    || fail "$package_name -> $dependency_name must have exactly one normal dependency edge"
  [[ "$(jq -r '.[0].path' <<< "$edge")" != null ]] \
    || fail "$package_name -> $dependency_name must retain a workspace path"
  bash "$dependency_requirement_validator" \
    "$workspace_version" \
    "$package_name" \
    "$dependency_name" \
    "$(jq -r '.[0].req' <<< "$edge")" \
    || fail "$package_name -> $dependency_name does not match the product version"
}

require_public_edge zinder-proto zinder-core
require_public_edge zinder-client zinder-core
require_public_edge zinder-client zinder-proto
