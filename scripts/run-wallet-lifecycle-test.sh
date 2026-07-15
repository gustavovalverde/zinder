#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
compose_file="$repository_root/deploy/docker-compose.wallet-lifecycle-test.yml"
validator="$repository_root/scripts/validate-wallet-lifecycle-report.sh"
evidence_path="${ZINDER_WALLET_LIFECYCLE_EVIDENCE_PATH:-$repository_root/.tmp/wallet-lifecycle-evidence}"
project_name="${ZINDER_WALLET_LIFECYCLE_PROJECT_NAME:-zinder-wallet-lifecycle-test}"
lifecycle_image="${ZINDER_WALLET_LIFECYCLE_IMAGE:-zinder-wallet-lifecycle:local}"
source_network="${ZINDER_SOURCE_NETWORK_NAME:-z3-testnet}"
source_cookie_volume="${ZINDER_SOURCE_COOKIE_VOLUME_NAME:-z3-testnet-cookie}"
source_chain_volume="${ZINDER_SOURCE_CHAIN_VOLUME_NAME:-z3-testnet-chain}"
network="${ZINDER_NETWORK:-zcash-testnet}"

fail() {
  echo >&2 "wallet lifecycle test refused to run: $*"
  exit 1
}

usage() {
  echo >&2 "usage: $0 rocksdb|postgres|all"
  exit 2
}

[[ "$#" -eq 1 ]] || usage
requested_topology="$1"
case "$requested_topology" in
  rocksdb | postgres | all) ;;
  *) usage ;;
esac

[[ "$project_name" == zinder-wallet-lifecycle-* ]] || fail \
  "project name must start with zinder-wallet-lifecycle- so cleanup cannot target Zebra or a deployment"
[[ "$source_cookie_volume" != "$source_chain_volume" ]] || fail \
  "source cookie volume must not be Zebra's chain volume: $source_chain_volume"
[[ "$network" == "zcash-testnet" ]] || fail \
  "this certification contract is testnet-only; ZINDER_NETWORK was $network"

command -v docker >/dev/null 2>&1 || fail "docker is required"
command -v jq >/dev/null 2>&1 || fail "jq is required"
docker compose version >/dev/null 2>&1 || fail "Docker Compose is required"
docker network inspect "$source_network" >/dev/null 2>&1 || fail \
  "source network does not exist: $source_network"
docker volume inspect "$source_cookie_volume" >/dev/null 2>&1 || fail \
  "source cookie volume does not exist: $source_cookie_volume"

mkdir -p "$evidence_path"
[[ -d "$evidence_path" && -w "$evidence_path" ]] || fail \
  "evidence path must be a writable directory: $evidence_path"

export ZINDER_WALLET_LIFECYCLE_EVIDENCE_PATH="$evidence_path"
export ZINDER_WALLET_LIFECYCLE_PROJECT_NAME="$project_name"
export ZINDER_WALLET_LIFECYCLE_IMAGE="$lifecycle_image"
export ZINDER_WALLET_LIFECYCLE_UID="${ZINDER_WALLET_LIFECYCLE_UID:-$(id -u)}"
export ZINDER_WALLET_LIFECYCLE_GID="${ZINDER_WALLET_LIFECYCLE_GID:-$(id -g)}"
export ZINDER_SOURCE_NETWORK_NAME="$source_network"
export ZINDER_SOURCE_COOKIE_VOLUME_NAME="$source_cookie_volume"

compose=(docker compose --project-name "$project_name" --file "$compose_file")
compose_config="$("${compose[@]}" --profile rocksdb --profile postgres config --format json)"

jq -e --arg source_network "$source_network" --arg source_cookie "$source_cookie_volume" '
  .networks.source.external == true
  and .networks.source.name == $source_network
  and .volumes.source_cookie.external == true
  and .volumes.source_cookie.name == $source_cookie
' <<<"$compose_config" >/dev/null || fail \
  "Compose must use the exact external Zebra network and read-only cookie volume"

if jq -e --arg chain_volume "$source_chain_volume" '
  (
    [.services[].volumes[]? | select(.type == "volume") | .source]
    + [.volumes[]?.name]
  )
  | index($chain_volume) != null
' <<<"$compose_config" >/dev/null; then
  fail "Compose must never mount the Zebra chain volume: $source_chain_volume"
fi

docker image inspect "$lifecycle_image" >/dev/null 2>&1 || fail \
  "lifecycle image is unavailable: $lifecycle_image; build the clean-v1 lifecycle certifier first"
docker run --rm --entrypoint /usr/local/bin/zinder-wallet-lifecycle \
  "$lifecycle_image" contract --required-version 1 >/dev/null || fail \
  "image does not provide the wallet lifecycle certification contract version 1"

cleanup_containers() {
  "${compose[@]}" --profile rocksdb --profile postgres down --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup_containers EXIT

# Every certification starts from clean, project-scoped Zinder state. This
# removes no external network or volume, and the Compose file never references
# Zebra's chain volume.
"${compose[@]}" --profile rocksdb --profile postgres down --volumes --remove-orphans
find "$evidence_path" -mindepth 1 -maxdepth 1 -type f -name '*.json' -delete

run_rocksdb() {
  "${compose[@]}" --profile rocksdb run --rm rocksdb-state-init
  "${compose[@]}" --profile rocksdb run --rm --no-deps rocksdb-certify
  "${compose[@]}" --profile rocksdb run --rm --no-deps rocksdb-restart-certify
  "$validator" rocksdb-single-host \
    "$evidence_path/rocksdb-certification.json" \
    "$evidence_path/rocksdb-restart.json"
}

run_postgres() {
  "${compose[@]}" --profile postgres up --detach --wait postgres-database
  "${compose[@]}" --profile postgres run --rm --no-deps postgres-certify
  "${compose[@]}" --profile postgres restart postgres-database
  "${compose[@]}" --profile postgres up --detach --wait postgres-database
  "${compose[@]}" --profile postgres run --rm --no-deps postgres-restart-certify
  "$validator" postgres-scale-out \
    "$evidence_path/postgres-certification.json" \
    "$evidence_path/postgres-restart.json"
}

case "$requested_topology" in
  rocksdb) run_rocksdb ;;
  postgres) run_postgres ;;
  all)
    run_rocksdb
    run_postgres
    ;;
esac

echo "wallet lifecycle certification passed: $requested_topology"
echo "evidence: $evidence_path"
