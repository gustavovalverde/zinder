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
