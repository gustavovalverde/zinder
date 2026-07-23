#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo >&2 "usage: check-native-proto-closure.sh ROOT_CONTAINING_PROTO"
  exit 2
fi

proto_root="$1/proto"
expected="$({
  printf '%s\n' \
    'zinder/v1/explorer/explorer.proto' \
    'zinder/v1/ingest/ingest.proto' \
    'zinder/v1/ops/error.proto' \
    'zinder/v1/ops/readiness.proto' \
    'zinder/v1/ops/server_info.proto' \
    'zinder/v1/wallet/wallet.proto'
})"
actual="$({
  find "$proto_root/zinder" -type f -name '*.proto' -printf 'zinder/%P\n' | sort
})"

[[ "$actual" == "$expected" ]] || {
  echo >&2 "native proto source closure differs from the release catalog"
  diff -u <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") || true
  exit 1
}

while IFS= read -r relative_proto; do
  test -s "$proto_root/$relative_proto" || {
    echo >&2 "native proto source is missing or empty: $relative_proto"
    exit 1
  }
done <<< "$expected"
