#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$repository_root"

buf lint
buf generate
find dist/openapi -type f -name '*.yaml' -size +0c -print -quit \
  | grep -q . || {
    echo "OpenAPI generation produced no non-empty YAML artifact" >&2
    exit 1
  }
buf build --output dist/zinder.v1.descriptor.bin
test -s dist/zinder.v1.descriptor.bin

scripts/check-native-proto-closure.sh crates/zinder-proto
mkdir -p .tmp
staging_root="$(mktemp -d .tmp/zinder-native-proto.XXXXXX)"
trap 'rm -rf "$staging_root"' EXIT
mkdir -p "$staging_root/proto"
cp -R crates/zinder-proto/proto/zinder "$staging_root/proto/zinder"
next_proto_dir="dist/.proto.next.$$"
previous_proto_dir="dist/.proto.previous.$$"
mv "$staging_root/proto" "$next_proto_dir"
if [[ -e dist/proto ]]; then
  mv dist/proto "$previous_proto_dir"
fi
mv "$next_proto_dir" dist/proto
rm -rf "$previous_proto_dir"
scripts/check-native-proto-closure.sh dist
