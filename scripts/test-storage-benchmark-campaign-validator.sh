#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
validator="$repository_root/scripts/validate-storage-benchmark-campaign.sh"
scratch_directory="$(mktemp -d)"
trap 'rm -rf "$scratch_directory"' EXIT

fail() {
  echo >&2 "storage benchmark campaign validator test failed: $*"
  exit 1
}

write_report() {
  local candidate="$1"
  local trial_id="$2"
  local started_at="$3"
  local completed_at="$4"
  local report_path="$5"
  jq -n \
    --arg candidate "$candidate" \
    --arg trial_id "$trial_id" \
    --argjson started_at "$started_at" \
    --argjson completed_at "$completed_at" '
    def rocksdb_storage: {
      engine: "rocksdb",
      storage_schema_version: 1,
      ingestion_mode: "sorted-external-sst",
      durability_mode: "external-sst-ingest-with-synchronous-completion-marker",
      database_io_mode: "buffered",
      external_sst_io_mode: "buffered",
      compression: "snappy",
      external_sst_bytes: 7000,
      rocksdb_resource_budget: {
        block_cache_bytes: 67108864,
        max_wal_bytes: 33554432,
        max_open_files: 128,
        write_buffer_bytes: 8388608,
        max_write_buffer_count: 2,
        max_background_jobs: 2,
        memtable_budget_bytes: 16777216,
        statistics_level: "tickers"
      }
    };
    def postgres_storage: {
      engine: "postgres",
      storage_schema_version: 1,
      ingestion_mode: "binary-copy-single-load-transaction-with-deferred-index",
      tables_logged: true,
      reference_encoding_compression: "lz4",
      fact_table_bytes: 6000,
      index_bytes: 1000,
      wal_bytes: 9000,
      server_settings: {
        server_version: "18.4",
        server_version_number: 180004,
        max_connections: 50,
        shared_buffers_bytes: 2147483648,
        effective_cache_size_bytes: 6442450944,
        maintenance_work_mem_bytes: 1073741824,
        work_mem_bytes: 67108864,
        max_wal_size_bytes: 17179869184,
        min_wal_size_bytes: 2147483648,
        checkpoint_timeout_seconds: 900,
        checkpoint_completion_target: 0.9,
        wal_compression: "on",
        password_encryption_default: "scram-sha-256",
        max_worker_processes: 6,
        max_parallel_workers: 6,
        max_parallel_maintenance_workers: 4,
        track_io_timing: true,
        huge_pages: "try",
        fsync: true,
        full_page_writes: true,
        synchronous_commit: "on",
        wal_level: "replica",
        data_checksums: true
      },
      benchmark_runtime: {
        database_image_reference: ("postgres@sha256:" + ("b" * 64)),
        client_cpu_limit_cores: 2,
        client_memory_limit_bytes: 8589934592,
        database_cpu_limit_cores: 6,
        database_memory_limit_bytes: 8589934592
      }
    };
    ($candidate == "rocksdb-fact-first") as $is_rocksdb
    | {
        report_format_version: 3,
        measurement_kind: "canonical-block-facts-round-trip",
        provenance: {
          benchmark_version: "0.1.0",
          software_revision: "0123456789abcdef",
          run: {
            trial_id: $trial_id,
            fixture_cache_policy: "warm",
            started_at_unix_millis: $started_at,
            completed_at_unix_millis: $completed_at
          },
          runner: {
            id: "linux-amd64-c8-m16-nvme-01",
            cpu_limit_cores: 8,
            memory_limit_bytes: 17179869184,
            storage_class: "local-nvme"
          },
          image_reference: ("sha256:" + ("a" * 64)),
          target_os: "linux",
          target_arch: "x86_64"
        },
        fixture: {
          fixture_format_version: 3,
          current_schema_oracle_artifact_schema_version: 18,
          canonical_block_facts_digest_evidence: {
            block_digest_version: 1,
            sequence_digest_version: 1,
            block_count: 10,
            sequence_digest_sha256: ("c" * 64)
          },
          tip_hash_hex: ("d" * 64),
          digest_sha256: ("e" * 64),
          network: "zcash-regtest",
          from_height: 1,
          to_height: 10,
          block_count: 10,
          workload_density: {block_count: 10},
          segment_count: 1
        },
        storage_candidate: {
          id: $candidate,
          canonical_engine: (if $is_rocksdb then "rocksdb" else "postgres" end),
          canonical_model: "block-granular-canonical-facts",
          diagnostic_projection_engine: null,
          topology: (if $is_rocksdb then "rocksdb-single-host" else "postgres-scale-out" end)
        },
        round_trip: {
          scope: "canonical-block-facts-fixture-round-trip",
          block_prepare_concurrency: 16,
          wall_clock_seconds: (if $is_rocksdb then 10 else 8 end),
          storage_initialization_wall_clock_seconds: 0.5,
          fact_preparation_wall_clock_seconds: 1,
          fact_persistence_wall_clock_seconds: 2,
          index_construction_wall_clock_seconds: 1,
          storage_optimization_wall_clock_seconds: 0.5,
          validation_wall_clock_seconds: 1,
          publication_wall_clock_seconds: 0.5,
          fresh_reader_validation_wall_clock_seconds: 0.5,
          storage_measurement_wall_clock_seconds: 0.5,
          unattributed_wall_clock_seconds: (if $is_rocksdb then 2.5 else 0.5 end),
          first_height: 1,
          first_hash_hex: ("f" * 64),
          tip_height: 10,
          tip_hash_hex: ("d" * 64),
          block_count: 10,
          blocks_per_second: (if $is_rocksdb then 1 else 1.25 end),
          logical_fact_bytes: 4096,
          physical_storage_bytes: 8192,
          persisted_sequence_digest: {
            block_digest_version: 1,
            sequence_digest_version: 1,
            block_count: 10,
            sha256: ("c" * 64)
          },
          fixture_sequence_digest_match: true,
          storage: (if $is_rocksdb then rocksdb_storage else postgres_storage end),
          benchmark_client_peak_rss: {bytes: null, source: "unavailable"}
        }
      }
  ' >"$report_path"
}

create_valid_campaign() {
  local campaign_directory="$1"
  mkdir -p "$campaign_directory"
  printf 'rocksdb_report\tpostgres_report\n' >"$campaign_directory/campaign.tsv"
  local trial_number
  for trial_number in 1 2 3 4 5; do
    local trial_id
    local base_timestamp
    local rocksdb_started
    local rocksdb_completed
    local postgres_started
    local postgres_completed
    trial_id="$(printf 'trial-%02d' "$trial_number")"
    base_timestamp=$((100000 + trial_number * 10000))
    if ((trial_number % 2 == 1)); then
      rocksdb_started=$base_timestamp
      rocksdb_completed=$((base_timestamp + 1000))
      postgres_started=$((base_timestamp + 2000))
      postgres_completed=$((base_timestamp + 3000))
    else
      postgres_started=$base_timestamp
      postgres_completed=$((base_timestamp + 1000))
      rocksdb_started=$((base_timestamp + 2000))
      rocksdb_completed=$((base_timestamp + 3000))
    fi
    write_report \
      "rocksdb-fact-first" \
      "$trial_id" \
      "$rocksdb_started" \
      "$rocksdb_completed" \
      "$campaign_directory/rocksdb-fact-first-$trial_id.json"
    write_report \
      "postgres-fact-first" \
      "$trial_id" \
      "$postgres_started" \
      "$postgres_completed" \
      "$campaign_directory/postgres-fact-first-$trial_id.json"
    printf 'rocksdb-fact-first-%s.json\tpostgres-fact-first-%s.json\n' \
      "$trial_id" \
      "$trial_id" >>"$campaign_directory/campaign.tsv"
  done
}

mutate_report() {
  local report_path="$1"
  local jq_filter="$2"
  local replacement_path="$report_path.replacement"
  jq "$jq_filter" "$report_path" >"$replacement_path"
  mv "$replacement_path" "$report_path"
}

expect_failure() {
  local case_name="$1"
  local campaign_directory="$2"
  if "$validator" "$campaign_directory/campaign.tsv" >/dev/null 2>&1; then
    fail "$case_name was accepted"
  fi
}

valid_campaign="$scratch_directory/valid"
create_valid_campaign "$valid_campaign"
mutate_report \
  "$valid_campaign/rocksdb-fact-first-trial-02.json" \
  '.round_trip.storage.external_sst_bytes = 7100'
mutate_report \
  "$valid_campaign/postgres-fact-first-trial-02.json" \
  '.round_trip.storage.fact_table_bytes = 6100
   | .round_trip.storage.index_bytes = 1100
   | .round_trip.storage.wal_bytes = 9100'
valid_summary="$($validator "$valid_campaign/campaign.tsv")"
[[ "$(jq -r '.campaign.trial_count' <<<"$valid_summary")" == "5" ]] \
  || fail "valid campaign did not report five trials"
[[ "$(jq -r '.campaign.trials | map(.arm_order) | join(",")' <<<"$valid_summary")" == "rocksdb-first,postgres-first,rocksdb-first,postgres-first,rocksdb-first" ]] \
  || fail "valid campaign did not preserve derived alternating order"
[[ "$(jq -r '.campaign.trials[0].arms.rocksdb.report_sha256 | test("^[0-9a-f]{64}$")' <<<"$valid_summary")" == "true" ]] \
  || fail "valid campaign did not identify the RocksDB report hash"
[[ "$(jq -r '.campaign.trials[0].arms.postgres.report_path | endswith("postgres-fact-first-trial-01.json")' <<<"$valid_summary")" == "true" ]] \
  || fail "valid campaign did not identify the PostgreSQL report path"

fractional_cpu_campaign="$scratch_directory/fractional-cpu"
cp -R "$valid_campaign" "$fractional_cpu_campaign"
for report_path in "$fractional_cpu_campaign"/*.json; do
  mutate_report "$report_path" '.provenance.runner.cpu_limit_cores = 0.3'
done
for report_path in "$fractional_cpu_campaign"/postgres-fact-first-*.json; do
  mutate_report \
    "$report_path" \
    '.round_trip.storage.benchmark_runtime.client_cpu_limit_cores = 0.1
     | .round_trip.storage.benchmark_runtime.database_cpu_limit_cores = 0.2'
done
"$validator" "$fractional_cpu_campaign/campaign.tsv" >/dev/null \
  || fail "valid fractional PostgreSQL CPU partition was rejected"

copied_campaign="$scratch_directory/copied"
cp -R "$valid_campaign" "$copied_campaign"
cp "$copied_campaign/rocksdb-fact-first-trial-01.json" \
  "$copied_campaign/rocksdb-fact-first-trial-05.json"
expect_failure "copied report" "$copied_campaign"

trial_mismatch_campaign="$scratch_directory/trial-mismatch"
cp -R "$valid_campaign" "$trial_mismatch_campaign"
mutate_report \
  "$trial_mismatch_campaign/postgres-fact-first-trial-03.json" \
  '.provenance.run.trial_id = "different-trial"'
expect_failure "paired trial mismatch" "$trial_mismatch_campaign"

wrong_version_campaign="$scratch_directory/wrong-version"
cp -R "$valid_campaign" "$wrong_version_campaign"
mutate_report "$wrong_version_campaign/rocksdb-fact-first-trial-02.json" '.report_format_version = 2'
expect_failure "wrong report version" "$wrong_version_campaign"

wrong_topology_campaign="$scratch_directory/wrong-topology"
cp -R "$valid_campaign" "$wrong_topology_campaign"
mutate_report \
  "$wrong_topology_campaign/postgres-fact-first-trial-04.json" \
  '.storage_candidate.topology = "rocksdb-single-host"'
expect_failure "wrong topology" "$wrong_topology_campaign"

null_configuration_campaign="$scratch_directory/null-configuration"
cp -R "$valid_campaign" "$null_configuration_campaign"
mutate_report \
  "$null_configuration_campaign/rocksdb-fact-first-trial-01.json" \
  '.round_trip.storage.rocksdb_resource_budget = null'
expect_failure "null RocksDB configuration" "$null_configuration_campaign"

zero_storage_campaign="$scratch_directory/zero-storage"
cp -R "$valid_campaign" "$zero_storage_campaign"
mutate_report \
  "$zero_storage_campaign/postgres-fact-first-trial-02.json" \
  '.round_trip.physical_storage_bytes = 0'
expect_failure "zero physical storage" "$zero_storage_campaign"

missing_database_image_campaign="$scratch_directory/missing-database-image"
cp -R "$valid_campaign" "$missing_database_image_campaign"
mutate_report \
  "$missing_database_image_campaign/postgres-fact-first-trial-05.json" \
  '.round_trip.storage.benchmark_runtime.database_image_reference = null'
expect_failure "missing PostgreSQL database image" "$missing_database_image_campaign"

overlapping_campaign="$scratch_directory/overlapping"
cp -R "$valid_campaign" "$overlapping_campaign"
mutate_report \
  "$overlapping_campaign/postgres-fact-first-trial-01.json" \
  '.provenance.run.started_at_unix_millis = 110500'
expect_failure "overlapping paired arms" "$overlapping_campaign"

cross_trial_overlap_campaign="$scratch_directory/cross-trial-overlap"
cp -R "$valid_campaign" "$cross_trial_overlap_campaign"
mutate_report \
  "$cross_trial_overlap_campaign/postgres-fact-first-trial-02.json" \
  '.provenance.run.started_at_unix_millis = 112500
   | .provenance.run.completed_at_unix_millis = 113500'
expect_failure "overlapping chronological trials" "$cross_trial_overlap_campaign"

chronological_order_campaign="$scratch_directory/chronological-order"
cp -R "$valid_campaign" "$chronological_order_campaign"
mutate_report \
  "$chronological_order_campaign/rocksdb-fact-first-trial-02.json" \
  '.provenance.run.started_at_unix_millis = 120000
   | .provenance.run.completed_at_unix_millis = 121000'
mutate_report \
  "$chronological_order_campaign/postgres-fact-first-trial-02.json" \
  '.provenance.run.started_at_unix_millis = 122000
   | .provenance.run.completed_at_unix_millis = 123000'
mutate_report \
  "$chronological_order_campaign/postgres-fact-first-trial-03.json" \
  '.provenance.run.started_at_unix_millis = 130000
   | .provenance.run.completed_at_unix_millis = 131000'
mutate_report \
  "$chronological_order_campaign/rocksdb-fact-first-trial-03.json" \
  '.provenance.run.started_at_unix_millis = 132000
   | .provenance.run.completed_at_unix_millis = 133000'
{
  printf 'rocksdb_report\tpostgres_report\n'
  for trial_number in 1 3 2 4 5; do
    trial_id="$(printf 'trial-%02d' "$trial_number")"
    printf 'rocksdb-fact-first-%s.json\tpostgres-fact-first-%s.json\n' \
      "$trial_id" \
      "$trial_id"
  done
} >"$chronological_order_campaign/campaign.tsv"
expect_failure "nonalternating chronological arm order" "$chronological_order_campaign"

missing_phase_campaign="$scratch_directory/missing-phase"
cp -R "$valid_campaign" "$missing_phase_campaign"
mutate_report \
  "$missing_phase_campaign/rocksdb-fact-first-trial-03.json" \
  'del(.round_trip.validation_wall_clock_seconds)'
expect_failure "missing phase timing" "$missing_phase_campaign"

wrong_range_campaign="$scratch_directory/wrong-range"
cp -R "$valid_campaign" "$wrong_range_campaign"
mutate_report \
  "$wrong_range_campaign/postgres-fact-first-trial-04.json" \
  '.round_trip.first_height = 2'
expect_failure "inconsistent persisted range" "$wrong_range_campaign"

wrong_digest_version_campaign="$scratch_directory/wrong-digest-version"
cp -R "$valid_campaign" "$wrong_digest_version_campaign"
mutate_report \
  "$wrong_digest_version_campaign/rocksdb-fact-first-trial-05.json" \
  '.round_trip.persisted_sequence_digest.sequence_digest_version = 2'
expect_failure "inconsistent digest version" "$wrong_digest_version_campaign"

wrong_throughput_campaign="$scratch_directory/wrong-throughput"
cp -R "$valid_campaign" "$wrong_throughput_campaign"
mutate_report \
  "$wrong_throughput_campaign/postgres-fact-first-trial-01.json" \
  '.round_trip.blocks_per_second = 99'
expect_failure "inconsistent throughput" "$wrong_throughput_campaign"

printf 'storage benchmark campaign validator tests passed\n'
