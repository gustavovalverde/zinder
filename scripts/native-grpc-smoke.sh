#!/usr/bin/env bash
#
# Smoke-test the native `zinder-query` gRPC surface against an already-running
# query process. Validates the deployment's advertised capabilities against the
# authoritative capability table and exercises the read-path RPCs that the wire
# surface most depends on (`BlockIdBySelector`, `BlockHeaderBySelector`,
# `LatestBlock`, `Transaction`).
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

WALLET_QUERY_SERVICE="zinder.v1.wallet.WalletQuery"
SERVER_INFO_RPC="${WALLET_QUERY_SERVICE}/ServerInfo"
LATEST_BLOCK_RPC="${WALLET_QUERY_SERVICE}/LatestBlock"
BLOCK_ID_BY_SELECTOR_RPC="${WALLET_QUERY_SERVICE}/BlockIdBySelector"
BLOCK_HEADER_BY_SELECTOR_RPC="${WALLET_QUERY_SERVICE}/BlockHeaderBySelector"
TRANSACTION_RPC="${WALLET_QUERY_SERVICE}/Transaction"
SMOKE_RPC_METHODS=(
  "${SERVER_INFO_RPC}"
  "${LATEST_BLOCK_RPC}"
  "${BLOCK_ID_BY_SELECTOR_RPC}"
  "${BLOCK_HEADER_BY_SELECTOR_RPC}"
  "${TRANSACTION_RPC}"
)

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

wallet_capability_constants() {
  awk '
    /^pub const CAPABILITIES:/ {
      in_table = 1
      next
    }
    !in_table {
      next
    }
    /CapabilitySpec::new\(/ {
      in_spec = 1
      capability_constant = ""
      next
    }
    in_spec && /^[[:space:]]*WALLET_[A-Z0-9_]+,/ {
      capability_constant = $0
      gsub(/^[[:space:]]+|,[[:space:]]*$/, "", capability_constant)
    }
    in_spec && /CapabilitySurface::Wallet,/ && capability_constant != "" {
      print capability_constant
    }
    in_spec && /^[[:space:]]*\),[[:space:]]*$/ {
      in_spec = 0
    }
  ' "${CAPABILITIES_RS}"
}

capability_constant_for_rpc() {
  local rpc_method="$1"
  local qualified_method="${rpc_method/\//.}"

  awk -v qualified_method="${qualified_method}" '
    /^pub const CAPABILITIES:/ {
      in_table = 1
      next
    }
    !in_table {
      next
    }
    /CapabilitySpec::new\(/ {
      in_spec = 1
      capability_constant = ""
      next
    }
    in_spec && /^[[:space:]]*WALLET_[A-Z0-9_]+,/ {
      capability_constant = $0
      gsub(/^[[:space:]]+|,[[:space:]]*$/, "", capability_constant)
    }
    in_spec && index($0, "\"" qualified_method "\"") > 0 {
      print capability_constant
      exit
    }
    in_spec && /^[[:space:]]*\),[[:space:]]*$/ {
      in_spec = 0
    }
  ' "${CAPABILITIES_RS}"
}

capability_string_for_constant() {
  local capability_constant="$1"

  awk -v capability_constant="${capability_constant}" '
    $0 ~ "^pub const " capability_constant ": &str =" {
      in_definition = 1
    }
    in_definition && match($0, /"wallet\.[a-z0-9_.]+_v[0-9]+"/) {
      capability_string = substr($0, RSTART + 1, RLENGTH - 2)
      print capability_string
      exit
    }
    in_definition && /;/ {
      exit
    }
  ' "${CAPABILITIES_RS}"
}

registered_wallet_capabilities() {
  local capability_constant
  local capability_string

  while IFS= read -r capability_constant; do
    capability_string="$(capability_string_for_constant "${capability_constant}")"
    [[ -n "${capability_string}" ]] \
      || die "could not resolve ${capability_constant} in ${CAPABILITIES_RS}"
    printf '%s\n' "${capability_string}"
  done < <(wallet_capability_constants)
}

required_smoke_capabilities() {
  local rpc_method
  local capability_constant
  local capability_string

  for rpc_method in "${SMOKE_RPC_METHODS[@]}"; do
    capability_constant="$(capability_constant_for_rpc "${rpc_method}")"
    [[ -n "${capability_constant}" ]] \
      || die "${rpc_method} has no WalletQuery capability in ${CAPABILITIES_RS}"
    capability_string="$(capability_string_for_constant "${capability_constant}")"
    [[ -n "${capability_string}" ]] \
      || die "could not resolve ${capability_constant} in ${CAPABILITIES_RS}"
    printf '%s\n' "${capability_string}"
  done
}

require_command grpcurl
require_command jq
[[ -f "${WALLET_PROTO}" ]] || die "wallet.proto not found at ${WALLET_PROTO}" 2
[[ -f "${CAPABILITIES_RS}" ]] || die "capabilities.rs not found at ${CAPABILITIES_RS}" 2

log "endpoint=${QUERY_ADDR} height=${HEIGHT:-latest}"

# 1. Capability descriptor parity with the authoritative WalletQuery table.
log "probe ServerInfo"
server_info=$(grpc_call "${SERVER_INFO_RPC}" || die "ServerInfo unreachable")
if ! jq -e '(.info.common.capabilities | type) == "array"' \
  >/dev/null <<<"${server_info}"; then
  die "ServerInfo did not return info.common.capabilities: ${server_info}"
fi

advertised_wallet_capability_set=$(jq -r '.info.common.capabilities[]' \
  <<<"${server_info}" | sort -u)
registered_wallet_capability_set=$(registered_wallet_capabilities | sort -u)
required_smoke_capability_set=$(required_smoke_capabilities | sort -u)

[[ -n "${advertised_wallet_capability_set}" ]] \
  || die "ServerInfo advertised no WalletQuery capabilities"
[[ -n "${registered_wallet_capability_set}" ]] \
  || die "could not parse WalletQuery capabilities from ${CAPABILITIES_RS}"

advertised_capability_count=$(jq '.info.common.capabilities | length' <<<"${server_info}")
advertised_unique_capability_count=$(wc -l \
  <<<"${advertised_wallet_capability_set}" | tr -d ' ')
if [[ "${advertised_capability_count}" != "${advertised_unique_capability_count}" ]]; then
  die "ServerInfo advertised duplicate capabilities"
fi

unregistered_capabilities=$(comm -13 \
  <(printf '%s\n' "${registered_wallet_capability_set}") \
  <(printf '%s\n' "${advertised_wallet_capability_set}") || true)
missing_smoke_capabilities=$(comm -23 \
  <(printf '%s\n' "${required_smoke_capability_set}") \
  <(printf '%s\n' "${advertised_wallet_capability_set}") || true)

if [[ -n "${unregistered_capabilities}" || -n "${missing_smoke_capabilities}" ]]; then
  printf '[native-grpc-smoke] capability contract drift detected:\n' >&2
  [[ -n "${unregistered_capabilities}" ]] \
    && printf '  unregistered capability advertised: %s\n' \
      "${unregistered_capabilities}" >&2
  [[ -n "${missing_smoke_capabilities}" ]] \
    && printf '  required smoke capability missing:  %s\n' \
      "${missing_smoke_capabilities}" >&2
  exit 1
fi
log "ServerInfo advertised ${advertised_capability_count} registered WalletQuery capabilities"

# 2. LatestBlock — round-trip the visible chain epoch.
log "probe LatestBlock"
latest=$(grpc_call "${LATEST_BLOCK_RPC}" || die "LatestBlock failed")
latest_height=$(jq -r '.latestBlock.height // empty' <<<"${latest}")
latest_chain_epoch_id=$(jq -r \
  '.chainView.chainEpoch.chainEpochId // empty' <<<"${latest}")
[[ -n "${latest_height}" ]] || die "LatestBlock did not return a height: ${latest}"
[[ -n "${latest_chain_epoch_id}" ]] \
  || die "LatestBlock did not return a chain epoch id: ${latest}"
log "LatestBlock.height=${latest_height}"
if [[ -z "${HEIGHT}" ]]; then
  HEIGHT="${latest_height}"
  log "probe height defaulted to LatestBlock.height=${HEIGHT}"
fi

# 3. BlockIdBySelector by height.
log "probe BlockIdBySelector (height=${HEIGHT})"
block_id=$(grpc_call "${BLOCK_ID_BY_SELECTOR_RPC}" \
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
hash_lookup=$(grpc_call "${BLOCK_ID_BY_SELECTOR_RPC}" \
  "{\"selector\":{\"hash\":\"${resolved_hash}\"}}" \
  || die "BlockIdBySelector by hash failed")
hash_lookup_height=$(jq -r '.blockId.height // empty' <<<"${hash_lookup}")
[[ "${hash_lookup_height}" == "${HEIGHT}" ]] \
  || die "BlockIdBySelector by hash returned height=${hash_lookup_height}, expected ${HEIGHT}"
log "BlockIdBySelector by hash matched height=${hash_lookup_height}"

# 5. BlockHeaderBySelector — exercises the typed BlockHeaderInfo shape.
log "probe BlockHeaderBySelector (height=${HEIGHT})"
header=$(grpc_call "${BLOCK_HEADER_BY_SELECTOR_RPC}" \
  "{\"selector\":{\"height\":${HEIGHT}}}" \
  || die "BlockHeaderBySelector failed")
header_height=$(jq -r '.blockHeader.blockId.height // empty' <<<"${header}")
[[ "${header_height}" == "${HEIGHT}" ]] \
  || die "BlockHeaderBySelector returned height=${header_height}, expected ${HEIGHT}"
log "BlockHeaderBySelector returned typed header at height=${header_height}"

# 6. Transaction NotFound mapping — confirm the wire returns plain NOT_FOUND
#    (Code = 5) for an unknown txid, with a "not visible" message.
log "probe Transaction (unknown txid expects NOT_FOUND)"
unknown_transaction_id=$(printf 'a%.0s' {1..64})
not_found=$(grpcurl -plaintext \
  -import-path "${PROTO_DIR}" \
  -proto "${WALLET_PROTO}" \
  -d "{\"transactionId\":\"${unknown_transaction_id}\",\"atEpochId\":\"${latest_chain_epoch_id}\"}" \
  "${QUERY_ADDR}" "${TRANSACTION_RPC}" 2>&1 \
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
