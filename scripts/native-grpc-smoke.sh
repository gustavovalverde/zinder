#!/usr/bin/env bash
#
# Smoke-test the native `zinder-query` gRPC surface against an already-running
# query process. Validates the baseline capability descriptor for a standalone
# query process and exercises the read-path RPCs that the wire surface most
# depends on (`BlockIdBySelector`, `BlockHeaderBySelector`, `LatestBlock`,
# `Transaction`).
#
# Usage:
#   scripts/native-grpc-smoke.sh [<query-addr>]
#
# Defaults: query-addr=127.0.0.1:9069. Override per env:
#   ZINDER_QUERY_GRPC_ADDR                     (host:port for the native gRPC endpoint)
#   ZINDER_QUERY_HEIGHT                        (block height to probe; default latest visible block)
#
# Exit codes:
#   0  all probes passed
#   1  a probe failed (capability drift, bad response, or unreachable endpoint)
#   2  prerequisite missing (grpcurl, jq, query endpoint not bound)

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROTO_DIR="${ROOT_DIR}/crates/zinder-proto/proto"
WALLET_PROTO="${PROTO_DIR}/zinder/v1/wallet/wallet.proto"
CAPABILITIES_RS="${ROOT_DIR}/crates/zinder-proto/src/capabilities.rs"

QUERY_ADDR="${1:-${ZINDER_QUERY_GRPC_ADDR:-127.0.0.1:9069}}"
HEIGHT="${ZINDER_QUERY_HEIGHT:-}"

log() {
  printf '[native-grpc-smoke] %s\n' "$*"
}

die() {
  printf '[native-grpc-smoke] error: %s\n' "$*" >&2
  exit "${2:-1}"
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1" 2
}

grpc_call() {
  local method="$1"
  local payload="${2-}"
  if [[ -z "${payload}" ]]; then
    payload='{}'
  fi
  grpcurl -plaintext \
    -import-path "${PROTO_DIR}" \
    -proto "${WALLET_PROTO}" \
    -d "${payload}" \
    "${QUERY_ADDR}" "${method}"
}

require_command grpcurl
require_command jq
[[ -f "${WALLET_PROTO}" ]] || die "wallet.proto not found at ${WALLET_PROTO}" 2
[[ -f "${CAPABILITIES_RS}" ]] || die "capabilities.rs not found at ${CAPABILITIES_RS}" 2

log "endpoint=${QUERY_ADDR} height=${HEIGHT:-latest}"

# 1. Capability descriptor parity with the standalone WalletQuery baseline.
log "probe ServerInfo"
server_info=$(grpc_call "zinder.v1.wallet.WalletQuery/ServerInfo" || die "ServerInfo unreachable")

advertised=$(jq -r '.capabilities.capabilities[]?' <<<"${server_info}" | sort)
expected=$(grep -E '^[[:space:]]*"[a-z][^"]+",' "${CAPABILITIES_RS}" \
  | sed -E 's/^[[:space:]]*"([^"]+)".*$/\1/' \
  | sort)

if [[ -z "${expected}" ]]; then
  die "could not parse expected capability list from ${CAPABILITIES_RS}"
fi

missing=$(comm -23 <(echo "${expected}") <(echo "${advertised}") || true)
extra=$(comm -13 <(echo "${expected}") <(echo "${advertised}") || true)

if [[ -n "${missing}" || -n "${extra}" ]]; then
  printf '[native-grpc-smoke] capability drift detected:\n' >&2
  [[ -n "${missing}" ]] && printf '  missing from server: %s\n' "${missing}" >&2
  [[ -n "${extra}" ]] && printf '  extra on server:     %s\n' "${extra}" >&2
  exit 1
fi
log "ServerInfo capabilities match standalone WalletQuery baseline ($(echo "${expected}" | wc -l | tr -d ' ') entries)"

# 2. LatestBlock — round-trip the visible chain epoch.
log "probe LatestBlock"
latest=$(grpc_call "zinder.v1.wallet.WalletQuery/LatestBlock" || die "LatestBlock failed")
latest_height=$(jq -r '.latestBlock.height // empty' <<<"${latest}")
[[ -n "${latest_height}" ]] || die "LatestBlock did not return a height: ${latest}"
log "LatestBlock.height=${latest_height}"
if [[ -z "${HEIGHT}" ]]; then
  HEIGHT="${latest_height}"
  log "probe height defaulted to LatestBlock.height=${HEIGHT}"
fi

# 3. BlockIdBySelector by height.
log "probe BlockIdBySelector (height=${HEIGHT})"
block_id=$(grpc_call "zinder.v1.wallet.WalletQuery/BlockIdBySelector" \
  "{\"selector\":{\"height\":${HEIGHT}}}" \
  || die "BlockIdBySelector failed")
resolved_height=$(jq -r '.blockId.height // empty' <<<"${block_id}")
resolved_hash=$(jq -r '.blockId.blockHash // empty' <<<"${block_id}")
[[ "${resolved_height}" == "${HEIGHT}" ]] \
  || die "BlockIdBySelector returned height=${resolved_height}, expected ${HEIGHT}"
[[ -n "${resolved_hash}" ]] \
  || die "BlockIdBySelector did not return a block hash"
log "BlockIdBySelector resolved height=${resolved_height} hash=${resolved_hash:0:16}..."

# 4. BlockIdBySelector by hash — confirms the typed BlockSelector hash arm and
#    the block_hash_index column family are wired end-to-end.
log "probe BlockIdBySelector (hash from previous response)"
hash_lookup=$(grpc_call "zinder.v1.wallet.WalletQuery/BlockIdBySelector" \
  "{\"selector\":{\"hash\":\"${resolved_hash}\"}}" \
  || die "BlockIdBySelector by hash failed")
hash_lookup_height=$(jq -r '.blockId.height // empty' <<<"${hash_lookup}")
[[ "${hash_lookup_height}" == "${HEIGHT}" ]] \
  || die "BlockIdBySelector by hash returned height=${hash_lookup_height}, expected ${HEIGHT}"
log "BlockIdBySelector by hash matched height=${hash_lookup_height}"

# 5. BlockHeaderBySelector — exercises the typed BlockHeaderInfo shape.
log "probe BlockHeaderBySelector (height=${HEIGHT})"
header=$(grpc_call "zinder.v1.wallet.WalletQuery/BlockHeaderBySelector" \
  "{\"selector\":{\"height\":${HEIGHT}}}" \
  || die "BlockHeaderBySelector failed")
header_height=$(jq -r '.blockHeader.blockId.height // empty' <<<"${header}")
[[ "${header_height}" == "${HEIGHT}" ]] \
  || die "BlockHeaderBySelector returned height=${header_height}, expected ${HEIGHT}"
log "BlockHeaderBySelector returned typed header at height=${header_height}"

# 6. Transaction NotFound mapping — confirm the wire returns plain NOT_FOUND
#    (Code = 5) for an unknown txid, with a "not visible" message.
log "probe Transaction (unknown txid expects NOT_FOUND)"
fake_txid=$(printf 'A%.0s' {1..32} | base64)
not_found=$(grpcurl -plaintext \
  -import-path "${PROTO_DIR}" \
  -proto "${WALLET_PROTO}" \
  -d "{\"transactionId\":\"${fake_txid}\"}" \
  "${QUERY_ADDR}" zinder.v1.wallet.WalletQuery/Transaction 2>&1 \
  || true)
if grep -q 'Code: NotFound' <<<"${not_found}" \
  && grep -qi 'not visible' <<<"${not_found}"; then
  log "Transaction NotFound mapping returns NOT_FOUND with the documented message"
else
  printf '[native-grpc-smoke] unexpected Transaction response for unknown txid:\n%s\n' \
    "${not_found}" >&2
  exit 1
fi

log "native gRPC smoke passed"
