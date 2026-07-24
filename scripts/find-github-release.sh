#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo >&2 "usage: find-github-release.sh v<semver>"
  exit 2
fi

release_tag="$1"
[[ "$release_tag" =~ ^v[0-9A-Za-z.-]+$ ]] || {
  echo >&2 "GitHub release tag contains unsupported characters"
  exit 1
}
[[ -n "${GITHUB_REPOSITORY:-}" ]] || {
  echo >&2 "GITHUB_REPOSITORY must name the release repository"
  exit 1
}

gh api \
  "repos/${GITHUB_REPOSITORY}/releases?per_page=100" \
  --paginate \
  --slurp \
  | jq -c --arg release_tag "$release_tag" '
      [.[][] | select(.tag_name == $release_tag)]
      | if length > 1 then
          error("multiple GitHub releases use the requested tag")
        elif length == 1 then
          .[0]
        else
          empty
        end
    '
