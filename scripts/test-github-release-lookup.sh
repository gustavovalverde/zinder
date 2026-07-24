#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
finder="$repository_root/scripts/find-github-release.sh"
temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/zinder-release-lookup.XXXXXX")"
trap 'rm -rf -- "$temporary_directory"' EXIT

fail() {
  echo >&2 "GitHub release lookup test failed: $*"
  exit 1
}

mkdir -p "$temporary_directory/bin"
cat > "$temporary_directory/bin/gh" <<'EOF'
#!/usr/bin/env bash
if [[ "${FAKE_GH_EXIT_CODE:-0}" -ne 0 ]]; then
  exit "$FAKE_GH_EXIT_CODE"
fi
printf '%s\n' "${FAKE_GH_RESPONSE:?}"
EOF
chmod +x "$temporary_directory/bin/gh"

release_json="$(
  PATH="$temporary_directory/bin:$PATH" \
    GITHUB_REPOSITORY=example/zinder \
    FAKE_GH_RESPONSE='[[]]' \
    "$finder" v0.5.0-rc.1
)"
[[ -z "$release_json" ]] || fail "a missing release produced JSON"

release_json="$(
  PATH="$temporary_directory/bin:$PATH" \
    GITHUB_REPOSITORY=example/zinder \
    FAKE_GH_RESPONSE='[[{"id":1,"tag_name":"v0.5.0-rc.1","draft":true,"assets":[]}]]' \
    "$finder" v0.5.0-rc.1
)"
jq -e '.id == 1 and .draft == true' <<< "$release_json" >/dev/null \
  || fail "a draft release was not returned"

release_json="$(
  PATH="$temporary_directory/bin:$PATH" \
    GITHUB_REPOSITORY=example/zinder \
    FAKE_GH_RESPONSE='[[{"id":1,"tag_name":"v0.4.0","draft":false}], [{"id":2,"tag_name":"v0.5.0-rc.1","draft":false}]]' \
    "$finder" v0.5.0-rc.1
)"
jq -e '.id == 2 and .draft == false' <<< "$release_json" >/dev/null \
  || fail "the requested published release was not returned"

if PATH="$temporary_directory/bin:$PATH" \
  GITHUB_REPOSITORY=example/zinder \
  FAKE_GH_RESPONSE='[[{"tag_name":"v0.5.0-rc.1"}, {"tag_name":"v0.5.0-rc.1"}]]' \
  "$finder" v0.5.0-rc.1 >/dev/null 2>&1; then
  fail "duplicate releases were accepted"
fi

if PATH="$temporary_directory/bin:$PATH" \
  GITHUB_REPOSITORY=example/zinder \
  FAKE_GH_EXIT_CODE=17 \
  "$finder" v0.5.0-rc.1 >/dev/null 2>&1; then
  fail "a GitHub API failure was treated as a missing release"
fi

release_workflow="$repository_root/.github/workflows/release.yml"
lookup_count="$(
  grep -Fc 'release_json="$(scripts/find-github-release.sh "$RELEASE_TAG")"' \
    "$release_workflow"
)"
[[ "$lookup_count" -eq 3 ]] \
  || fail "release workflow must use the shared lookup in all three stages"
if grep -Fq '/releases/tags/' "$release_workflow"; then
  fail "release workflow still uses the endpoint that hides draft releases"
fi
