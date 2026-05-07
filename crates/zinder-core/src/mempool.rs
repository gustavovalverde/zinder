//! Mempool entry vocabulary.
//!
//! These values describe a single hydrated mempool transaction as the
//! indexer observed it. They are produced by ingest after the source layer
//! delivers raw observations and the ingest layer stamps them with the chain
//! epoch visible at first observation.

use crate::{
    AuthDigest, ChainEpoch, RawTransactionBytes, TransactionId, TransparentOutPoint,
    UnixTimestampMillis, transparent_utxo::TransparentAddressScriptHash,
};

/// Hydrated record describing a mempool transaction observed by the indexer.
///
/// `MempoolEntry` is the canonical server-observed type. It is never used to
/// describe wallet-local pending state. Wallet-local "pending" can include
/// transactions the network never accepted; `MempoolEntry` only describes
/// transactions that an upstream node observed in its mempool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MempoolEntry {
    /// Transaction identifier reported by the source.
    pub transaction_id: TransactionId,
    /// ZIP-244 authorization digest, when the source provides one.
    ///
    /// Set for v5+ transactions; `None` for v1-v4 transactions where the
    /// txid alone authenticates witness data.
    pub auth_digest: Option<AuthDigest>,
    /// Raw serialized transaction bytes hydrated from the source.
    pub raw_transaction_bytes: RawTransactionBytes,
    /// Lightwalletd-compatible compact transaction bytes derived from the
    /// raw transaction.
    ///
    /// Pre-built so the lightwalletd compatibility adapter does not parse
    /// raw bytes on the read path.
    pub compact_transaction_bytes: Vec<u8>,
    /// Wall-clock time when the indexer first observed this mempool entry.
    pub first_seen_unix_millis: UnixTimestampMillis,
    /// Chain epoch visible to ingest when this mempool entry was first
    /// observed.
    pub first_seen_chain_epoch: ChainEpoch,
    /// Transparent outputs created by this mempool transaction, indexed for
    /// address lookups.
    pub transparent_outputs: Vec<TransparentMempoolOutput>,
    /// Transparent inputs that spend previously-known outpoints.
    pub transparent_spends: Vec<TransparentMempoolSpend>,
}

/// Reason a mempool transaction was removed from the source's mempool view
/// without being mined.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MempoolEvictionReason {
    /// Conflicting transaction was admitted instead.
    Conflict,
    /// Source mempool expiry policy removed the transaction.
    Expired,
    /// Fee policy rejected the transaction at the source.
    LowFee,
    /// Source consensus or policy verifier rejected the transaction.
    NodeRejected,
    /// Source removed the transaction without a classifiable reason.
    ///
    /// Polling backends that observe a txid disappear without a chain
    /// commit emit this reason. The streaming `MempoolChange` wire
    /// envelope from Zebra carries no eviction reason, so streaming
    /// backends always emit `Unknown`.
    Unknown,
}

/// Transparent output visible in the live mempool index.
///
/// Mempool outputs do not carry a block height because the transaction has
/// not been mined yet. Consumers correlating mempool outputs with mined
/// outputs must compare on [`TransparentOutPoint`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentMempoolOutput {
    /// Hash of the transparent output script.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Raw `scriptPubKey` bytes for the output.
    pub script_pub_key: Vec<u8>,
    /// Output identity within the mempool transaction.
    pub outpoint: TransparentOutPoint,
    /// Output value in zatoshis.
    pub value_zat: u64,
}

/// Transparent input visible in the live mempool index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransparentMempoolSpend {
    /// Outpoint consumed by this mempool input.
    pub spent_outpoint: TransparentOutPoint,
    /// Transaction in the mempool that consumes the outpoint.
    pub spending_transaction_id: TransactionId,
}

/// Bounded request for transparent mempool outputs tied to one address.
///
/// Mirrors the mined `TransparentAddressUtxosRequest` shape so the call
/// surface for transparent address queries stays uniform between mined and
/// mempool reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransparentMempoolOutputsRequest {
    /// Hash of the transparent address script being queried.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Maximum number of outputs the response may carry.
    pub max_entries: u32,
}
