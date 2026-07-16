#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
compose_file="$repository_root/deploy/docker-compose.storage-lifecycle-test.yml"
validator="$repository_root/scripts/validate-rocksdb-storage-lifecycle-report.sh"
evidence_path="${ZINDER_STORAGE_LIFECYCLE_EVIDENCE_PATH:-$repository_root/.tmp/rocksdb-storage-lifecycle-evidence}"
project_name="${ZINDER_STORAGE_LIFECYCLE_PROJECT_NAME:-zinder-storage-lifecycle-test}"
requested_image="${ZINDER_STORAGE_LIFECYCLE_IMAGE:-zinder-bench:local}"
source_network="${ZINDER_SOURCE_NETWORK_NAME:-z3-testnet}"
source_cookie_volume="${ZINDER_SOURCE_COOKIE_VOLUME_NAME:-z3-testnet-cookie}"
source_chain_volume="${ZINDER_SOURCE_CHAIN_VOLUME_NAME:-z3-testnet-chain}"
network="${ZINDER_NETWORK:-zcash-testnet}"
tip_height="${ZINDER_STORAGE_LIFECYCLE_TIP_HEIGHT:-}"

fail() {
  echo >&2 "RocksDB storage lifecycle refused to run: $*"
  exit 1
}

[[ "$#" -eq 0 ]] || fail "this command accepts configuration through ZINDER_* environment variables"
[[ "$project_name" =~ ^zinder-storage-lifecycle-[a-z0-9][a-z0-9_-]*$ ]] || fail \
  "project name must be a lowercase zinder-storage-lifecycle-* identifier so cleanup cannot target Zebra or a deployment"
[[ "$source_cookie_volume" != "$source_chain_volume" ]] || fail \
  "source cookie volume must not be Zebra's chain volume: $source_chain_volume"
[[ "$network" == "zcash-testnet" ]] || fail \
  "this local certification runner is testnet-only; ZINDER_NETWORK was $network"

for required_command in docker jq; do
  command -v "$required_command" >/dev/null 2>&1 || fail "$required_command is required"
done
docker compose version >/dev/null 2>&1 || fail "Docker Compose is required"
docker network inspect "$source_network" >/dev/null 2>&1 || fail \
  "source network does not exist: $source_network"
docker volume inspect "$source_cookie_volume" >/dev/null 2>&1 || fail \
  "source cookie volume does not exist: $source_cookie_volume"

if [[ -z "$tip_height" ]]; then
  source_container="${ZINDER_SOURCE_CONTAINER_NAME:-z3-testnet-zebra-1}"
  [[ "$(docker inspect --format '{{.State.Running}}' "$source_container" 2>/dev/null)" == true ]] || fail \
    "source container is not running: $source_container"
  tip_height="$(docker exec "$source_container" sh -c '
    auth=$(cat /var/run/auth/.cookie)
    curl --fail --silent --show-error --user "$auth" \
      --header "content-type: application/json" \
      --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getblockchaininfo\",\"params\":[]}" \
      http://127.0.0.1:18232
  ' | jq -er '.result.blocks')" || fail "could not capture the synchronized Zebra tip"
fi
[[ "$tip_height" =~ ^[1-9][0-9]*$ && "$tip_height" -le 4294967295 ]] || fail \
  "ZINDER_STORAGE_LIFECYCLE_TIP_HEIGHT must be a nonzero u32"

resolved_image_id="$(docker image inspect --format '{{.Id}}' "$requested_image" 2>/dev/null)" || fail \
  "benchmark image is unavailable: $requested_image"
[[ "$resolved_image_id" =~ ^sha256:[0-9a-f]{64}$ ]] || fail \
  "Docker returned a malformed immutable image ID for $requested_image"

mkdir -p "$evidence_path"
[[ -d "$evidence_path" && -w "$evidence_path" ]] || fail \
  "evidence path must be a writable directory: $evidence_path"
evidence_path="$(CDPATH='' cd -- "$evidence_path" && pwd -P)"

if [[ -z "${ZINDER_STORAGE_LIFECYCLE_SOFTWARE_REVISION:-}" ]] \
  && [[ -n "$(git -C "$repository_root" status --porcelain --untracked-files=all)" ]]; then
  fail \
    "the worktree is dirty; commit the measured source or set the exact image revision explicitly"
fi
software_revision="${ZINDER_STORAGE_LIFECYCLE_SOFTWARE_REVISION:-$(git -C "$repository_root" rev-parse HEAD)}"
trial_id="${ZINDER_STORAGE_LIFECYCLE_TRIAL_ID:-testnet-$(date -u +%Y%m%dT%H%M%SZ)}"
runner_id="${ZINDER_STORAGE_LIFECYCLE_RUNNER_ID:-local-docker-desktop-12cpu-32gib}"
storage_class="${ZINDER_STORAGE_LIFECYCLE_STORAGE_CLASS:-docker-desktop-local-volume}"

[[ "$software_revision" =~ ^[0-9a-f]{40}$|^[0-9a-f]{64}$ ]] || fail \
  "ZINDER_STORAGE_LIFECYCLE_SOFTWARE_REVISION must be a full hexadecimal object ID"
[[ "$trial_id" =~ ^[[:alnum:]][[:alnum:]._-]*$ ]] || fail \
  "ZINDER_STORAGE_LIFECYCLE_TRIAL_ID must be an evidence identifier"
[[ "$runner_id" =~ ^[[:alnum:]][[:alnum:]._-]*$ ]] || fail \
  "ZINDER_STORAGE_LIFECYCLE_RUNNER_ID must be an evidence identifier"
[[ "$storage_class" =~ ^[[:alnum:]][[:alnum:]._-]*$ ]] || fail \
  "ZINDER_STORAGE_LIFECYCLE_STORAGE_CLASS must be an evidence identifier"

export ZINDER_STORAGE_LIFECYCLE_EVIDENCE_PATH="$evidence_path"
export ZINDER_STORAGE_LIFECYCLE_PROJECT_NAME="$project_name"
export ZINDER_STORAGE_LIFECYCLE_IMAGE="$resolved_image_id"
export ZINDER_STORAGE_LIFECYCLE_SOFTWARE_REVISION="$software_revision"
export ZINDER_STORAGE_LIFECYCLE_TRIAL_ID="$trial_id"
export ZINDER_STORAGE_LIFECYCLE_TIP_HEIGHT="$tip_height"
export ZINDER_STORAGE_LIFECYCLE_UID="${ZINDER_STORAGE_LIFECYCLE_UID:-$(id -u)}"
export ZINDER_STORAGE_LIFECYCLE_GID="${ZINDER_STORAGE_LIFECYCLE_GID:-$(id -g)}"
export ZINDER_STORAGE_LIFECYCLE_RUNNER_ID="$runner_id"
export ZINDER_STORAGE_LIFECYCLE_STORAGE_CLASS="$storage_class"
export ZINDER_SOURCE_NETWORK_NAME="$source_network"
export ZINDER_SOURCE_COOKIE_VOLUME_NAME="$source_cookie_volume"

compose=(docker compose --project-name "$project_name" --file "$compose_file")
compose_config="$("${compose[@]}" config --format json)"

jq -e \
  --arg source_network "$source_network" \
  --arg source_cookie "$source_cookie_volume" \
  --arg evidence_path "$evidence_path" \
  --arg image "$resolved_image_id" \
  --arg tip_height "$tip_height" '
  def command_argument($flag):
    (.command | index($flag)) as $argument_index
    | if $argument_index == null then null else .command[$argument_index + 1] end;

  (.services | keys) == ["rocksdb-storage-lifecycle", "state-init"]
  and (.volumes | keys) == ["canonical_state", "source_cookie", "wallet_state"]
  and (.networks | keys) == ["source"]
  and .networks.source.external == true
  and .networks.source.name == $source_network
  and .volumes.source_cookie.external == true
  and .volumes.source_cookie.name == $source_cookie
  and (.volumes.canonical_state.external // false) == false
  and (.volumes.wallet_state.external // false) == false
  and (.services."state-init"
    | .network_mode == "none"
      and .user == "0:0"
      and .restart == "no"
      and (.volumes | length) == 2
      and ([.volumes[].source] | sort) == ["canonical_state", "wallet_state"]
      and ([.volumes[].target] | sort) == [
        "/var/lib/zinder/canonical-state",
        "/var/lib/zinder/wallet-state"
      ])
  and (.services."rocksdb-storage-lifecycle"
    | .image == $image
      and .read_only == true
      and .cgroup == "private"
      and .init == true
      and .restart == "no"
      and (.networks | keys) == ["source"]
      and (.volumes | length) == 4
      and ([.volumes[] | select(
        .type == "bind"
        and .source == $evidence_path
        and .target == "/var/lib/zinder-evidence"
        and .bind.create_host_path == false
      )] | length) == 1
      and ([.volumes[] | select(
        .type == "volume"
        and .source == "source_cookie"
        and .target == "/var/run/zebra-auth"
        and .read_only == true
      )] | length) == 1
      and ([.volumes[] | select(
        .type == "volume"
        and .source == "canonical_state"
        and .target == "/var/lib/zinder/canonical-state"
        and (.read_only // false) == false
      )] | length) == 1
      and ([.volumes[] | select(
        .type == "volume"
        and .source == "wallet_state"
        and .target == "/var/lib/zinder/wallet-state"
        and (.read_only // false) == false
      )] | length) == 1
      and .environment.ZINDER_BENCH_RESOURCE_COMPONENT_ID == "rocksdb-storage-lifecycle"
      and .environment.ZINDER_BENCH_RESOURCE_EVIDENCE_PATH
        == "/var/lib/zinder-evidence/rocksdb-storage-lifecycle.resources.json"
      and .environment.ZINDER_BENCH_RESOURCE_STORAGE_PATH == "/var/lib/zinder"
      and .environment.ZINDER_BENCH_RESOURCE_REQUIRE_EXACT_MEMORY == "true"
      and .environment.ZINDER_BENCH_RESOURCE_REQUIRE_SAMPLED_STORAGE == "true"
      and .environment.ZINDER_BENCH_RESOURCE_REQUIRE_PRIVATE_CGROUP_NAMESPACE == "true"
      and .command[0] == "rocksdb-storage-lifecycle"
      and command_argument("--tip-height") == $tip_height
      and command_argument("--canonical-store")
        == "/var/lib/zinder/canonical-state/canonical"
      and command_argument("--wallet-store") == "/var/lib/zinder/wallet-state/wallet"
      and command_argument("--report")
        == "/var/lib/zinder-evidence/rocksdb-storage-lifecycle.json"
      and all(.command[]; test("^--(start-height|schema-version|store-version)$") | not))
' <<<"$compose_config" >/dev/null || fail \
  "Compose topology differs from the closed storage lifecycle contract"

if jq -e --arg chain_volume "$source_chain_volume" '
  (
    [.services[].volumes[]? | select(.type == "volume") | .source]
    + [.volumes[]?.name]
  )
  | index($chain_volume) != null
' <<<"$compose_config" >/dev/null; then
  fail "Compose must never mount the Zebra chain volume: $source_chain_volume"
fi

docker run --rm --entrypoint /usr/local/bin/zinder-bench "$resolved_image_id" \
  rocksdb-storage-lifecycle --help >/dev/null || fail \
  "image does not provide the RocksDB storage lifecycle command"

cleanup_containers() {
  "${compose[@]}" down --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup_containers EXIT

# This deletes only project-scoped Zinder state. The rendered topology has
# already proved that Zebra's chain volume is outside the project.
"${compose[@]}" down --volumes --remove-orphans
find "$evidence_path" -mindepth 1 -maxdepth 1 -type f \
  \( -name 'rocksdb-storage-lifecycle.json' -o -name 'rocksdb-storage-lifecycle.resources.json' \) \
  -delete

"${compose[@]}" run --rm state-init
"${compose[@]}" run --rm --no-deps rocksdb-storage-lifecycle
"$validator" \
  "$evidence_path/rocksdb-storage-lifecycle.json" \
  "$evidence_path/rocksdb-storage-lifecycle.resources.json" \
  "$tip_height" \
  "$resolved_image_id" \
  "$software_revision" \
  "$trial_id" \
  "$network"

echo "RocksDB storage lifecycle passed"
echo "report: $evidence_path/rocksdb-storage-lifecycle.json"
echo "resources: $evidence_path/rocksdb-storage-lifecycle.resources.json"
