#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo >&2 "wallet lifecycle evidence is invalid: $*"
  exit 1
}

if [[ "$#" -ne 3 ]]; then
  echo >&2 "usage: $0 TOPOLOGY CERTIFICATION_REPORT RESTART_REPORT"
  exit 2
fi

command -v jq >/dev/null 2>&1 || fail "jq is required"

topology="$1"
certification_report="$2"
restart_report="$3"

case "$topology" in
  rocksdb-single-host | postgres-scale-out) ;;
  *) fail "unsupported topology: $topology" ;;
esac

[[ -f "$certification_report" ]] || fail "missing certification report: $certification_report"
[[ -f "$restart_report" ]] || fail "missing restart report: $restart_report"

jq -e --arg topology "$topology" '
  def nonblank: type == "string" and length > 0;
  def positive_integer: type == "number" and . > 0 and floor == .;
  def nonnegative_number: type == "number" and . >= 0;
  def lowercase_sha256: type == "string" and test("^[0-9a-f]{64}$");
  def phase($name): [.phases[] | select(.name == $name)] | if length == 1 then .[0] else null end;
  .contract_identity == "wallet-lifecycle-certification"
  and .report_format_version == 1
  and .result == "pass"
  and .topology == $topology
  and .network == "zcash-testnet"
  and .schema_identities.canonical == "canonical"
  and .schema_identities.wallet == "wallet"
  and .schema_versions.canonical == 1
  and .schema_versions.wallet == 1
  and (.process_instance_id | nonblank)
  and (.fixed_tip.height | positive_integer)
  and (.fixed_tip.hash_hex | lowercase_sha256)
  and (.fixed_tip.block_time_seconds | positive_integer)
  and (.ordered_sequence_digest_sha256 | lowercase_sha256)
  and ([.phases[].name] | sort == [
    "canonical_build",
    "canonical_publication",
    "fresh_reader_query_smoke",
    "source_authentication",
    "wallet_derivation"
  ])
  and ([.phases[].elapsed_seconds] | all(nonnegative_number))
  and (phase("canonical_build").block_count | positive_integer)
  and (phase("canonical_build").first_height | type == "number")
  and (phase("canonical_build").tip_height == .fixed_tip.height)
  and (phase("canonical_build").tip_hash_hex == .fixed_tip.hash_hex)
  and (phase("canonical_build").historical_prevout_read_count == 0)
  and (phase("canonical_build").logical_bytes | positive_integer)
  and (phase("canonical_build").persisted_bytes | positive_integer)
  and (phase("source_authentication").final_checkpoint_matches == true)
  and (phase("source_authentication").checkpoint_count | positive_integer)
  and (phase("source_authentication").checkpoint_maximum_gap_blocks <= 100)
  and (phase("source_authentication").subtree_root_ranges_complete == true)
  and (phase("source_authentication").active_pool_count | positive_integer)
  and (phase("source_authentication").subtree_root_count | positive_integer)
  and (phase("source_authentication").authenticated_tip_height == .fixed_tip.height)
  and (phase("source_authentication").authenticated_tip_hash_hex == .fixed_tip.hash_hex)
  and (phase("canonical_publication").state == "READY")
  and (phase("canonical_publication").epoch_id == 1)
  and (phase("canonical_publication").event_sequence == 1)
  and (phase("canonical_publication").ready_epoch_event_committed_atomically == true)
  and (phase("canonical_publication").created_record_count == 2)
  and (phase("canonical_publication").updated_control_record_count == 1)
  and (phase("canonical_publication").atomic_write_operation_count == 3)
  and (phase("wallet_derivation").covered_epoch_id == 1)
  and (phase("wallet_derivation").covered_tip_height == .fixed_tip.height)
  and (phase("wallet_derivation").block_count == phase("canonical_build").block_count)
  and (phase("wallet_derivation").transaction_count | positive_integer)
  and (phase("fresh_reader_query_smoke").fresh_reader == true)
  and (phase("fresh_reader_query_smoke").ready_epoch_id == 1)
  and (phase("fresh_reader_query_smoke").wallet_covered_epoch_id == 1)
  and (phase("fresh_reader_query_smoke").latest_block_height == .fixed_tip.height)
  and (phase("fresh_reader_query_smoke").request_count >= 5)
  and (
    [
      "latest_block",
      "compact_block_range",
      "transaction",
      "tree_state_checkpoint",
      "subtree_roots"
    ] - [phase("fresh_reader_query_smoke").successful_probes[]]
    | length == 0
  )
  and (.provenance.software_revision | nonblank)
  and (.provenance.image_reference | nonblank)
' "$certification_report" >/dev/null || fail "$certification_report does not prove the complete clean-v1 lifecycle"

jq -e --arg topology "$topology" --slurpfile certification "$certification_report" '
  def nonblank: type == "string" and length > 0;
  def positive_integer: type == "number" and . > 0 and floor == .;
  .contract_identity == "wallet-lifecycle-restart-certification"
  and .report_format_version == 1
  and .result == "pass"
  and .topology == $topology
  and .network == $certification[0].network
  and .schema_identities == $certification[0].schema_identities
  and .schema_versions == $certification[0].schema_versions
  and .fixed_tip == $certification[0].fixed_tip
  and .ordered_sequence_digest_sha256 == $certification[0].ordered_sequence_digest_sha256
  and .fresh_process == true
  and (.process_instance_id | nonblank)
  and .process_instance_id != $certification[0].process_instance_id
  and .canonical_ready_reopened == true
  and .wallet_coverage_reopened == true
  and .query_smoke_passed == true
  and (.ready_epoch_id == 1)
  and (.wallet_covered_epoch_id == 1)
  and (.latest_block_height == $certification[0].fixed_tip.height)
  and (.elapsed_seconds | type == "number" and . >= 0)
  and (
    if $topology == "postgres-scale-out" then
      .database_restart_observed == true
    else
      .database_restart_observed == false
    end
  )
' "$restart_report" >/dev/null || fail "$restart_report does not prove restart recovery"

echo "wallet lifecycle evidence is valid for $topology"
