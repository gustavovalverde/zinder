#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo >&2 "usage: extract-release-notes.sh VERSION [CHANGELOG]"
  exit 2
fi

release_version="${1#v}"
changelog_path="${2:-$repository_root/CHANGELOG.md}"
semver_pattern='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-([0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*))?$'
if [[ ! "$release_version" =~ $semver_pattern ]]; then
  echo >&2 "release-note version must be complete SemVer without build metadata"
  exit 1
fi
escaped_release_version="${release_version//./\\.}"
section_heading_pattern="^## \\[${escaped_release_version}\\] - [0-9]{4}-[0-9]{2}-[0-9]{2}$"
section_count="$(grep -Ec -- "$section_heading_pattern" "$changelog_path" || true)"
if [[ "$section_count" -ne 1 ]]; then
  echo >&2 \
    "release-note extraction requires exactly one '## [${release_version}] - YYYY-MM-DD' section"
  exit 1
fi
section_heading="$(grep -E -- "$section_heading_pattern" "$changelog_path" | head -n 1)"

awk -v section_heading="$section_heading" '
  BEGIN {
    count = 0
  }
  $0 == section_heading {
    printing = 1
  }
  printing && $0 != section_heading && /^## / {
    exit
  }
  printing {
    lines[count] = $0
    count++
  }
  END {
    while (count > 0 && lines[count - 1] == "") {
      count--
    }
    for (line_number = 0; line_number < count; line_number++) {
      print lines[line_number]
    }
  }
' "$changelog_path"
