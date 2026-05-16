//! Public-facts read-model for explorer transaction views.
//!
//! Single typed value the explorer plane returns when rendering a
//! transaction page. Parsed once at the source boundary
//! (`zinder_source::parse_transaction_public_facts`) from raw serialized
//! transaction bytes plus the node-discovered network upgrade table.
//! Carries everything the wallet plane already publishes plus the
//! ZIP-aware version, lock-time, expiry-height, component counts, and
//! privacy classification the explorer needs.
//!
//! The full design contract lives in
//! [ADR-0010](../../../docs/adrs/0010-transaction-public-facts.md).

use crate::{AuthDigest, BlockHeight, ConsensusBranchId, TransactionId};

/// Wallet-transaction identifier carrying the witness-data commitment.
///
/// Per [ZIP-239](https://zips.z.cash/zip-0239), v5 transactions are relayed
/// under `MSG_WTX` with `wtxid = txid || auth_digest` (64 bytes total).
/// Pre-v5 transactions have no distinct wtxid because their txid already
/// covers their witness data; `Wtxid` is therefore only populated for v5+
/// transactions and `None` otherwise.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Wtxid([u8; 64]);

impl Wtxid {
    /// Creates a wtxid from canonical 64-byte material (`txid || auth_digest`).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    /// Returns the wtxid bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 64] {
        self.0
    }

    /// Returns the leading 32 bytes (`txid` half) of the wtxid.
    #[must_use]
    pub const fn txid_bytes(self) -> [u8; 32] {
        let mut out = [0_u8; 32];
        let mut idx = 0;
        while idx < 32 {
            out[idx] = self.0[idx];
            idx += 1;
        }
        out
    }
}

/// Coarse transaction-format identifier surfaced on the explorer wire.
///
/// Carries the integer `effective_version` from [ZIP-225](https://zips.z.cash/zip-0225)
/// for every variant plus the `version_group_id` from the v3/v4/v5 header so
/// future variants stay distinguishable without inventing fresh enum values.
/// `Unsupported` covers transactions whose effective version the parser
/// does not yet model (NU7 v6, hypothetical v7, etc.); the explorer surface
/// degrades to `unsupported_sections` and `PrivacyShape::Unclassified` for
/// those rows instead of failing outright.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionVersion {
    /// Sprout v1 transaction (pre-Overwinter).
    V1,
    /// Sprout v2 transaction (pre-Overwinter).
    V2,
    /// Overwinter-activated v3 transaction.
    V3,
    /// Sapling-activated v4 transaction.
    V4,
    /// NU5-activated v5 transaction.
    V5,
    /// Transaction format the parser does not model end-to-end.
    Unsupported {
        /// Raw effective-version field from the transaction header.
        effective_version: u32,
        /// Raw `version_group_id` when one is present in the header.
        version_group_id: Option<u32>,
    },
}

impl TransactionVersion {
    /// Returns the effective-version integer the wire surface advertises.
    #[must_use]
    pub const fn effective_version(self) -> u32 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
            Self::V3 => 3,
            Self::V4 => 4,
            Self::V5 => 5,
            Self::Unsupported {
                effective_version, ..
            } => effective_version,
        }
    }

    /// Returns `true` for any version Zinder fully models.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        !matches!(self, Self::Unsupported { .. })
    }
}

/// Lock-time classification kept thin so the wire can carry it as a oneof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockTime {
    /// No lock-time constraint applies (header value zero or all sequences max).
    Unlocked,
    /// Lock time interpreted as a block height.
    Height(BlockHeight),
    /// Lock time interpreted as a Unix epoch seconds value.
    UnixSeconds(u64),
}

/// Coarse privacy classification surfaced verbatim on the explorer wire.
///
/// Computed by [`classify_privacy_shape`] from the component counts and
/// `is_coinbase` flag; the classifier does not look at scripts, values, or
/// shielded encryption blobs. Every transaction the explorer renders falls
/// into exactly one shape; `Unclassified` covers `TransactionVersion::Unsupported`
/// rows where the parser could not extract reliable component counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivacyShape {
    /// Transparent inputs and outputs only.
    TransparentOnly,
    /// Transparent inputs, shielded outputs, no shielded inputs.
    Shielding,
    /// Shielded inputs, transparent outputs, no shielded outputs.
    Deshielding,
    /// Shielded inputs and outputs only, no transparent components.
    ShieldedOnly,
    /// Mixed transparent and shielded components on both sides.
    Mixed,
    /// Coinbase transaction with transparent-only outputs.
    Coinbase,
    /// Coinbase transaction with shielded outputs ([ZIP-213](https://zips.z.cash/zip-0213)).
    ShieldedCoinbase,
    /// Parser could not classify because the transaction version is unsupported.
    Unclassified,
}

/// Forward-compatibility marker for transaction sections the parser does not model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum UnsupportedSection {
    /// Future transaction-version header the parser does not decode.
    FutureVersionHeader,
    /// Future shielded protocol the parser does not iterate.
    FutureShieldedProtocol,
}

/// Component counts feeding [`classify_privacy_shape`] and the wire surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionComponentCounts {
    /// Transparent input count.
    pub transparent_input_count: u32,
    /// Transparent output count.
    pub transparent_output_count: u32,
    /// Sapling spend count.
    pub sapling_spend_count: u32,
    /// Sapling output count.
    pub sapling_output_count: u32,
    /// Orchard action count.
    pub orchard_action_count: u32,
    /// Sprout `JoinSplit` count.
    pub sprout_joinsplit_count: u32,
}

impl TransactionComponentCounts {
    /// All-zero counts marker, useful as a default for unsupported transactions.
    pub const EMPTY: Self = Self {
        transparent_input_count: 0,
        transparent_output_count: 0,
        sapling_spend_count: 0,
        sapling_output_count: 0,
        orchard_action_count: 0,
        sprout_joinsplit_count: 0,
    };

    /// Returns `true` when the transaction has any transparent inputs.
    #[must_use]
    pub const fn has_transparent_input(self) -> bool {
        self.transparent_input_count > 0
    }

    /// Returns `true` when the transaction has any transparent outputs.
    #[must_use]
    pub const fn has_transparent_output(self) -> bool {
        self.transparent_output_count > 0
    }

    /// Returns `true` when the transaction has any shielded inputs
    /// (Sapling spend, Orchard action acting as spend, or Sprout `JoinSplit`).
    #[must_use]
    pub const fn has_shielded_input(self) -> bool {
        self.sapling_spend_count > 0
            || self.orchard_action_count > 0
            || self.sprout_joinsplit_count > 0
    }

    /// Returns the [ZIP-317](https://zips.z.cash/zip-0317) conventional
    /// fee floor in zatoshi computed from the component counts alone.
    ///
    /// The conventional fee is `MARGINAL_FEE * max(logical_actions,
    /// GRACE_ACTIONS)`, where `MARGINAL_FEE = 5_000`, `GRACE_ACTIONS =
    /// 2`, and `logical_actions = max(transparent_input_count,
    /// transparent_output_count, max(sapling_spend_count,
    /// sapling_output_count), orchard_action_count)`. The result is the
    /// minimum fee a wallet should attach to a transaction with this
    /// shape; the actual fee a miner collected may differ and requires
    /// prevout resolution to compute.
    ///
    /// Used by `ExplorerQuery.FeeSummary` to aggregate fee floors across
    /// a block range without resolving every transparent input.
    #[must_use]
    pub const fn zip317_conventional_fee_zat(self) -> u64 {
        const MARGINAL_FEE_ZAT: u64 = 5_000;
        const GRACE_ACTIONS: u32 = 2;
        let sapling_logical = if self.sapling_spend_count > self.sapling_output_count {
            self.sapling_spend_count
        } else {
            self.sapling_output_count
        };
        let mut max_logical = self.transparent_input_count;
        if self.transparent_output_count > max_logical {
            max_logical = self.transparent_output_count;
        }
        if sapling_logical > max_logical {
            max_logical = sapling_logical;
        }
        if self.orchard_action_count > max_logical {
            max_logical = self.orchard_action_count;
        }
        let logical_actions = if max_logical < GRACE_ACTIONS {
            GRACE_ACTIONS
        } else {
            max_logical
        };
        MARGINAL_FEE_ZAT * logical_actions as u64
    }

    /// Returns `true` when the transaction has any shielded outputs
    /// (Sapling output, Orchard action acting as output, or Sprout `JoinSplit`).
    #[must_use]
    pub const fn has_shielded_output(self) -> bool {
        self.sapling_output_count > 0
            || self.orchard_action_count > 0
            || self.sprout_joinsplit_count > 0
    }
}

/// Read-model carrying every public fact the explorer renders for a transaction.
///
/// Constructed once by `zinder_source::parse_transaction_public_facts` and
/// surfaced verbatim on `ExplorerQuery.TransactionDetail`. Wallet ingest and
/// the mempool entry hydration path will adopt the same struct in subsequent
/// slices so all three call sites share one parse. Adding a new field is a
/// breaking change for external constructors; the struct stays passive on
/// purpose so callers spell out every field at construction time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionPublicFacts {
    /// Canonical transaction identifier.
    pub transaction_id: TransactionId,
    /// ZIP-244 authorization digest (v5+; `None` for v1-v4).
    pub auth_digest: Option<AuthDigest>,
    /// ZIP-239 witness-data identifier (v5+; `None` for v1-v4).
    pub wtxid: Option<Wtxid>,
    /// Coarse transaction-format identifier.
    pub version: TransactionVersion,
    /// Consensus branch ID that authorized the transaction.
    ///
    /// Pulled from the transaction header for v5+; resolved from the node
    /// activation table for v3/v4 transactions; `None` for v1/v2 (no
    /// `nVersionGroupId` field on those legacy headers).
    pub consensus_branch_id: Option<ConsensusBranchId>,
    /// Lock-time classification.
    pub lock_time: LockTime,
    /// [ZIP-203](https://zips.z.cash/zip-0203) `nExpiryHeight` when present and
    /// non-sentinel. `None` for v1/v2 (no expiry field) and for the
    /// "no expiry" sentinel value 0.
    pub expiry_height: Option<BlockHeight>,
    /// Raw transaction byte length.
    pub size_bytes: u32,
    /// Component counts feeding the privacy classifier and wire surface.
    pub counts: TransactionComponentCounts,
    /// Coarse privacy classification.
    pub privacy_shape: PrivacyShape,
    /// `true` when the transaction is a coinbase row.
    pub is_coinbase: bool,
    /// Forward-compatibility markers for sections the parser does not decode.
    pub unsupported_sections: Vec<UnsupportedSection>,
}

/// Classifies a transaction's privacy shape from its component counts.
///
/// Pure function on the counts struct and `is_coinbase` flag; no I/O, no
/// transaction bytes. Coinbase transactions classify as
/// [`PrivacyShape::Coinbase`] when their outputs are transparent-only and
/// [`PrivacyShape::ShieldedCoinbase`] when they contain Sapling outputs or
/// Orchard actions per [ZIP-213](https://zips.z.cash/zip-0213).
/// Non-coinbase transactions classify into the four pure shapes
/// ([`PrivacyShape::TransparentOnly`], [`Shielding`](PrivacyShape::Shielding),
/// [`Deshielding`](PrivacyShape::Deshielding),
/// [`ShieldedOnly`](PrivacyShape::ShieldedOnly)) when the counts split
/// cleanly, [`PrivacyShape::Mixed`] when both sides carry both kinds, and
/// [`PrivacyShape::Unclassified`] when no rule fires (e.g. zero-input,
/// zero-output transactions or unsupported versions).
#[must_use]
pub fn classify_privacy_shape(
    counts: TransactionComponentCounts,
    is_coinbase: bool,
    version: TransactionVersion,
) -> PrivacyShape {
    if !version.is_supported() {
        return PrivacyShape::Unclassified;
    }
    if is_coinbase {
        return if counts.sapling_output_count > 0 || counts.orchard_action_count > 0 {
            PrivacyShape::ShieldedCoinbase
        } else {
            PrivacyShape::Coinbase
        };
    }
    classify_non_coinbase(counts)
}

fn classify_non_coinbase(counts: TransactionComponentCounts) -> PrivacyShape {
    let has_transparent_in = counts.has_transparent_input();
    let has_transparent_out = counts.has_transparent_output();
    let has_shielded_in = counts.has_shielded_input();
    let has_shielded_out = counts.has_shielded_output();
    match (
        has_transparent_in,
        has_transparent_out,
        has_shielded_in,
        has_shielded_out,
    ) {
        (true, true, false, false) => PrivacyShape::TransparentOnly,
        (true, _, false, true) if !has_shielded_in => PrivacyShape::Shielding,
        (_, true, true, false) if !has_shielded_out => PrivacyShape::Deshielding,
        (false, false, true, true) => PrivacyShape::ShieldedOnly,
        (true, true, true, true) | (true, _, true, _) | (_, true, _, true) => PrivacyShape::Mixed,
        _ => PrivacyShape::Unclassified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds counts from a positional array.
    ///
    /// Indices: `[transparent_in, transparent_out, sapling_spend, sapling_out, orchard_action, sprout_joinsplit]`.
    fn counts(values: [u32; 6]) -> TransactionComponentCounts {
        TransactionComponentCounts {
            transparent_input_count: values[0],
            transparent_output_count: values[1],
            sapling_spend_count: values[2],
            sapling_output_count: values[3],
            orchard_action_count: values[4],
            sprout_joinsplit_count: values[5],
        }
    }

    #[test]
    fn transparent_only_v5_classifies_as_transparent_only() {
        let shape =
            classify_privacy_shape(counts([1, 1, 0, 0, 0, 0]), false, TransactionVersion::V5);
        assert_eq!(shape, PrivacyShape::TransparentOnly);
    }

    #[test]
    fn shielding_v5_classifies_as_shielding() {
        let shape =
            classify_privacy_shape(counts([1, 0, 0, 1, 0, 0]), false, TransactionVersion::V5);
        assert_eq!(shape, PrivacyShape::Shielding);
    }

    #[test]
    fn deshielding_v5_classifies_as_deshielding() {
        let shape =
            classify_privacy_shape(counts([0, 1, 1, 0, 0, 0]), false, TransactionVersion::V5);
        assert_eq!(shape, PrivacyShape::Deshielding);
    }

    #[test]
    fn shielded_only_orchard_classifies_as_shielded_only() {
        let shape =
            classify_privacy_shape(counts([0, 0, 0, 0, 2, 0]), false, TransactionVersion::V5);
        assert_eq!(shape, PrivacyShape::ShieldedOnly);
    }

    #[test]
    fn mixed_components_classify_as_mixed() {
        let shape =
            classify_privacy_shape(counts([1, 1, 1, 1, 0, 0]), false, TransactionVersion::V5);
        assert_eq!(shape, PrivacyShape::Mixed);
    }

    #[test]
    fn coinbase_transparent_only_classifies_as_coinbase() {
        let shape =
            classify_privacy_shape(counts([1, 1, 0, 0, 0, 0]), true, TransactionVersion::V5);
        assert_eq!(shape, PrivacyShape::Coinbase);
    }

    #[test]
    fn coinbase_with_sapling_outputs_classifies_as_shielded_coinbase() {
        let shape =
            classify_privacy_shape(counts([1, 0, 0, 1, 0, 0]), true, TransactionVersion::V5);
        assert_eq!(shape, PrivacyShape::ShieldedCoinbase);
    }

    #[test]
    fn unsupported_version_classifies_as_unclassified() {
        let unsupported = TransactionVersion::Unsupported {
            effective_version: 6,
            version_group_id: Some(0x0123_4567),
        };
        let shape = classify_privacy_shape(counts([1, 1, 0, 0, 0, 0]), false, unsupported);
        assert_eq!(shape, PrivacyShape::Unclassified);
    }

    #[test]
    fn zip317_conventional_fee_matches_grace_floor() {
        // Empty counts: logical_actions = 0, clamped up to GRACE_ACTIONS = 2;
        // conventional fee = 5_000 * 2 = 10_000 zatoshi.
        assert_eq!(
            TransactionComponentCounts::EMPTY.zip317_conventional_fee_zat(),
            10_000,
        );
    }

    #[test]
    fn zip317_conventional_fee_uses_max_across_axes() {
        // 1 transparent_in, 2 transparent_out, 1 sapling_spend, 0 sapling_out,
        // 3 orchard_action -> logical_actions = max(1, 2, max(1, 0), 3) = 3.
        // Conventional fee = 5_000 * 3 = 15_000 zatoshi.
        let payload = counts([1, 2, 1, 0, 3, 0]);
        assert_eq!(payload.zip317_conventional_fee_zat(), 15_000);
    }

    #[test]
    fn zip317_conventional_fee_uses_max_of_sapling_spends_and_outputs() {
        // 4 sapling_spend, 7 sapling_out -> sapling_logical = 7, dominates.
        // Conventional fee = 5_000 * 7 = 35_000 zatoshi.
        let payload = counts([0, 0, 4, 7, 0, 0]);
        assert_eq!(payload.zip317_conventional_fee_zat(), 35_000);
    }

    #[test]
    fn zip317_conventional_fee_floored_to_grace_actions() {
        // 1 transparent_in, 1 transparent_out -> logical_actions = 1, floored
        // up to GRACE_ACTIONS = 2; conventional fee = 5_000 * 2 = 10_000.
        let payload = counts([1, 1, 0, 0, 0, 0]);
        assert_eq!(payload.zip317_conventional_fee_zat(), 10_000);
    }

    #[test]
    fn wtxid_txid_bytes_matches_leading_half() {
        let mut bytes = [0_u8; 64];
        for (idx, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::try_from(idx % 256).unwrap_or(0);
        }
        let wtxid = Wtxid::from_bytes(bytes);
        let txid = wtxid.txid_bytes();
        for idx in 0..32 {
            assert_eq!(txid[idx], bytes[idx]);
        }
    }
}
