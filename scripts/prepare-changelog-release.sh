#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
validator="$repository_root/scripts/validate-changelog.sh"
release_identity_validator="$repository_root/scripts/validate-release-tag.sh"
changie_bin="${CHANGIE_BIN:-changie}"

if [[ $# -ne 1 ]]; then
  echo >&2 "usage: prepare-changelog-release.sh VERSION"
  exit 2
fi

release_version="${1#v}"
semver_pattern='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-([0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*))?$'
if [[ ! "$release_version" =~ $semver_pattern ]]; then
  echo >&2 "release version must be complete SemVer without build metadata"
  exit 1
fi

bash "$release_identity_validator" "v${release_version}" >/dev/null

pending_fragment="$({
  find "$repository_root/.changes/unreleased" \
    -maxdepth 1 \
    -type f \
    -name '*.yaml' \
    -print \
    -quit
} || true)"
escaped_release_version="${release_version//./\\.}"
section_heading_pattern="^## \\[${escaped_release_version}\\] - [0-9]{4}-[0-9]{2}-[0-9]{2}$"
section_count="$(grep -Ec -- "$section_heading_pattern" "$repository_root/CHANGELOG.md" || true)"

if [[ -z "$pending_fragment" ]]; then
  bash "$validator" release "v${release_version}" "$repository_root"
  echo "changelog v${release_version} is already prepared"
  exit 0
fi
if [[ "$section_count" -ne 0 ]]; then
  echo >&2 \
    "pending fragments exist but CHANGELOG.md already contains a v${release_version} section"
  exit 1
fi

next_version="$(
  cd "$repository_root"
  "$changie_bin" next auto
)"
if [[ "${next_version#v}" != "$release_version" ]]; then
  echo >&2 \
    "requested v${release_version}, but pending fragment impacts require ${next_version}"
  exit 1
fi

(
  cd "$repository_root"
  "$changie_bin" batch "v${release_version}"
  "$changie_bin" merge --include-unreleased '## [Unreleased]'
)
bash "$validator" release "v${release_version}" "$repository_root"
echo "prepared changelog v${release_version}"
