#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo >&2 "storage benchmark campaign is invalid: $*"
  exit 1
}

if [[ "$#" -ne 1 ]]; then
  echo >&2 "usage: $0 CAMPAIGN_LEDGER.tsv"
  exit 2
fi
command -v jq >/dev/null 2>&1 || fail "jq is required"
command -v realpath >/dev/null 2>&1 || fail "realpath is required"

ledger_input="$1"
[[ -f "$ledger_input" ]] || fail "ledger does not exist: $ledger_input"
ledger_directory="$(CDPATH='' cd -- "$(dirname -- "$ledger_input")" && pwd)"
ledger_path="$ledger_directory/$(basename -- "$ledger_input")"
expected_header=$'rocksdb_report\tpostgres_report'
actual_header="$(sed -n '1p' "$ledger_path")"
[[ "$actual_header" == "$expected_header" ]] || fail "ledger header must be: $expected_header"

scratch_directory="$(mktemp -d)"
trap 'rm -rf "$scratch_directory"' EXIT
trial_ids_path="$scratch_directory/trial-ids"
report_paths_path="$scratch_directory/report-paths"
report_hashes_path="$scratch_directory/report-hashes"
run_timestamps_path="$scratch_directory/run-timestamps"
trial_evidence_path="$scratch_directory/trials.jsonl"
chronological_trials_path="$scratch_directory/chronological-trials.json"
: >"$trial_ids_path"
: >"$report_paths_path"
: >"$report_hashes_path"
: >"$run_timestamps_path"
: >"$trial_evidence_path"

rocksdb_reports=()
postgres_reports=()
trial_count=0
campaign_cache_policy=""
campaign_runner_id=""
common_fingerprint=""
rocksdb_configuration=""
postgres_configuration=""

resolve_report_path() {
  local unresolved_path
  case "$1" in
    /*) unresolved_path="$1" ;;
    *) unresolved_path="$ledger_directory/$1" ;;
  esac
  [[ -f "$unresolved_path" ]] || fail "report does not exist: $unresolved_path"
  realpath "$unresolved_path"
}

report_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

validate_report() {
  local report_path="$1"
  local expected_candidate="$2"
  local expected_engine="$3"
  local expected_topology="$4"
  jq -e \
    --arg candidate "$expected_candidate" \
    --arg engine "$expected_engine" \
    --arg topology "$expected_topology" '
    def nonblank: type == "string" and (length > 0);
    def nonnegative_number: type == "number" and . >= 0;
    def positive_number: type == "number" and . > 0;
    def lowercase_sha256: type == "string" and test("^[0-9a-f]{64}$");
    def absolute: if . < 0 then -. else . end;
    def close_to($actual; $expected):
      (($actual - $expected) | absolute)
      <= ([($actual | absolute), ($expected | absolute), 1] | max) * 0.000000001;
    def attributed_seconds:
      .round_trip.storage_initialization_wall_clock_seconds
      + .round_trip.fact_preparation_wall_clock_seconds
      + .round_trip.fact_persistence_wall_clock_seconds
      + .round_trip.index_construction_wall_clock_seconds
      + .round_trip.storage_optimization_wall_clock_seconds
      + .round_trip.validation_wall_clock_seconds
      + .round_trip.publication_wall_clock_seconds
      + .round_trip.fresh_reader_validation_wall_clock_seconds
      + .round_trip.storage_measurement_wall_clock_seconds;
    def immutable_image:
      type == "string"
      and test("(^sha256:|@sha256:)[0-9A-Fa-f]{64}$")
      and ((capture("sha256:(?<digest>[0-9A-Fa-f]{64})$").digest | test("^0+$")) | not);
    .report_format_version == 3
    and .measurement_kind == "canonical-block-facts-round-trip"
    and .storage_candidate.id == $candidate
    and .storage_candidate.canonical_engine == $engine
    and .storage_candidate.canonical_model == "block-granular-canonical-facts"
    and .storage_candidate.diagnostic_projection_engine == null
    and .storage_candidate.topology == $topology
    and .round_trip.scope == "canonical-block-facts-fixture-round-trip"
    and .round_trip.fixture_sequence_digest_match == true
    and (.fixture.fixture_format_version > 0)
    and (.fixture.current_schema_oracle_artifact_schema_version > 0)
    and (.fixture.digest_sha256 | lowercase_sha256)
    and (.fixture.canonical_block_facts_digest_evidence.block_digest_version > 0)
    and (.fixture.canonical_block_facts_digest_evidence.sequence_digest_version > 0)
    and (.fixture.canonical_block_facts_digest_evidence.sequence_digest_sha256 | lowercase_sha256)
    and (.fixture.tip_hash_hex | lowercase_sha256)
    and (.fixture.network | nonblank)
    and (.fixture.block_count > 0)
    and (.fixture.canonical_block_facts_digest_evidence.block_count == .fixture.block_count)
    and (.fixture.to_height >= .fixture.from_height)
    and ((.fixture.to_height - .fixture.from_height + 1) == .fixture.block_count)
    and (.fixture.segment_count > 0)
    and (.round_trip.block_count == .fixture.block_count)
    and (.round_trip.first_height == .fixture.from_height)
    and (.round_trip.first_hash_hex | lowercase_sha256)
    and (.round_trip.tip_height == .fixture.to_height)
    and (.round_trip.tip_hash_hex == .fixture.tip_hash_hex)
    and (.round_trip.persisted_sequence_digest.block_count == .round_trip.block_count)
    and (
      .round_trip.persisted_sequence_digest.block_digest_version
      == .fixture.canonical_block_facts_digest_evidence.block_digest_version
    )
    and (
      .round_trip.persisted_sequence_digest.sequence_digest_version
      == .fixture.canonical_block_facts_digest_evidence.sequence_digest_version
    )
    and (.round_trip.persisted_sequence_digest.sha256 == .fixture.canonical_block_facts_digest_evidence.sequence_digest_sha256)
    and (.round_trip.wall_clock_seconds | positive_number)
    and (.round_trip.blocks_per_second | positive_number)
    and close_to(
      .round_trip.blocks_per_second;
      (.round_trip.block_count / .round_trip.wall_clock_seconds)
    )
    and (.round_trip.storage_initialization_wall_clock_seconds | nonnegative_number)
    and (.round_trip.fact_preparation_wall_clock_seconds | nonnegative_number)
    and (.round_trip.fact_persistence_wall_clock_seconds | nonnegative_number)
    and (.round_trip.index_construction_wall_clock_seconds | nonnegative_number)
    and (.round_trip.storage_optimization_wall_clock_seconds | nonnegative_number)
    and (.round_trip.validation_wall_clock_seconds | nonnegative_number)
    and (.round_trip.publication_wall_clock_seconds | nonnegative_number)
    and (.round_trip.fresh_reader_validation_wall_clock_seconds | nonnegative_number)
    and (.round_trip.storage_measurement_wall_clock_seconds | nonnegative_number)
    and (.round_trip.unattributed_wall_clock_seconds | nonnegative_number)
    and close_to(
      (attributed_seconds + .round_trip.unattributed_wall_clock_seconds);
      .round_trip.wall_clock_seconds
    )
    and (.round_trip.logical_fact_bytes | positive_number)
    and (.round_trip.physical_storage_bytes | positive_number)
    and (.round_trip.block_prepare_concurrency > 0)
    and (.round_trip.benchmark_client_peak_rss.source | nonblank)
    and (
      .round_trip.benchmark_client_peak_rss.bytes == null
      or (.round_trip.benchmark_client_peak_rss.bytes | positive_number)
    )
    and (.provenance.benchmark_version | nonblank)
    and (.provenance.software_revision | nonblank)
    and (.provenance.image_reference | immutable_image)
    and (.provenance.run.trial_id | nonblank)
    and (.provenance.run.trial_id | test("^[A-Za-z0-9][A-Za-z0-9._-]*$"))
    and (.provenance.run.fixture_cache_policy == "warm" or .provenance.run.fixture_cache_policy == "cold")
    and (.provenance.run.started_at_unix_millis | positive_number)
    and (.provenance.run.completed_at_unix_millis | positive_number)
    and (.provenance.run.completed_at_unix_millis >= .provenance.run.started_at_unix_millis)
    and (.provenance.runner.id | nonblank)
    and (.provenance.runner.cpu_limit_cores | positive_number)
    and (.provenance.runner.memory_limit_bytes | positive_number)
    and (.provenance.runner.storage_class | nonblank)
    and (.provenance.target_os | nonblank)
    and (.provenance.target_arch | nonblank)
  ' "$report_path" >/dev/null || fail "$report_path is not a complete $expected_candidate report"

  if [[ "$expected_candidate" == "rocksdb-fact-first" ]]; then
    jq -e '
      def nonblank: type == "string" and (length > 0);
      .round_trip.storage.engine == "rocksdb"
      and (.round_trip.storage.storage_schema_version > 0)
      and .round_trip.storage.ingestion_mode == "sorted-external-sst"
      and (.round_trip.storage.durability_mode | nonblank)
      and (.round_trip.storage.database_io_mode | nonblank)
      and (.round_trip.storage.external_sst_io_mode | nonblank)
      and (.round_trip.storage.compression | nonblank)
      and (.round_trip.storage.external_sst_bytes > 0)
      and (.round_trip.physical_storage_bytes >= .round_trip.storage.external_sst_bytes)
      and (.round_trip.storage.rocksdb_resource_budget.block_cache_bytes > 0)
      and (.round_trip.storage.rocksdb_resource_budget.max_wal_bytes > 0)
      and (.round_trip.storage.rocksdb_resource_budget.max_open_files > 0)
      and (.round_trip.storage.rocksdb_resource_budget.write_buffer_bytes > 0)
      and (.round_trip.storage.rocksdb_resource_budget.max_write_buffer_count > 0)
      and (.round_trip.storage.rocksdb_resource_budget.max_background_jobs > 0)
      and (.round_trip.storage.rocksdb_resource_budget.memtable_budget_bytes > 0)
      and (.round_trip.storage.rocksdb_resource_budget.statistics_level | nonblank)
    ' "$report_path" >/dev/null || fail "$report_path lacks complete RocksDB candidate evidence"
  else
    jq -e '
      def nonblank: type == "string" and (length > 0);
      def positive_number: type == "number" and . > 0;
      def absolute: if . < 0 then -. else . end;
      def close_to($actual; $expected):
        (($actual - $expected) | absolute)
        <= ([($actual | absolute), ($expected | absolute), 1] | max) * 0.000000001;
      def immutable_image:
        type == "string"
        and test("(^sha256:|@sha256:)[0-9A-Fa-f]{64}$")
        and ((capture("sha256:(?<digest>[0-9A-Fa-f]{64})$").digest | test("^0+$")) | not);
      .round_trip.storage.engine == "postgres"
      and (.round_trip.storage.storage_schema_version > 0)
      and .round_trip.storage.ingestion_mode == "binary-copy-single-load-transaction-with-deferred-index"
      and .round_trip.storage.tables_logged == true
      and .round_trip.storage.reference_encoding_compression == "lz4"
      and (.round_trip.storage.fact_table_bytes | positive_number)
      and (.round_trip.storage.index_bytes | positive_number)
      and (.round_trip.storage.wal_bytes | positive_number)
      and (
        .round_trip.physical_storage_bytes
        >= (.round_trip.storage.fact_table_bytes + .round_trip.storage.index_bytes)
      )
      and (.round_trip.storage.server_settings.server_version | nonblank)
      and (.round_trip.storage.server_settings.server_version_number > 0)
      and (.round_trip.storage.server_settings.max_connections > 0)
      and (.round_trip.storage.server_settings.shared_buffers_bytes > 0)
      and (.round_trip.storage.server_settings.effective_cache_size_bytes > 0)
      and (.round_trip.storage.server_settings.maintenance_work_mem_bytes > 0)
      and (.round_trip.storage.server_settings.work_mem_bytes > 0)
      and (.round_trip.storage.server_settings.max_wal_size_bytes > 0)
      and (.round_trip.storage.server_settings.min_wal_size_bytes > 0)
      and (.round_trip.storage.server_settings.checkpoint_timeout_seconds > 0)
      and (.round_trip.storage.server_settings.checkpoint_completion_target | positive_number)
      and (.round_trip.storage.server_settings.wal_compression | nonblank)
      and .round_trip.storage.server_settings.password_encryption_default == "scram-sha-256"
      and (.round_trip.storage.server_settings.max_worker_processes > 0)
      and (.round_trip.storage.server_settings.max_parallel_workers > 0)
      and (.round_trip.storage.server_settings.max_parallel_maintenance_workers > 0)
      and .round_trip.storage.server_settings.track_io_timing == true
      and (.round_trip.storage.server_settings.huge_pages | nonblank)
      and .round_trip.storage.server_settings.fsync == true
      and .round_trip.storage.server_settings.full_page_writes == true
      and .round_trip.storage.server_settings.synchronous_commit == "on"
      and (.round_trip.storage.server_settings.wal_level | nonblank)
      and .round_trip.storage.server_settings.data_checksums == true
      and (.round_trip.storage.benchmark_runtime.database_image_reference | immutable_image)
      and (.round_trip.storage.benchmark_runtime.client_cpu_limit_cores | positive_number)
      and (.round_trip.storage.benchmark_runtime.client_memory_limit_bytes | positive_number)
      and (.round_trip.storage.benchmark_runtime.database_cpu_limit_cores | positive_number)
      and (.round_trip.storage.benchmark_runtime.database_memory_limit_bytes | positive_number)
      and close_to(
        .round_trip.storage.benchmark_runtime.client_cpu_limit_cores
        + .round_trip.storage.benchmark_runtime.database_cpu_limit_cores;
        .provenance.runner.cpu_limit_cores
      )
      and (
        .round_trip.storage.benchmark_runtime.client_memory_limit_bytes
        + .round_trip.storage.benchmark_runtime.database_memory_limit_bytes
        == .provenance.runner.memory_limit_bytes
      )
    ' "$report_path" >/dev/null || fail "$report_path lacks complete PostgreSQL candidate evidence"
  fi
}

report_fingerprint() {
  jq -cS '[
    .fixture.digest_sha256,
    .fixture.canonical_block_facts_digest_evidence.sequence_digest_sha256,
    .fixture.block_count,
    .provenance.software_revision,
    .provenance.image_reference,
    .provenance.run.fixture_cache_policy,
    .provenance.runner.id,
    .provenance.runner.cpu_limit_cores,
    .provenance.runner.memory_limit_bytes,
    .provenance.runner.storage_class,
    .round_trip.block_prepare_concurrency
  ]' "$1"
}

report_configuration() {
  jq -cS '
    if .round_trip.storage.engine == "rocksdb" then
      (.round_trip.storage | del(.external_sst_bytes))
    elif .round_trip.storage.engine == "postgres" then
      (.round_trip.storage | del(.fact_table_bytes, .index_bytes, .wal_bytes))
    else
      error("unsupported storage engine")
    end
  ' "$1"
}

line_number=1
while IFS=$'\t' read -r rocksdb_report postgres_report extra; do
  line_number=$((line_number + 1))
  if [[ -z "${rocksdb_report}${postgres_report}${extra}" ]]; then
    continue
  fi
  [[ -z "$extra" ]] || fail "ledger line $line_number has more than two columns"
  [[ -n "$rocksdb_report" && -n "$postgres_report" ]] \
    || fail "ledger line $line_number has an empty required column"

  rocksdb_path="$(resolve_report_path "$rocksdb_report")"
  postgres_path="$(resolve_report_path "$postgres_report")"
  validate_report "$rocksdb_path" "rocksdb-fact-first" "rocksdb" "rocksdb-single-host"
  validate_report "$postgres_path" "postgres-fact-first" "postgres" "postgres-scale-out"

  rocksdb_trial_id="$(jq -er '.provenance.run.trial_id' "$rocksdb_path")"
  postgres_trial_id="$(jq -er '.provenance.run.trial_id' "$postgres_path")"
  [[ "$rocksdb_trial_id" == "$postgres_trial_id" ]] \
    || fail "paired report trial IDs differ on line $line_number"
  [[ "$(basename -- "$rocksdb_path")" == *"$rocksdb_trial_id"* ]] \
    || fail "RocksDB report name on line $line_number does not contain trial ID $rocksdb_trial_id"
  [[ "$(basename -- "$postgres_path")" == *"$rocksdb_trial_id"* ]] \
    || fail "PostgreSQL report name on line $line_number does not contain trial ID $rocksdb_trial_id"

  rocksdb_report_sha256="$(report_sha256 "$rocksdb_path")"
  postgres_report_sha256="$(report_sha256 "$postgres_path")"
  trial_evidence="$(jq -cen \
    --slurpfile rocksdb "$rocksdb_path" \
    --slurpfile postgres "$postgres_path" \
    --arg rocksdb_path "$rocksdb_path" \
    --arg postgres_path "$postgres_path" \
    --arg rocksdb_sha256 "$rocksdb_report_sha256" \
    --arg postgres_sha256 "$postgres_report_sha256" '
    ($rocksdb[0].provenance.run) as $r
    | ($postgres[0].provenance.run) as $p
    | {
        rocksdb: {
          candidate: $rocksdb[0].storage_candidate.id,
          report_path: $rocksdb_path,
          report_sha256: $rocksdb_sha256,
          wall_clock_seconds: $rocksdb[0].round_trip.wall_clock_seconds,
          blocks_per_second: $rocksdb[0].round_trip.blocks_per_second,
          logical_fact_bytes: $rocksdb[0].round_trip.logical_fact_bytes,
          physical_storage_bytes: $rocksdb[0].round_trip.physical_storage_bytes
        },
        postgres: {
          candidate: $postgres[0].storage_candidate.id,
          report_path: $postgres_path,
          report_sha256: $postgres_sha256,
          wall_clock_seconds: $postgres[0].round_trip.wall_clock_seconds,
          blocks_per_second: $postgres[0].round_trip.blocks_per_second,
          logical_fact_bytes: $postgres[0].round_trip.logical_fact_bytes,
          physical_storage_bytes: $postgres[0].round_trip.physical_storage_bytes
        }
      } as $arms
    | if $r.started_at_unix_millis == $p.started_at_unix_millis then
        error("paired arms have identical start timestamps")
      elif $r.started_at_unix_millis < $p.started_at_unix_millis then
        {
          trial_id: $r.trial_id,
          arm_order: "rocksdb-first",
          started_at_unix_millis: $r.started_at_unix_millis,
          completed_at_unix_millis: $p.completed_at_unix_millis,
          non_overlapping: ($r.completed_at_unix_millis <= $p.started_at_unix_millis),
          arms: $arms
        }
      else
        {
          trial_id: $r.trial_id,
          arm_order: "postgres-first",
          started_at_unix_millis: $p.started_at_unix_millis,
          completed_at_unix_millis: $r.completed_at_unix_millis,
          non_overlapping: ($p.completed_at_unix_millis <= $r.started_at_unix_millis),
          arms: $arms
        }
      end
  ')" || fail "could not derive arm order on line $line_number"
  [[ "$(jq -r '.non_overlapping' <<<"$trial_evidence")" == "true" ]] \
    || fail "paired arms overlap on line $line_number"
  rocksdb_fingerprint="$(report_fingerprint "$rocksdb_path")"
  postgres_fingerprint="$(report_fingerprint "$postgres_path")"
  [[ "$rocksdb_fingerprint" == "$postgres_fingerprint" ]] \
    || fail "paired report provenance differs on line $line_number"
  if [[ -z "$common_fingerprint" ]]; then
    common_fingerprint="$rocksdb_fingerprint"
    campaign_cache_policy="$(jq -er '.provenance.run.fixture_cache_policy' "$rocksdb_path")"
    campaign_runner_id="$(jq -er '.provenance.runner.id' "$rocksdb_path")"
  else
    [[ "$rocksdb_fingerprint" == "$common_fingerprint" ]] \
      || fail "campaign provenance differs on line $line_number"
  fi

  current_rocksdb_configuration="$(report_configuration "$rocksdb_path")"
  current_postgres_configuration="$(report_configuration "$postgres_path")"
  if [[ -z "$rocksdb_configuration" ]]; then
    rocksdb_configuration="$current_rocksdb_configuration"
    postgres_configuration="$current_postgres_configuration"
  else
    [[ "$current_rocksdb_configuration" == "$rocksdb_configuration" ]] \
      || fail "RocksDB configuration differs on line $line_number"
    [[ "$current_postgres_configuration" == "$postgres_configuration" ]] \
      || fail "PostgreSQL configuration differs on line $line_number"
  fi

  printf '%s\n' "$rocksdb_trial_id" >>"$trial_ids_path"
  printf '%s\n%s\n' "$rocksdb_path" "$postgres_path" >>"$report_paths_path"
  printf '%s\n%s\n' "$rocksdb_report_sha256" "$postgres_report_sha256" >>"$report_hashes_path"
  jq -r '[.provenance.run.started_at_unix_millis, .provenance.run.completed_at_unix_millis][]' \
    "$rocksdb_path" "$postgres_path" >>"$run_timestamps_path"
  printf '%s\n' "$trial_evidence" >>"$trial_evidence_path"
  rocksdb_reports+=("$rocksdb_path")
  postgres_reports+=("$postgres_path")
  trial_count=$((trial_count + 1))
done < <(tail -n +2 "$ledger_path")

[[ "$trial_count" -ge 5 ]] || fail "at least five paired trials are required; found $trial_count"
unique_trial_count="$(sort -u "$trial_ids_path" | wc -l | tr -d ' ')"
[[ "$unique_trial_count" -eq "$trial_count" ]] || fail "report trial IDs must be unique"
unique_report_count="$(sort -u "$report_paths_path" | wc -l | tr -d ' ')"
[[ "$unique_report_count" -eq $((trial_count * 2)) ]] || fail "each ledger arm must reference a unique canonical report path"
unique_hash_count="$(sort -u "$report_hashes_path" | wc -l | tr -d ' ')"
[[ "$unique_hash_count" -eq $((trial_count * 2)) ]] || fail "copied or byte-identical reports cannot represent distinct runs"
unique_timestamp_count="$(sort -u "$run_timestamps_path" | wc -l | tr -d ' ')"
[[ "$unique_timestamp_count" -eq $((trial_count * 4)) ]] || fail "every run start and completion timestamp must be unique"
jq -s 'sort_by(.started_at_unix_millis)' "$trial_evidence_path" >"$chronological_trials_path"
jq -e '
  . as $trials
  | all(
      range(1; length);
      . as $index
      | $trials[$index - 1].completed_at_unix_millis
        <= $trials[$index].started_at_unix_millis
    )
' "$chronological_trials_path" >/dev/null \
  || fail "campaign trials overlap in chronological report order"
jq -e '
  . as $trials
  | all(
      range(1; length);
      . as $index
      | $trials[$index - 1].arm_order != $trials[$index].arm_order
    )
' "$chronological_trials_path" >/dev/null \
  || fail "arm order must alternate in chronological report order"

all_reports=("${rocksdb_reports[@]}" "${postgres_reports[@]}")
fixture_digest="$(jq -r '.fixture.canonical_block_facts_digest_evidence.sequence_digest_sha256' "${rocksdb_reports[0]}")"
jq -s \
  --slurpfile chronological_trials "$chronological_trials_path" \
  --arg cache_policy "$campaign_cache_policy" \
  --arg runner_id "$campaign_runner_id" \
  --arg fixture_digest "$fixture_digest" \
  --argjson trial_count "$trial_count" '
  def statistics:
    sort as $values
    | {
        minimum: $values[0],
        median: (
          if ($values | length) % 2 == 1
          then $values[(($values | length) / 2 | floor)]
          else (
            $values[(($values | length) / 2) - 1]
            + $values[(($values | length) / 2)]
          ) / 2
          end
        ),
        maximum: $values[-1]
      };
  {
    campaign: {
      trial_count: $trial_count,
      fixture_cache_policy: $cache_policy,
      runner_id: $runner_id,
      fixture_sequence_digest_sha256: $fixture_digest,
      trials: $chronological_trials[0]
    },
    candidates: (
      sort_by(.storage_candidate.id)
      | group_by(.storage_candidate.id)
      | map({
          candidate: .[0].storage_candidate.id,
          wall_clock_seconds: ([.[].round_trip.wall_clock_seconds] | statistics),
          blocks_per_second: ([.[].round_trip.blocks_per_second] | statistics),
          physical_storage_bytes: ([.[].round_trip.physical_storage_bytes] | statistics)
        })
    )
  }
' "${all_reports[@]}"
