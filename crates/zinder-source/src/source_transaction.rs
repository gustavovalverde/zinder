//! Node-sourced transaction values.
//!
//! Helpers that translate raw serialized Zcash transaction bytes into
//! Zinder vocabulary. Mirrors the [`crate::source_block`] pattern: the
//! upstream Zebra type is parsed once at the boundary and the typed
//! Zinder shape is the public return.

use zebra_chain::{
    parameters::NetworkUpgrade as ZebraNetworkUpgrade,
    serialization::ZcashDeserializeInto,
    transaction::{
        AuthDigest as ZebraAuthDigest, Transaction as ZebraTransaction, WtxId as ZebraWtxId,
    },
};
use zinder_core::{
    AuthDigest, BlockHeight, ConsensusBranchId, LockTime, NetworkUpgradeActivations,
    TransactionComponentCounts, TransactionId, TransactionPublicFacts, TransactionVersion,
    UnsupportedSection, Wtxid, classify_privacy_shape,
};

use crate::SourceError;

/// Parses every public transaction fact the explorer plane renders.
///
/// Single source of truth for transaction parsing across ingest, mempool
/// hydration, and the explorer read path per
/// [ADR-0010](../../../docs/adrs/0010-transaction-public-facts.md). The
/// `activations` table resolves the consensus branch ID for v3/v4
/// transactions whose header omits it; v5+ transactions carry the branch ID
/// directly through `Transaction::network_upgrade()`.
///
/// The parser delegates byte-level decoding to `zebra-chain` and never
/// re-derives the txid: it trusts `Transaction::hash()` which routes
/// `SHA256d` for pre-v5 and `BLAKE2b` ZIP-244 `txid_digest` for v5+.
pub fn parse_transaction_public_facts(
    raw_transaction_bytes: &[u8],
    mined_height: Option<BlockHeight>,
    activations: &NetworkUpgradeActivations,
) -> Result<TransactionPublicFacts, SourceError> {
    let transaction: ZebraTransaction =
        raw_transaction_bytes
            .zcash_deserialize_into()
            .map_err(|source| SourceError::RawTransactionParseFailed {
                reason: source.to_string(),
            })?;
    let version = classify_transaction_version(&transaction);
    let counts = transaction_component_counts(&transaction);
    let is_coinbase = transaction.is_coinbase();
    let unsupported_sections = if version.is_supported() {
        Vec::new()
    } else {
        vec![UnsupportedSection::FutureVersionHeader]
    };
    let consensus_branch_id = resolve_consensus_branch_id(&transaction, mined_height, activations);
    let auth_digest = extract_auth_digest(&transaction);
    let wtxid = extract_wtxid(&transaction);
    let lock_time = extract_lock_time(&transaction);
    let expiry_height = extract_expiry_height(&transaction);
    let size_bytes = u32::try_from(raw_transaction_bytes.len()).unwrap_or(u32::MAX);
    let transaction_id = TransactionId::from_bytes(transaction.hash().0);
    let privacy_shape = classify_privacy_shape(counts, is_coinbase, version);

    Ok(TransactionPublicFacts {
        transaction_id,
        auth_digest,
        wtxid,
        version,
        consensus_branch_id,
        lock_time,
        expiry_height,
        size_bytes,
        counts,
        privacy_shape,
        is_coinbase,
        unsupported_sections,
    })
}

fn classify_transaction_version(transaction: &ZebraTransaction) -> TransactionVersion {
    match transaction {
        ZebraTransaction::V1 { .. } => TransactionVersion::V1,
        ZebraTransaction::V2 { .. } => TransactionVersion::V2,
        ZebraTransaction::V3 { .. } => TransactionVersion::V3,
        ZebraTransaction::V4 { .. } => TransactionVersion::V4,
        ZebraTransaction::V5 { .. } => TransactionVersion::V5,
        ZebraTransaction::V6 { .. } => TransactionVersion::V6,
    }
}

/// Counts the transparent / shielded components of a parsed Zcash transaction.
///
/// This is the canonical way to populate [`TransactionComponentCounts`] from
/// a `zebra-chain` transaction; every explorer-plane and source-plane caller
/// goes through this function so the counting rules stay in one place.
///
/// Each count is saturated at `u32::MAX` rather than overflowing; a
/// transaction with more than four billion of any component would not
/// validate, so the cap is defensive rather than load-bearing.
#[must_use]
pub fn transaction_component_counts(transaction: &ZebraTransaction) -> TransactionComponentCounts {
    let transparent_input_count = u32::try_from(transaction.inputs().len()).unwrap_or(u32::MAX);
    let transparent_output_count = u32::try_from(transaction.outputs().len()).unwrap_or(u32::MAX);
    let sapling_spend_count =
        u32::try_from(transaction.sapling_spends_per_anchor().count()).unwrap_or(u32::MAX);
    let sapling_output_count =
        u32::try_from(transaction.sapling_outputs().count()).unwrap_or(u32::MAX);
    let orchard_action_count =
        u32::try_from(transaction.orchard_actions().count()).unwrap_or(u32::MAX);
    let ironwood_action_count =
        u32::try_from(transaction.ironwood_actions().count()).unwrap_or(u32::MAX);
    let sprout_joinsplit_count = u32::try_from(transaction.joinsplit_count()).unwrap_or(u32::MAX);
    TransactionComponentCounts {
        transparent_input_count,
        transparent_output_count,
        sapling_spend_count,
        sapling_output_count,
        orchard_action_count,
        ironwood_action_count,
        sprout_joinsplit_count,
    }
}

fn extract_auth_digest(transaction: &ZebraTransaction) -> Option<AuthDigest> {
    transaction
        .auth_digest()
        .map(|digest: ZebraAuthDigest| AuthDigest::from_bytes(digest.0))
}

fn extract_wtxid(transaction: &ZebraTransaction) -> Option<Wtxid> {
    transaction.network_upgrade()?;
    let zebra_wtxid: ZebraWtxId = ZebraWtxId::from(transaction);
    Some(Wtxid::from_bytes(zebra_wtxid.as_bytes()))
}

fn extract_lock_time(transaction: &ZebraTransaction) -> LockTime {
    use zebra_chain::transaction::LockTime as ZebraLockTime;
    match transaction.lock_time() {
        Some(ZebraLockTime::Height(height)) => LockTime::Height(BlockHeight::new(height.0)),
        Some(ZebraLockTime::Time(timestamp)) => {
            LockTime::UnixSeconds(u64::try_from(timestamp.timestamp()).unwrap_or(0))
        }
        None => LockTime::Unlocked,
    }
}

fn extract_expiry_height(transaction: &ZebraTransaction) -> Option<BlockHeight> {
    transaction
        .expiry_height()
        .map(|height| BlockHeight::new(height.0))
}

fn resolve_consensus_branch_id(
    transaction: &ZebraTransaction,
    mined_height: Option<BlockHeight>,
    activations: &NetworkUpgradeActivations,
) -> Option<ConsensusBranchId> {
    if let Some(network_upgrade) = transaction.network_upgrade() {
        return zebra_network_upgrade_branch_id(network_upgrade);
    }
    match transaction {
        ZebraTransaction::V1 { .. } | ZebraTransaction::V2 { .. } => None,
        ZebraTransaction::V3 { .. }
        | ZebraTransaction::V4 { .. }
        | ZebraTransaction::V5 { .. }
        | ZebraTransaction::V6 { .. } => {
            mined_height.map(|height| activations.consensus_branch_id_at(height))
        }
    }
}

fn zebra_network_upgrade_branch_id(
    network_upgrade: ZebraNetworkUpgrade,
) -> Option<ConsensusBranchId> {
    network_upgrade
        .branch_id()
        .map(|branch_id| ConsensusBranchId::new(branch_id.into()))
}
