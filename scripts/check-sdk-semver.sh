#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo >&2 "usage: check-sdk-semver.sh BASELINE_REVISION"
  exit 2
fi

baseline_revision="$1"
git rev-parse --verify "${baseline_revision}^{commit}" >/dev/null

# The SDK has no crates.io baseline yet. The registry-candidate revision is an
# explicit non-registry bootstrap baseline: cargo-semver-checks must be able to
# model each packaged public API, but there is no published predecessor to
# compare it with. After the first publication this gate must switch to
# cargo-semver-checks' registry baseline mode.
for package_name in zinder-core zinder-proto zinder-client; do
  cargo semver-checks check-release \
    --package "$package_name" \
    --baseline-rev "$baseline_revision" \
    --release-type major
done
