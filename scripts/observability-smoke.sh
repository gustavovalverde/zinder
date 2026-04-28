#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ROOT_DIR}/docker-compose.observability.yml"
PROJECT_NAME="${ZINDER_OBSERVABILITY_PROJECT:-zinder-observability}"
WORK_DIR="${ZINDER_OBSERVABILITY_WORK_DIR:-${ROOT_DIR}/.tmp/observability}"
CONFIG_DIR="${WORK_DIR}/config"
LOG_DIR="${WORK_DIR}/logs"
PIDS_FILE="${WORK_DIR}/pids"
REPORT_DIR="${WORK_DIR}/reports"

NETWORK="${ZINDER_OBSERVABILITY_NETWORK:-${ZINDER_NETWORK:-zcash-regtest}}"
NODE_ADDR="${ZINDER_OBSERVABILITY_NODE_ADDR:-${ZINDER_NODE__JSON_RPC_ADDR:-http://127.0.0.1:39232}}"
NODE_AUTH_USERNAME="${ZINDER_OBSERVABILITY_NODE_AUTH_USERNAME:-${ZINDER_NODE__AUTH__USERNAME:-zebra}}"
NODE_AUTH_PASSWORD="${ZINDER_OBSERVABILITY_NODE_AUTH_PASSWORD:-${ZINDER_NODE__AUTH__PASSWORD:-zebra}}"

PROMETHEUS_PORT="${ZINDER_PROMETHEUS_PORT:-9095}"
GRAFANA_PORT="${ZINDER_GRAFANA_PORT:-3002}"
INGEST_OPS_ADDR="${ZINDER_OBSERVABILITY_INGEST_OPS_ADDR:-0.0.0.0:9190}"
QUERY_OPS_ADDR="${ZINDER_OBSERVABILITY_QUERY_OPS_ADDR:-0.0.0.0:9191}"
COMPAT_OPS_ADDR="${ZINDER_OBSERVABILITY_COMPAT_OPS_ADDR:-0.0.0.0:9192}"
INGEST_CONTROL_ADDR="${ZINDER_OBSERVABILITY_INGEST_CONTROL_ADDR:-${ZINDER_OBSERVABILITY_WRITER_STATUS_ADDR:-127.0.0.1:9100}}"
QUERY_GRPC_ADDR="${ZINDER_OBSERVABILITY_QUERY_GRPC_ADDR:-127.0.0.1:9101}"
COMPAT_GRPC_ADDR="${ZINDER_OBSERVABILITY_COMPAT_GRPC_ADDR:-127.0.0.1:9067}"
RESTORE_QUERY_GRPC_ADDR="${ZINDER_OBSERVABILITY_RESTORE_QUERY_GRPC_ADDR:-127.0.0.1:9103}"
RESTORE_QUERY_OPS_ADDR="${ZINDER_OBSERVABILITY_RESTORE_QUERY_OPS_ADDR:-0.0.0.0:9193}"

BACKFILL_BLOCKS="${ZINDER_OBSERVABILITY_BACKFILL_BLOCKS:-50}"
COMMIT_BATCH_BLOCKS="${ZINDER_OBSERVABILITY_COMMIT_BATCH_BLOCKS:-25}"
TIP_FOLLOW_POLL_INTERVAL_MS="${ZINDER_OBSERVABILITY_TIP_FOLLOW_POLL_INTERVAL_MS:-1000}"
GENERATE_BLOCKS="${ZINDER_OBSERVABILITY_GENERATE_BLOCKS:-1}"
RESET_WORK_DIR="${ZINDER_OBSERVABILITY_RESET:-1}"
BACKUP_RESTORE_CHECK="${ZINDER_OBSERVABILITY_BACKUP_RESTORE:-1}"
CALIBRATION_RUNS="${ZINDER_OBSERVABILITY_RUNS:-5}"
LIGHTWALLETD_TESTCLIENT="${ZINDER_OBSERVABILITY_LIGHTWALLETD_TESTCLIENT:-0}"
LIGHTWALLETD_REPO="${ZINDER_OBSERVABILITY_LIGHTWALLETD_REPO:-}"

INGEST_CONFIG="${CONFIG_DIR}/zinder-ingest.toml"
QUERY_CONFIG="${CONFIG_DIR}/zinder-query.toml"
COMPAT_CONFIG="${CONFIG_DIR}/zinder-compat-lightwalletd.toml"
RESTORE_QUERY_CONFIG="${CONFIG_DIR}/zinder-query-restore.toml"
GRPC_HEALTH_PROTO="${CONFIG_DIR}/grpc-health-v1.proto"

BACKFILL_SECONDS="null"
BACKUP_RESTORE_STATUS="not_run"
BACKUP_RESTORE_ERROR_CLASS="none"
REPORT_JSON_PATH=""
REPORT_MARKDOWN_PATH=""
RUN_ID=""
STARTED_PROCESS_PID=""

log() {
  printf '[zinder-observability] %s\n' "$*"
}

die() {
  printf '[zinder-observability] error: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'USAGE'
Usage: scripts/observability-smoke.sh [run|calibrate|snapshot|stop]

Commands:
  run        Start Prometheus/Grafana, backfill from a checkpoint, verify
             backup restore, start ingest/query/compat services, generate
             traffic, print Prometheus evidence, and write a readiness report.
             This is the default command.
  calibrate  Run the same smoke multiple times and write an aggregate baseline
             report. Set ZINDER_OBSERVABILITY_RUNS to control the run count.
  snapshot  Query the currently running Prometheus and service /metrics surfaces.
  stop      Stop local Zinder service processes and the observability compose stack.

Default local node:
  ZINDER_OBSERVABILITY_NODE_ADDR=http://127.0.0.1:39232
  ZINDER_OBSERVABILITY_NODE_AUTH_USERNAME=zebra
  ZINDER_OBSERVABILITY_NODE_AUTH_PASSWORD=zebra

Optional upstream lightwalletd client check:
  ZINDER_OBSERVABILITY_LIGHTWALLETD_TESTCLIENT=1
  ZINDER_OBSERVABILITY_LIGHTWALLETD_REPO=/path/to/lightwalletd
USAGE
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

require_commands() {
  require_command cargo
  require_command curl
  require_command docker
  require_command grpcurl
  require_command jq
  require_command python3
}

docker_compose() {
  docker compose -f "$COMPOSE_FILE" -p "$PROJECT_NAME" "$@"
}

local_url_addr() {
  local listen_addr="$1"
  printf '127.0.0.1:%s' "${listen_addr##*:}"
}

toml_escape() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '%s' "$value"
}

json_rpc() {
  local method="$1"
  local params="${2:-[]}"

  curl -fsS \
    -u "${NODE_AUTH_USERNAME}:${NODE_AUTH_PASSWORD}" \
    -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":\"zinder-observability\",\"method\":\"${method}\",\"params\":${params}}" \
    "$NODE_ADDR"
}

node_tip_height() {
  local response
  response="$(json_rpc getblockcount '[]')"
  if jq -e '.error? != null' >/dev/null <<<"$response"; then
    jq -r '.error.message // .error' <<<"$response" >&2
    return 1
  fi
  jq -er '.result | numbers' <<<"$response"
}

wait_http() {
  local name="$1"
  local url="$2"
  local timeout_seconds="${3:-60}"
  local deadline=$((SECONDS + timeout_seconds))

  until curl -fsS "$url" >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      die "${name} did not become reachable at ${url}"
    fi
    sleep 1
  done
}

reload_prometheus() {
  curl -fsS -X POST "http://127.0.0.1:${PROMETHEUS_PORT}/-/reload" >/dev/null 2>&1 || true
}

prometheus_query() {
  local query="$1"
  curl -fsS --get "http://127.0.0.1:${PROMETHEUS_PORT}/api/v1/query" \
    --data-urlencode "query=${query}"
}

prometheus_max_value() {
  local query="$1"
  local response

  if ! response="$(prometheus_query "$query" 2>/dev/null)"; then
    printf 'null'
    return 0
  fi

  jq -r '
    if .status != "success" or (.data.result | length) == 0 then
      "null"
    else
      ([.data.result[].value[1] | tonumber] | max)
    end
  ' <<<"$response"
}

prometheus_sample_count() {
  local query="$1"
  local response

  if ! response="$(prometheus_query "$query" 2>/dev/null)"; then
    printf '0'
    return 0
  fi

  jq -r '
    if .status != "success" then
      0
    else
      (.data.result | length)
    end
  ' <<<"$response"
}

wait_prometheus_samples() {
  local name="$1"
  local query="$2"
  local timeout_seconds="${3:-45}"
  local deadline=$((SECONDS + timeout_seconds))
  local response

  while true; do
    if response="$(prometheus_query "$query" 2>/dev/null)" &&
      jq -e '.status == "success" and (.data.result | length > 0)' >/dev/null <<<"$response"; then
      return 0
    fi

    if (( SECONDS >= deadline )); then
      log "${name}: no Prometheus samples yet for query: ${query}"
      return 1
    fi
    sleep 1
  done
}

prepare_work_dir() {
  mkdir -p "$WORK_DIR"
  if [[ "$RESET_WORK_DIR" == "1" ]]; then
    rm -rf \
      "${WORK_DIR}/zinder-store" \
      "${WORK_DIR}/query-secondary" \
      "${WORK_DIR}/compat-secondary" \
      "${WORK_DIR}/restore-query-secondary" \
      "${WORK_DIR}/backup-checkpoint" \
      "$CONFIG_DIR" \
      "$LOG_DIR"
  fi
  mkdir -p "$CONFIG_DIR" "$LOG_DIR" "$REPORT_DIR"
  : >"$PIDS_FILE"
}

write_configs() {
  local storage_path query_secondary_path compat_secondary_path
  storage_path="$(toml_escape "${WORK_DIR}/zinder-store")"
  query_secondary_path="$(toml_escape "${WORK_DIR}/query-secondary")"
  compat_secondary_path="$(toml_escape "${WORK_DIR}/compat-secondary")"
  local node_addr node_username node_password network
  node_addr="$(toml_escape "$NODE_ADDR")"
  node_username="$(toml_escape "$NODE_AUTH_USERNAME")"
  node_password="$(toml_escape "$NODE_AUTH_PASSWORD")"
  network="$(toml_escape "$NETWORK")"

  cat >"$INGEST_CONFIG" <<EOF
[network]
name = "${network}"

[node]
source = "zebra-json-rpc"
json_rpc_addr = "${node_addr}"
request_timeout_secs = 30
max_response_bytes = 16777216

[node.auth]
method = "basic"
username = "${node_username}"
password = "${node_password}"

[storage]
path = "${storage_path}"

[ingest]
reorg_window_blocks = 100
commit_batch_blocks = ${COMMIT_BATCH_BLOCKS}

[ingest.control]
listen_addr = "${INGEST_CONTROL_ADDR}"

[backfill]
from_height = ${BACKFILL_FROM_HEIGHT}
to_height = ${BACKFILL_TO_HEIGHT}
allow_near_tip_finalize = true
checkpoint_height = ${CHECKPOINT_HEIGHT}

[tip_follow]
poll_interval_ms = ${TIP_FOLLOW_POLL_INTERVAL_MS}
lag_threshold_blocks = 2
EOF

  cat >"$QUERY_CONFIG" <<EOF
[network]
name = "${network}"

[storage]
path = "${storage_path}"
secondary_path = "${query_secondary_path}"
secondary_catchup_interval_ms = 250
secondary_replica_lag_threshold_chain_epochs = 4
ingest_control_addr = "http://${INGEST_CONTROL_ADDR}"

[query]
listen_addr = "${QUERY_GRPC_ADDR}"

[node]
json_rpc_addr = "${node_addr}"
request_timeout_secs = 30
max_response_bytes = 16777216

[node.auth]
method = "basic"
username = "${node_username}"
password = "${node_password}"
EOF

  cat >"$COMPAT_CONFIG" <<EOF
[network]
name = "${network}"

[storage]
path = "${storage_path}"
secondary_path = "${compat_secondary_path}"
secondary_catchup_interval_ms = 250
secondary_replica_lag_threshold_chain_epochs = 4
ingest_control_addr = "http://${INGEST_CONTROL_ADDR}"

[compat]
listen_addr = "${COMPAT_GRPC_ADDR}"

[node]
json_rpc_addr = "${node_addr}"
request_timeout_secs = 30
max_response_bytes = 16777216

[node.auth]
method = "basic"
username = "${node_username}"
password = "${node_password}"
EOF

  write_grpc_health_proto
}

write_grpc_health_proto() {
  cat >"$GRPC_HEALTH_PROTO" <<'EOF'
syntax = "proto3";

package grpc.health.v1;

message HealthCheckRequest {
  string service = 1;
}

message HealthCheckResponse {
  enum ServingStatus {
    UNKNOWN = 0;
    SERVING = 1;
    NOT_SERVING = 2;
    SERVICE_UNKNOWN = 3;
  }
  ServingStatus status = 1;
}

service Health {
  rpc Check(HealthCheckRequest) returns (HealthCheckResponse);
  rpc Watch(HealthCheckRequest) returns (stream HealthCheckResponse);
}
EOF
}

write_restore_query_config() {
  local restored_storage_path restore_secondary_path network
  restored_storage_path="$(toml_escape "$1")"
  restore_secondary_path="$(toml_escape "${WORK_DIR}/restore-query-secondary")"
  network="$(toml_escape "$NETWORK")"

  cat >"$RESTORE_QUERY_CONFIG" <<EOF
[network]
name = "${network}"

[storage]
path = "${restored_storage_path}"
secondary_path = "${restore_secondary_path}"
secondary_catchup_interval_ms = 250
secondary_replica_lag_threshold_chain_epochs = 4
ingest_control_addr = "http://${INGEST_CONTROL_ADDR}"

[query]
listen_addr = "${RESTORE_QUERY_GRPC_ADDR}"
EOF
}

stop_services() {
  if [[ ! -f "$PIDS_FILE" ]]; then
    return 0
  fi

  while read -r name pid; do
    [[ -n "${name:-}" && -n "${pid:-}" ]] || continue
    if kill -0 "$pid" >/dev/null 2>&1; then
      log "stopping ${name} (${pid})"
      kill "-${pid}" >/dev/null 2>&1 || kill "$pid" >/dev/null 2>&1 || true
    fi
  done <"$PIDS_FILE"

  sleep 1

  while read -r name pid; do
    [[ -n "${name:-}" && -n "${pid:-}" ]] || continue
    if kill -0 "$pid" >/dev/null 2>&1; then
      log "force-stopping ${name} (${pid})"
      kill -9 "-${pid}" >/dev/null 2>&1 || kill -9 "$pid" >/dev/null 2>&1 || true
    fi
  done <"$PIDS_FILE"

  rm -f "$PIDS_FILE"
}

stop_process_pid() {
  local name="$1"
  local pid="$2"

  if [[ -z "$pid" ]]; then
    return 0
  fi

  if kill -0 "$pid" >/dev/null 2>&1; then
    log "stopping ${name} (${pid})"
    kill "-${pid}" >/dev/null 2>&1 || kill "$pid" >/dev/null 2>&1 || true
    sleep 1
  fi

  if kill -0 "$pid" >/dev/null 2>&1; then
    log "force-stopping ${name} (${pid})"
    kill -9 "-${pid}" >/dev/null 2>&1 || kill -9 "$pid" >/dev/null 2>&1 || true
  fi
}

start_process() {
  local name="$1"
  shift
  local log_file="${LOG_DIR}/${name}.log"
  local pid

  log "starting ${name}; log: ${log_file}"
  if ! pid="$(
    python3 - "$log_file" "$@" <<'PY'
import os
import sys

log_path = sys.argv[1]
argv = sys.argv[2:]

pid = os.fork()
if pid != 0:
    print(pid)
    raise SystemExit(0)

os.setsid()
preserved_env = {
    "HOME": os.environ.get("HOME", ""),
    "PATH": os.environ.get("PATH", ""),
    "TMPDIR": os.environ.get("TMPDIR", "/tmp"),
    "RUST_LOG": os.environ.get("RUST_LOG", "info"),
}
os.environ.clear()
os.environ.update({key: value for key, value in preserved_env.items() if value})
log_fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
stdin_fd = os.open(os.devnull, os.O_RDONLY)
os.dup2(stdin_fd, 0)
os.dup2(log_fd, 1)
os.dup2(log_fd, 2)
os.close(stdin_fd)
os.close(log_fd)
os.execvp(argv[0], argv)
PY
  )"; then
    die "failed to launch ${name}"
  fi
  STARTED_PROCESS_PID="$pid"
  printf '%s %s\n' "$name" "$pid" >>"$PIDS_FILE"

  sleep 2
  if ! kill -0 "$pid" >/dev/null 2>&1; then
    tail -n 120 "$log_file" >&2 || true
    die "${name} exited during startup"
  fi
}

zinder_process_env() {
  env -i \
    "HOME=${HOME:-}" \
    "PATH=${PATH:-}" \
    "TMPDIR=${TMPDIR:-/tmp}" \
    "RUST_LOG=${RUST_LOG:-info}" \
    "$@"
}

build_binaries() {
  local log_file="${LOG_DIR}/build.log"
  log "building Zinder service binaries; log: ${log_file}"
  if ! (
    cd "$ROOT_DIR"
    cargo build -p zinder-ingest -p zinder-query -p zinder-compat-lightwalletd
  ) >"$log_file" 2>&1; then
    tail -n 160 "$log_file" >&2 || true
    die "service binary build failed"
  fi
}

run_backfill() {
  local log_file="${LOG_DIR}/backfill.log"
  local started_at ended_at
  log "backfilling ${BACKFILL_FROM_HEIGHT}..${BACKFILL_TO_HEIGHT} from checkpoint ${CHECKPOINT_HEIGHT}; log: ${log_file}"
  started_at="$(python3 - <<'PY'
import time
print(time.time())
PY
)"
  if ! (
    cd "$ROOT_DIR"
    zinder_process_env "${ROOT_DIR}/target/debug/zinder-ingest" --config "$INGEST_CONFIG" backfill
  ) >"$log_file" 2>&1; then
    tail -n 160 "$log_file" >&2 || true
    die "checkpoint backfill failed"
  fi
  ended_at="$(python3 - <<'PY'
import time
print(time.time())
PY
)"
  BACKFILL_SECONDS="$(python3 - "$started_at" "$ended_at" <<'PY'
import sys
started_at = float(sys.argv[1])
ended_at = float(sys.argv[2])
print(f"{ended_at - started_at:.6f}")
PY
)"
  log "checkpoint backfill completed in ${BACKFILL_SECONDS}s"
}

run_backup_restore_check() {
  if [[ "$BACKUP_RESTORE_CHECK" == "0" ]]; then
    BACKUP_RESTORE_STATUS="disabled"
    BACKUP_RESTORE_ERROR_CLASS="none"
    log "skipping backup restore check"
    return 0
  fi

  local backup_path="${WORK_DIR}/backup-checkpoint"
  local backup_log="${LOG_DIR}/backup.log"
  local restore_output="${LOG_DIR}/grpc-restore-latest-block.json"
  local restore_pid restore_response

  rm -rf "$backup_path" "${WORK_DIR}/restore-query-secondary"
  log "creating backup checkpoint at ${backup_path}; log: ${backup_log}"
  if ! (
    cd "$ROOT_DIR"
    zinder_process_env "${ROOT_DIR}/target/debug/zinder-ingest" \
      --config "$INGEST_CONFIG" \
      backup \
      --to "$backup_path"
  ) >"$backup_log" 2>&1; then
    BACKUP_RESTORE_STATUS="failed"
    BACKUP_RESTORE_ERROR_CLASS="backup_failed"
    tail -n 120 "$backup_log" >&2 || true
    die "backup checkpoint creation failed"
  fi

  write_restore_query_config "$backup_path"
  start_process zinder-query-restore \
    "${ROOT_DIR}/target/debug/zinder-query" \
      --config "$RESTORE_QUERY_CONFIG" \
      --ops-listen-addr "$RESTORE_QUERY_OPS_ADDR"
  restore_pid="$STARTED_PROCESS_PID"

  wait_http zinder-query-restore "http://$(local_url_addr "$RESTORE_QUERY_OPS_ADDR")/healthz" 60
  if ! wallet_grpc_at "$RESTORE_QUERY_GRPC_ADDR" '{}' zinder.v1.wallet.WalletQuery/LatestBlock \
    >"$restore_output" 2>&1; then
    BACKUP_RESTORE_STATUS="failed"
    BACKUP_RESTORE_ERROR_CLASS="restore_query_failed"
    tail -n 80 "$restore_output" >&2 || true
    stop_process_pid zinder-query-restore "$restore_pid"
    die "restored checkpoint query failed"
  fi
  restore_response="$(<"$restore_output")"
  if ! jq -e --argjson height "$BACKFILL_TO_HEIGHT" '.latestBlock.height == $height' \
    >/dev/null <<<"$restore_response"; then
    BACKUP_RESTORE_STATUS="failed"
    BACKUP_RESTORE_ERROR_CLASS="restore_height_mismatch"
    cat "$restore_output" >&2 || true
    stop_process_pid zinder-query-restore "$restore_pid"
    die "restored checkpoint did not serve expected height ${BACKFILL_TO_HEIGHT}"
  fi

  stop_process_pid zinder-query-restore "$restore_pid"
  BACKUP_RESTORE_STATUS="passed"
  BACKUP_RESTORE_ERROR_CLASS="none"
  log "backup restore check passed"
}

wallet_grpc_at() {
  local grpc_addr="$1"
  shift
  local request_json="$1"
  local method="$2"
  grpcurl -plaintext \
    -import-path "${ROOT_DIR}/crates/zinder-proto/proto" \
    -proto zinder/v1/wallet/wallet.proto \
    -d "$request_json" \
    "$grpc_addr" \
    "$method"
}

wallet_grpc() {
  wallet_grpc_at "$QUERY_GRPC_ADDR" "$@"
}

wallet_health_grpc() {
  local request_json="$1"
  local method="$2"
  grpcurl -plaintext \
    -import-path "$CONFIG_DIR" \
    -proto "$(basename "$GRPC_HEALTH_PROTO")" \
    -d "$request_json" \
    "$QUERY_GRPC_ADDR" \
    "$method"
}

lightwalletd_grpc() {
  local request_json="$1"
  local method="$2"
  grpcurl -plaintext \
    -import-path "${ROOT_DIR}/crates/zinder-proto/proto/compat/lightwalletd" \
    -proto service.proto \
    -d "$request_json" \
    "$COMPAT_GRPC_ADDR" \
    "$method"
}

wait_wallet_tip() {
  local target_height="$1"
  local timeout_seconds="${2:-90}"
  local deadline=$((SECONDS + timeout_seconds))
  local response

  while true; do
    if response="$(wallet_grpc '{}' zinder.v1.wallet.WalletQuery/LatestBlock 2>/dev/null)" &&
      jq -e --argjson height "$target_height" '.latestBlock.height >= $height' >/dev/null <<<"$response"; then
      return 0
    fi

    if (( SECONDS >= deadline )); then
      log "latest WalletQuery response:"
      printf '%s\n' "${response:-<unavailable>}"
      die "WalletQuery did not reach visible height ${target_height}"
    fi
    sleep 1
  done
}

run_grpc_call() {
  local name="$1"
  shift
  local output_file="${LOG_DIR}/grpc-${name}.json"
  if "$@" >"$output_file" 2>&1; then
    log "gRPC ${name} ok; output: ${output_file}"
  else
    tail -n 80 "$output_file" >&2 || true
    die "gRPC ${name} failed"
  fi
}

maybe_generate_blocks() {
  TARGET_TIP_HEIGHT="$BACKFILL_TO_HEIGHT"

  if [[ "$GENERATE_BLOCKS" == "0" ]]; then
    log "skipping regtest block generation"
    return 0
  fi

  local before response after
  before="$(node_tip_height)"
  log "requesting ${GENERATE_BLOCKS} regtest block(s) from upstream node"
  if ! response="$(json_rpc generate "[${GENERATE_BLOCKS}]" 2>&1)"; then
    log "node generate RPC failed; ingest commit metrics may remain idle: ${response}"
    return 0
  fi

  if jq -e '.error? != null' >/dev/null <<<"$response"; then
    log "node generate RPC returned an error; ingest commit metrics may remain idle: $(jq -r '.error.message // .error' <<<"$response")"
    return 0
  fi

  after="$(node_tip_height)"
  if (( after > before )); then
    TARGET_TIP_HEIGHT="$after"
    log "node tip advanced from ${before} to ${after}; waiting for tip-follow commit"
    wait_wallet_tip "$TARGET_TIP_HEIGHT" 120
  else
    log "node tip did not advance; ingest commit metrics may remain idle"
  fi
}

generate_traffic() {
  local range_end="$TARGET_TIP_HEIGHT"
  local range_start="$BACKFILL_FROM_HEIGHT"
  local range_limit=$((range_start + 2))
  if (( range_end > range_limit )); then
    range_end="$range_limit"
  fi

  wait_wallet_tip "$TARGET_TIP_HEIGHT" 90

  run_grpc_call native-latest-block \
    wallet_grpc '{}' zinder.v1.wallet.WalletQuery/LatestBlock
  run_grpc_call native-compact-block \
    wallet_grpc "{\"height\":${TARGET_TIP_HEIGHT}}" zinder.v1.wallet.WalletQuery/CompactBlock
  run_grpc_call native-compact-block-range \
    wallet_grpc "{\"startHeight\":${range_start},\"endHeight\":${range_end}}" zinder.v1.wallet.WalletQuery/CompactBlockRange
  run_grpc_call native-latest-tree-state \
    wallet_grpc '{}' zinder.v1.wallet.WalletQuery/LatestTreeState
  run_grpc_call native-server-info \
    wallet_grpc '{}' zinder.v1.wallet.WalletQuery/ServerInfo
  run_grpc_call native-health \
    wallet_health_grpc '{"service":"zinder.v1.wallet.WalletQuery"}' grpc.health.v1.Health/Check

  run_grpc_call compat-latest-block \
    lightwalletd_grpc '{}' cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestBlock
  run_grpc_call compat-block \
    lightwalletd_grpc "{\"height\":${TARGET_TIP_HEIGHT}}" cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlock
  run_grpc_call compat-block-range \
    lightwalletd_grpc "{\"start\":{\"height\":${range_start}},\"end\":{\"height\":${range_end}}}" cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRange
  run_grpc_call compat-latest-tree-state \
    lightwalletd_grpc '{}' cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestTreeState
  run_grpc_call compat-lightd-info \
    lightwalletd_grpc '{}' cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo
}

run_lightwalletd_testclient() {
  if [[ "$LIGHTWALLETD_TESTCLIENT" != "1" ]]; then
    return 0
  fi

  require_command go

  if [[ -z "$LIGHTWALLETD_REPO" ]]; then
    die "ZINDER_OBSERVABILITY_LIGHTWALLETD_REPO is required when ZINDER_OBSERVABILITY_LIGHTWALLETD_TESTCLIENT=1"
  fi
  if [[ ! -d "${LIGHTWALLETD_REPO}/testclient" ]]; then
    die "lightwalletd testclient directory not found under ${LIGHTWALLETD_REPO}"
  fi

  case "$COMPAT_GRPC_ADDR" in
    127.0.0.1:9067 | localhost:9067) ;;
    *)
      die "lightwalletd testclient hard-codes localhost:9067; set ZINDER_OBSERVABILITY_COMPAT_GRPC_ADDR=127.0.0.1:9067"
      ;;
  esac

  local range_start range_end range_limit log_file
  range_start="$BACKFILL_FROM_HEIGHT"
  range_end="$TARGET_TIP_HEIGHT"
  range_limit=$((range_start + 2))
  if (( range_end > range_limit )); then
    range_end="$range_limit"
  fi
  log_file="${LOG_DIR}/lightwalletd-testclient.log"

  log "running upstream lightwalletd testclient; log: ${log_file}"
  if ! (
    cd "$LIGHTWALLETD_REPO"
    go run ./testclient -op getlightdinfo -iterations 1 -v
    go run ./testclient -op getblock -iterations 1 -v "$TARGET_TIP_HEIGHT"
    go run ./testclient -op getblockrange -iterations 1 -v "$range_start" "$range_end"
  ) >"$log_file" 2>&1; then
    tail -n 120 "$log_file" >&2 || true
    die "upstream lightwalletd testclient failed"
  fi
  log "upstream lightwalletd testclient ok"
}

print_query_summary() {
  local name="$1"
  local query="$2"
  local response sample_count

  if ! response="$(prometheus_query "$query" 2>/dev/null)"; then
    printf '%-48s unavailable\n' "$name"
    return 0
  fi

  sample_count="$(jq -r '.data.result | length' <<<"$response")"
  printf '%-48s %s sample(s)\n' "$name" "$sample_count"
  jq -r '
    .data.result[:6][]
    | "  " + (
        .metric
        | to_entries
        | map(select(.key != "__name__"))
        | map("\(.key)=\(.value)")
        | join(",")
      ) + " => " + .value[1]
  ' <<<"$response"
}

snapshot() {
  local ingest_ops_url_addr query_ops_url_addr compat_ops_url_addr
  ingest_ops_url_addr="$(local_url_addr "$INGEST_OPS_ADDR")"
  query_ops_url_addr="$(local_url_addr "$QUERY_OPS_ADDR")"
  compat_ops_url_addr="$(local_url_addr "$COMPAT_OPS_ADDR")"

  log "service endpoints"
  printf '  Prometheus: http://127.0.0.1:%s\n' "$PROMETHEUS_PORT"
  printf '  Grafana:    http://127.0.0.1:%s (admin/admin unless overridden)\n' "$GRAFANA_PORT"
  printf '  Ingest ops: http://%s/metrics\n' "$ingest_ops_url_addr"
  printf '  Query ops:  http://%s/metrics\n' "$query_ops_url_addr"
  printf '  Compat ops: http://%s/metrics\n' "$compat_ops_url_addr"
  printf '  Logs:       %s\n' "$LOG_DIR"

  log "Prometheus evidence"
  print_query_summary "targets up" 'up{stack="zinder-local"}'
  print_query_summary "build info" 'zinder_build_info'
  print_query_summary "readiness states" 'zinder_readiness_state'
  print_query_summary "readiness sync lag" 'zinder_readiness_sync_lag_blocks'
  print_query_summary "readiness replica lag" 'zinder_readiness_replica_lag_chain_epochs'
  print_query_summary "node requests" 'sum by (service, method, status, error_class) (zinder_node_request_total)'
  print_query_summary "ingest commits" 'sum by (service, status, error_class) (zinder_ingest_commit_duration_seconds_count)'
  print_query_summary "ingest writer progress" 'zinder_ingest_writer_chain_epoch_id'
  print_query_summary "ingest writer status requests" 'sum by (service, status, error_class) (zinder_ingest_writer_status_request_total)'
  print_query_summary "ingest writer status available" 'zinder_ingest_writer_status_available'
  print_query_summary "ingest backups" 'sum by (network, status, error_class) (zinder_ingest_backup_total)'
  print_query_summary "query requests" 'sum by (service, operation, status, error_class) (zinder_query_request_total)'
  print_query_summary "wallet query p95" 'zinder_query_request_duration_seconds{quantile="0.95"}'
  print_query_summary "query secondary catchup" 'sum by (service, status, error_class) (zinder_query_secondary_catchup_total)'
  print_query_summary "query secondary catchup p95" 'zinder_query_secondary_catchup_duration_seconds{quantile="0.95"}'
  print_query_summary "query secondary progress" 'zinder_query_secondary_chain_epoch_id'
  print_query_summary "query secondary lag" 'zinder_query_secondary_replica_lag_chain_epochs'
  print_query_summary "query writer status requests" 'sum by (service, status, error_class) (zinder_query_writer_status_request_total)'
  print_query_summary "query writer status available" 'zinder_query_writer_status_available'
  print_query_summary "query writer status progress" 'zinder_query_writer_status_chain_epoch_id'
  print_query_summary "node rpc p95" 'zinder_node_request_latency_seconds{quantile="0.95"}'
  print_query_summary "store reads" 'sum by (service, operation, table, status) (zinder_store_read_latency_seconds_count)'
  print_query_summary "store read p95" 'zinder_store_read_latency_seconds{quantile="0.95"}'
  print_query_summary "visibility seeks" 'sum by (service, artifact_family) (zinder_store_visibility_seek_total)'
  print_query_summary "rocksdb properties" 'zinder_store_rocksdb_property'
}

write_readiness_report() {
  local generated_at
  local run_suffix
  local latest_json="${REPORT_DIR}/latest-readiness.json"
  local latest_markdown="${REPORT_DIR}/latest-readiness.md"
  local report_ingest_metrics_url
  local report_query_metrics_url
  local report_compat_metrics_url
  local report_targets_up
  local report_non_ready_services
  local report_readiness_sync_lag_blocks
  local report_readiness_replica_lag_chain_epochs
  local report_wallet_query_p95_seconds
  local report_node_rpc_p95_seconds
  local report_store_read_p95_seconds
  local report_secondary_catchup_p95_seconds
  local report_rocksdb_pending_compaction_bytes
  local report_rocksdb_running_compactions
  local report_rocksdb_property_samples
  local report_ingest_commit_count
  local report_query_request_count
  local report_node_request_count
  local report_backup_metric_count

  generated_at="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  run_suffix="$(date -u +"%Y%m%dT%H%M%SZ")"
  if [[ -n "${CALIBRATION_RUN_INDEX:-}" ]]; then
    run_suffix="${run_suffix}-run-${CALIBRATION_RUN_INDEX}"
  fi

  RUN_ID="${NETWORK}-${run_suffix}"
  REPORT_JSON_PATH="${REPORT_DIR}/${RUN_ID}.json"
  REPORT_MARKDOWN_PATH="${REPORT_DIR}/${RUN_ID}.md"
  report_ingest_metrics_url="http://$(local_url_addr "$INGEST_OPS_ADDR")/metrics"
  report_query_metrics_url="http://$(local_url_addr "$QUERY_OPS_ADDR")/metrics"
  report_compat_metrics_url="http://$(local_url_addr "$COMPAT_OPS_ADDR")/metrics"
  report_targets_up="$(prometheus_max_value 'sum(up{stack="zinder-local"})')"
  report_non_ready_services="$(prometheus_max_value 'sum(zinder_readiness_state{cause!="ready"} == 1) or vector(0)')"
  report_readiness_sync_lag_blocks="$(prometheus_max_value 'max(zinder_readiness_sync_lag_blocks)')"
  report_readiness_replica_lag_chain_epochs="$(prometheus_max_value 'max(zinder_readiness_replica_lag_chain_epochs)')"
  report_wallet_query_p95_seconds="$(prometheus_max_value 'max(zinder_query_request_duration_seconds{quantile="0.95"})')"
  report_node_rpc_p95_seconds="$(prometheus_max_value 'max(zinder_node_request_latency_seconds{quantile="0.95"})')"
  report_store_read_p95_seconds="$(prometheus_max_value 'max(zinder_store_read_latency_seconds{quantile="0.95"})')"
  report_secondary_catchup_p95_seconds="$(prometheus_max_value 'max(zinder_query_secondary_catchup_duration_seconds{quantile="0.95"})')"
  report_rocksdb_pending_compaction_bytes="$(prometheus_max_value 'max(zinder_store_rocksdb_property{property="rocksdb.estimate-pending-compaction-bytes"})')"
  report_rocksdb_running_compactions="$(prometheus_max_value 'max(zinder_store_rocksdb_property{property="rocksdb.num-running-compactions"})')"
  report_rocksdb_property_samples="$(prometheus_sample_count 'zinder_store_rocksdb_property')"
  report_ingest_commit_count="$(prometheus_max_value 'sum(zinder_ingest_commit_duration_seconds_count)')"
  report_query_request_count="$(prometheus_max_value 'sum(zinder_query_request_total)')"
  report_node_request_count="$(prometheus_max_value 'sum(zinder_node_request_total)')"
  report_backup_metric_count="$(prometheus_max_value 'sum(zinder_ingest_backup_total)')"

  export REPORT_GENERATED_AT="$generated_at"
  export REPORT_RUN_ID="$RUN_ID"
  export REPORT_NETWORK="$NETWORK"
  export REPORT_NODE_ADDR="$NODE_ADDR"
  export REPORT_NODE_TIP_HEIGHT="$BACKFILL_TO_HEIGHT"
  export REPORT_CHECKPOINT_HEIGHT="$CHECKPOINT_HEIGHT"
  export REPORT_BACKFILL_FROM_HEIGHT="$BACKFILL_FROM_HEIGHT"
  export REPORT_BACKFILL_TO_HEIGHT="$BACKFILL_TO_HEIGHT"
  export REPORT_BACKFILL_BLOCKS="$((BACKFILL_TO_HEIGHT - CHECKPOINT_HEIGHT))"
  export REPORT_BACKFILL_SECONDS="$BACKFILL_SECONDS"
  export REPORT_BACKUP_RESTORE_STATUS="$BACKUP_RESTORE_STATUS"
  export REPORT_BACKUP_RESTORE_ERROR_CLASS="$BACKUP_RESTORE_ERROR_CLASS"
  export REPORT_PROMETHEUS_URL="http://127.0.0.1:${PROMETHEUS_PORT}"
  export REPORT_GRAFANA_URL="http://127.0.0.1:${GRAFANA_PORT}"
  export REPORT_INGEST_METRICS_URL="$report_ingest_metrics_url"
  export REPORT_QUERY_METRICS_URL="$report_query_metrics_url"
  export REPORT_COMPAT_METRICS_URL="$report_compat_metrics_url"
  export REPORT_TARGETS_UP="$report_targets_up"
  export REPORT_NON_READY_SERVICES="$report_non_ready_services"
  export REPORT_READINESS_SYNC_LAG_BLOCKS="$report_readiness_sync_lag_blocks"
  export REPORT_READINESS_REPLICA_LAG_CHAIN_EPOCHS="$report_readiness_replica_lag_chain_epochs"
  export REPORT_WALLET_QUERY_P95_SECONDS="$report_wallet_query_p95_seconds"
  export REPORT_NODE_RPC_P95_SECONDS="$report_node_rpc_p95_seconds"
  export REPORT_STORE_READ_P95_SECONDS="$report_store_read_p95_seconds"
  export REPORT_SECONDARY_CATCHUP_P95_SECONDS="$report_secondary_catchup_p95_seconds"
  export REPORT_ROCKSDB_PENDING_COMPACTION_BYTES="$report_rocksdb_pending_compaction_bytes"
  export REPORT_ROCKSDB_RUNNING_COMPACTIONS="$report_rocksdb_running_compactions"
  export REPORT_ROCKSDB_PROPERTY_SAMPLES="$report_rocksdb_property_samples"
  export REPORT_INGEST_COMMIT_COUNT="$report_ingest_commit_count"
  export REPORT_QUERY_REQUEST_COUNT="$report_query_request_count"
  export REPORT_NODE_REQUEST_COUNT="$report_node_request_count"
  export REPORT_BACKUP_METRIC_COUNT="$report_backup_metric_count"

  python3 - "$REPORT_JSON_PATH" "$REPORT_MARKDOWN_PATH" <<'PY'
import json
import os
import sys
from pathlib import Path

json_path = Path(sys.argv[1])
markdown_path = Path(sys.argv[2])


def metric(name):
    value = os.environ[name]
    if value == "null" or value == "":
        return None
    if any(ch in value for ch in (".", "e", "E")):
        return float(value)
    return int(value)


report = {
    "run_id": os.environ["REPORT_RUN_ID"],
    "generated_at": os.environ["REPORT_GENERATED_AT"],
    "network": os.environ["REPORT_NETWORK"],
    "node": {
        "json_rpc_addr": os.environ["REPORT_NODE_ADDR"],
        "tip_height": metric("REPORT_NODE_TIP_HEIGHT"),
    },
    "checkpoint": {
        "height": metric("REPORT_CHECKPOINT_HEIGHT"),
        "backfill_from_height": metric("REPORT_BACKFILL_FROM_HEIGHT"),
        "backfill_to_height": metric("REPORT_BACKFILL_TO_HEIGHT"),
        "backfill_blocks": metric("REPORT_BACKFILL_BLOCKS"),
    },
    "measurements": {
        "backfill_seconds": metric("REPORT_BACKFILL_SECONDS"),
        "targets_up": metric("REPORT_TARGETS_UP"),
        "non_ready_services": metric("REPORT_NON_READY_SERVICES"),
        "readiness_sync_lag_blocks": metric("REPORT_READINESS_SYNC_LAG_BLOCKS"),
        "readiness_replica_lag_chain_epochs": metric("REPORT_READINESS_REPLICA_LAG_CHAIN_EPOCHS"),
        "wallet_query_p95_max_seconds": metric("REPORT_WALLET_QUERY_P95_SECONDS"),
        "node_rpc_p95_max_seconds": metric("REPORT_NODE_RPC_P95_SECONDS"),
        "store_read_p95_max_seconds": metric("REPORT_STORE_READ_P95_SECONDS"),
        "secondary_catchup_p95_max_seconds": metric("REPORT_SECONDARY_CATCHUP_P95_SECONDS"),
        "rocksdb_pending_compaction_bytes": metric("REPORT_ROCKSDB_PENDING_COMPACTION_BYTES"),
        "rocksdb_running_compactions": metric("REPORT_ROCKSDB_RUNNING_COMPACTIONS"),
        "rocksdb_property_samples": metric("REPORT_ROCKSDB_PROPERTY_SAMPLES"),
        "ingest_commit_count": metric("REPORT_INGEST_COMMIT_COUNT"),
        "query_request_count": metric("REPORT_QUERY_REQUEST_COUNT"),
        "node_request_count": metric("REPORT_NODE_REQUEST_COUNT"),
        "backup_metric_count": metric("REPORT_BACKUP_METRIC_COUNT"),
    },
    "backup_restore": {
        "status": os.environ["REPORT_BACKUP_RESTORE_STATUS"],
        "error_class": os.environ["REPORT_BACKUP_RESTORE_ERROR_CLASS"],
    },
    "endpoints": {
        "prometheus": os.environ["REPORT_PROMETHEUS_URL"],
        "grafana": os.environ["REPORT_GRAFANA_URL"],
        "ingest_metrics": os.environ["REPORT_INGEST_METRICS_URL"],
        "query_metrics": os.environ["REPORT_QUERY_METRICS_URL"],
        "compat_metrics": os.environ["REPORT_COMPAT_METRICS_URL"],
    },
}

json_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")

measurements = report["measurements"]
lines = [
    f"# Zinder Readiness Report: {report['network']}",
    "",
    f"- Run ID: `{report['run_id']}`",
    f"- Generated: `{report['generated_at']}`",
    f"- Node tip: `{report['node']['tip_height']}`",
    f"- Checkpoint height: `{report['checkpoint']['height']}`",
    f"- Backfill range: `{report['checkpoint']['backfill_from_height']}..{report['checkpoint']['backfill_to_height']}`",
    f"- Backfill duration: `{measurements['backfill_seconds']}` seconds",
    f"- Targets up: `{measurements['targets_up']}`",
    f"- Non-ready services: `{measurements['non_ready_services']}`",
    f"- Sync lag: `{measurements['readiness_sync_lag_blocks']}` blocks",
    f"- Replica lag: `{measurements['readiness_replica_lag_chain_epochs']}` chain epochs",
    f"- Wallet query p95 max: `{measurements['wallet_query_p95_max_seconds']}` seconds",
    f"- Node RPC p95 max: `{measurements['node_rpc_p95_max_seconds']}` seconds",
    f"- Store read p95 max: `{measurements['store_read_p95_max_seconds']}` seconds",
    f"- Secondary catchup p95 max: `{measurements['secondary_catchup_p95_max_seconds']}` seconds",
    f"- RocksDB pending compaction bytes: `{measurements['rocksdb_pending_compaction_bytes']}`",
    f"- RocksDB running compactions: `{measurements['rocksdb_running_compactions']}`",
    f"- RocksDB property samples: `{measurements['rocksdb_property_samples']}`",
    f"- Backup restore: `{report['backup_restore']['status']}`",
    "",
    "## Endpoints",
    "",
    f"- Prometheus: {report['endpoints']['prometheus']}",
    f"- Grafana: {report['endpoints']['grafana']}",
    f"- Ingest metrics: {report['endpoints']['ingest_metrics']}",
    f"- Query metrics: {report['endpoints']['query_metrics']}",
    f"- Compat metrics: {report['endpoints']['compat_metrics']}",
    "",
]
markdown_path.write_text("\n".join(lines), encoding="utf-8")
PY

  cp "$REPORT_JSON_PATH" "$latest_json"
  cp "$REPORT_MARKDOWN_PATH" "$latest_markdown"
  log "readiness report: ${REPORT_JSON_PATH}"
  log "readiness summary: ${REPORT_MARKDOWN_PATH}"
}

write_calibration_report() {
  local aggregate_suffix aggregate_json aggregate_markdown latest_json latest_markdown
  aggregate_suffix="$(date -u +"%Y%m%dT%H%M%SZ")"
  aggregate_json="${REPORT_DIR}/calibration-${NETWORK}-${aggregate_suffix}.json"
  aggregate_markdown="${REPORT_DIR}/calibration-${NETWORK}-${aggregate_suffix}.md"
  latest_json="${REPORT_DIR}/latest-calibration.json"
  latest_markdown="${REPORT_DIR}/latest-calibration.md"

  python3 - "$aggregate_json" "$aggregate_markdown" "$@" <<'PY'
import json
import math
import statistics
import sys
from datetime import datetime, timezone
from pathlib import Path

aggregate_json = Path(sys.argv[1])
aggregate_markdown = Path(sys.argv[2])
report_paths = [Path(path) for path in sys.argv[3:]]
reports = [json.loads(path.read_text(encoding="utf-8")) for path in report_paths]


def percentile(values, percentile_value):
    if not values:
        return None
    sorted_values = sorted(values)
    index = max(0, math.ceil((percentile_value / 100) * len(sorted_values)) - 1)
    return sorted_values[min(index, len(sorted_values) - 1)]


def measurement_values(name):
    values = []
    for report in reports:
        value = report["measurements"].get(name)
        if value is not None:
            values.append(value)
    return values


metric_names = [
    "backfill_seconds",
    "wallet_query_p95_max_seconds",
    "node_rpc_p95_max_seconds",
    "store_read_p95_max_seconds",
    "secondary_catchup_p95_max_seconds",
    "readiness_sync_lag_blocks",
    "readiness_replica_lag_chain_epochs",
    "rocksdb_pending_compaction_bytes",
]

summary = {}
for name in metric_names:
    values = measurement_values(name)
    summary[name] = {
        "samples": len(values),
        "p50": statistics.median(values) if values else None,
        "p95": percentile(values, 95),
        "p99": percentile(values, 99),
        "worst_case": max(values) if values else None,
    }

aggregate = {
    "generated_at": datetime.now(timezone.utc)
    .replace(microsecond=0)
    .isoformat()
    .replace("+00:00", "Z"),
    "network": reports[0]["network"] if reports else None,
    "run_count": len(reports),
    "runs": [
        {
            "path": str(path),
            "run_id": report["run_id"],
            "checkpoint_height": report["checkpoint"]["height"],
            "backfill_to_height": report["checkpoint"]["backfill_to_height"],
            "backfill_seconds": report["measurements"]["backfill_seconds"],
            "wallet_query_p95_max_seconds": report["measurements"]["wallet_query_p95_max_seconds"],
            "store_read_p95_max_seconds": report["measurements"]["store_read_p95_max_seconds"],
            "backup_restore_status": report["backup_restore"]["status"],
        }
        for path, report in zip(report_paths, reports)
    ],
    "summary": summary,
}

aggregate_json.write_text(json.dumps(aggregate, indent=2, sort_keys=True) + "\n", encoding="utf-8")

lines = [
    f"# Zinder Calibration Baseline: {aggregate['network']}",
    "",
    f"- Generated: `{aggregate['generated_at']}`",
    f"- Runs: `{aggregate['run_count']}`",
    "",
    "| Metric | Samples | P50 | P95 | P99 | Worst case |",
    "| --- | ---: | ---: | ---: | ---: | ---: |",
]
for name in metric_names:
    metric = summary[name]
    lines.append(
        f"| `{name}` | {metric['samples']} | {metric['p50']} | {metric['p95']} | "
        f"{metric['p99']} | {metric['worst_case']} |"
    )
lines.append("")
aggregate_markdown.write_text("\n".join(lines), encoding="utf-8")
PY

  cp "$aggregate_json" "$latest_json"
  cp "$aggregate_markdown" "$latest_markdown"
  log "calibration report: ${aggregate_json}"
  log "calibration summary: ${aggregate_markdown}"
}

run_stack() {
  require_commands

  local tip_height effective_backfill_blocks
  log "checking node ${NODE_ADDR} on ${NETWORK}"
  tip_height="$(node_tip_height)" || die "node did not return a tip height"
  if (( tip_height < 2 )); then
    die "node tip ${tip_height} is too low for checkpoint backfill"
  fi

  effective_backfill_blocks="$BACKFILL_BLOCKS"
  if (( effective_backfill_blocks >= tip_height )); then
    effective_backfill_blocks=$((tip_height - 1))
  fi
  if (( effective_backfill_blocks < 1 )); then
    die "effective backfill window must be at least one block"
  fi

  CHECKPOINT_HEIGHT=$((tip_height - effective_backfill_blocks))
  BACKFILL_FROM_HEIGHT=$((CHECKPOINT_HEIGHT + 1))
  BACKFILL_TO_HEIGHT="$tip_height"
  export CHECKPOINT_HEIGHT BACKFILL_FROM_HEIGHT BACKFILL_TO_HEIGHT TARGET_TIP_HEIGHT

  stop_services
  prepare_work_dir
  write_configs
  build_binaries

  log "starting Prometheus and Grafana"
  docker_compose up -d
  wait_http prometheus "http://127.0.0.1:${PROMETHEUS_PORT}/-/healthy" 90
  reload_prometheus
  wait_http grafana "http://127.0.0.1:${GRAFANA_PORT}/api/health" 120

  run_backfill
  run_backup_restore_check

  start_process zinder-ingest \
    "${ROOT_DIR}/target/debug/zinder-ingest" \
      --config "$INGEST_CONFIG" \
      --ops-listen-addr "$INGEST_OPS_ADDR" \
      tip-follow
  start_process zinder-query \
    "${ROOT_DIR}/target/debug/zinder-query" \
      --config "$QUERY_CONFIG" \
      --ops-listen-addr "$QUERY_OPS_ADDR"
  start_process zinder-compat-lightwalletd \
    "${ROOT_DIR}/target/debug/zinder-compat-lightwalletd" \
      --config "$COMPAT_CONFIG" \
      --ops-listen-addr "$COMPAT_OPS_ADDR"

  wait_http zinder-ingest "http://$(local_url_addr "$INGEST_OPS_ADDR")/healthz" 60
  wait_http zinder-query "http://$(local_url_addr "$QUERY_OPS_ADDR")/healthz" 60
  wait_http zinder-compat-lightwalletd "http://$(local_url_addr "$COMPAT_OPS_ADDR")/healthz" 60

  maybe_generate_blocks
  generate_traffic
  run_lightwalletd_testclient

  wait_prometheus_samples "target scrape" 'up{stack="zinder-local"}' 45 || true
  wait_prometheus_samples "readiness metric" 'zinder_readiness_state' 45 || true
  wait_prometheus_samples "query request metric" 'zinder_query_request_total' 45 || true
  wait_prometheus_samples "query secondary catchup metric" 'zinder_query_secondary_catchup_total' 45 || true
  wait_prometheus_samples "query writer status metric" 'zinder_query_writer_status_request_total' 45 || true
  wait_prometheus_samples "store read metric" 'zinder_store_read_latency_seconds_count' 45 || true
  wait_prometheus_samples "rocksdb property metric" 'zinder_store_rocksdb_property' 45 || true
  wait_prometheus_samples "ingest commit metric" 'zinder_ingest_commit_duration_seconds_count' 45 || true
  wait_prometheus_samples "ingest writer progress metric" 'zinder_ingest_writer_chain_epoch_id' 45 || true
  wait_prometheus_samples "ingest writer status metric" 'zinder_ingest_writer_status_request_total' 45 || true

  snapshot
  write_readiness_report

  log "smoke run is still running so dashboards stay inspectable"
  log "stop it with: scripts/observability-smoke.sh stop"
}

calibrate_stack() {
  require_commands

  if ! [[ "$CALIBRATION_RUNS" =~ ^[0-9]+$ ]] || (( CALIBRATION_RUNS < 1 )); then
    die "ZINDER_OBSERVABILITY_RUNS must be a positive integer"
  fi

  local reports=()
  local run_index
  for ((run_index = 1; run_index <= CALIBRATION_RUNS; run_index++)); do
    log "calibration run ${run_index}/${CALIBRATION_RUNS}"
    CALIBRATION_RUN_INDEX="$run_index"
    run_stack
    reports+=("$REPORT_JSON_PATH")
  done

  unset CALIBRATION_RUN_INDEX
  write_calibration_report "${reports[@]}"
}

stop_stack() {
  stop_services
  if [[ -f "$COMPOSE_FILE" ]]; then
    docker_compose down
  fi
}

command="${1:-run}"
case "$command" in
  run)
    run_stack
    ;;
  calibrate)
    calibrate_stack
    ;;
  snapshot | status)
    require_commands
    snapshot
    ;;
  stop)
    require_command docker
    stop_stack
    ;;
  -h | --help | help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
