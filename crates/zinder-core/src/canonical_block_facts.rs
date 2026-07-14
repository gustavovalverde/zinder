//! Backend-neutral canonical block facts and their reference digests.

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    BlockHeaderArtifact, CompactBlockArtifact, ConsensusBranchId, LockTime, PrivacyShape,
    TransactionComponentCounts, TransactionIntrinsicValueBalances, TransactionPublicFacts,
    TransactionVersion, TransparentInputFact, TransparentOutPoint, TransparentOutputFact,
    UnsupportedSection,
};

const BLOCK_DIGEST_DOMAIN: &[u8] = b"zinder:canonical-block-facts:sha256\0";
const SEQUENCE_ITEM_DOMAIN: &[u8] = b"zinder:canonical-block-facts:ordered-items:sha256\0";
const SEQUENCE_DIGEST_DOMAIN: &[u8] = b"zinder:canonical-block-facts:ordered-sequence:sha256\0";

/// Version of the backend-neutral [`CanonicalBlockFacts`] digest contract.
///
/// This version is independent of physical `RocksDB` or Postgres schemas. A new
/// variant means at least one field, tag, byte order, or sequence rule changed;
/// existing variants retain their exact encoding forever.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum CanonicalBlockFactsDigestVersion {
    /// Initial explicit tagged encoding covering every canonical block fact.
    V1,
}

impl CanonicalBlockFactsDigestVersion {
    /// Version emitted by new reference-digest computations.
    pub const CURRENT: Self = Self::V1;

    /// Returns the stable numeric version written into digest preimages.
    #[must_use]
    pub const fn value(self) -> u16 {
        match self {
            Self::V1 => 1,
        }
    }
}

/// SHA-256 reference digest of one versioned [`CanonicalBlockFacts`] value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalBlockFactsDigest {
    version: CanonicalBlockFactsDigestVersion,
    bytes: [u8; 32],
}

/// Version of the ordered canonical-fact sequence digest algorithm.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum CanonicalBlockFactsSequenceDigestVersion {
    /// Initial ordered SHA-256 sequence digest over typed per-block digests.
    V1,
}

impl CanonicalBlockFactsSequenceDigestVersion {
    /// Version emitted by new ordered sequence digest builders.
    pub const CURRENT: Self = Self::V1;

    /// Returns the stable numeric version written into sequence preimages.
    #[must_use]
    pub const fn value(self) -> u16 {
        match self {
            Self::V1 => 1,
        }
    }
}

impl CanonicalBlockFactsDigest {
    /// Returns the fact-encoding version committed by this digest.
    #[must_use]
    pub const fn version(self) -> CanonicalBlockFactsDigestVersion {
        self.version
    }

    /// Returns the SHA-256 digest bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.bytes
    }
}

/// Ordered digest of a complete canonical fact sequence.
///
/// The digest commits to the item count and to each per-block digest's version
/// and bytes in append order. It can therefore compare a storage candidate
/// with a serial reference without retaining every block digest in memory.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalBlockFactsSequenceDigest {
    version: CanonicalBlockFactsSequenceDigestVersion,
    block_count: u64,
    bytes: [u8; 32],
}

impl CanonicalBlockFactsSequenceDigest {
    /// Returns the ordered-sequence algorithm version committed by this digest.
    #[must_use]
    pub const fn version(self) -> CanonicalBlockFactsSequenceDigestVersion {
        self.version
    }

    /// Returns the number of per-block digests committed by this sequence.
    #[must_use]
    pub const fn block_count(self) -> u64 {
        self.block_count
    }

    /// Returns the SHA-256 digest bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.bytes
    }
}

/// Streaming builder for an ordered [`CanonicalBlockFactsSequenceDigest`].
#[derive(Clone, Debug)]
pub struct CanonicalBlockFactsSequenceDigestBuilder {
    sequence_version: CanonicalBlockFactsSequenceDigestVersion,
    ordered_item_hasher: Sha256,
    block_count: u64,
}

/// Failure to extend an ordered canonical-fact digest beyond its encoded count.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("canonical block facts sequence digest cannot exceed u64::MAX blocks")]
pub struct CanonicalBlockFactsSequenceLengthOverflow;

impl CanonicalBlockFactsSequenceDigestBuilder {
    /// Starts an empty ordered sequence digest using an explicit algorithm version.
    #[must_use]
    pub fn new(sequence_version: CanonicalBlockFactsSequenceDigestVersion) -> Self {
        let mut ordered_item_hasher = Sha256::new();
        match sequence_version {
            CanonicalBlockFactsSequenceDigestVersion::V1 => {
                ordered_item_hasher.update(SEQUENCE_ITEM_DOMAIN);
            }
        }
        Self {
            sequence_version,
            ordered_item_hasher,
            block_count: 0,
        }
    }

    /// Appends one block digest at the next sequence position.
    pub fn try_append(
        &mut self,
        digest: CanonicalBlockFactsDigest,
    ) -> Result<(), CanonicalBlockFactsSequenceLengthOverflow> {
        let next_block_count = self
            .block_count
            .checked_add(1)
            .ok_or(CanonicalBlockFactsSequenceLengthOverflow)?;
        let CanonicalBlockFactsDigest { version, bytes } = digest;
        let mut block_digest_encoding = FactsV1Encoder::new();
        block_digest_encoding.field(1, &version.value().to_le_bytes());
        block_digest_encoding.field(2, &bytes);
        self.ordered_item_hasher
            .update(block_digest_encoding.into_bytes());
        self.block_count = next_block_count;
        Ok(())
    }

    /// Finishes the ordered sequence digest.
    #[must_use]
    pub fn finish(self) -> CanonicalBlockFactsSequenceDigest {
        let Self {
            sequence_version,
            ordered_item_hasher,
            block_count,
        } = self;
        let ordered_item_digest: [u8; 32] = ordered_item_hasher.finalize().into();
        let mut sequence = FactsV1Encoder::new();
        sequence.field(1, &sequence_version.value().to_le_bytes());
        sequence.field(2, &block_count.to_le_bytes());
        sequence.field(3, &ordered_item_digest);

        CanonicalBlockFactsSequenceDigest {
            version: sequence_version,
            block_count,
            bytes: sha256_domain_and_fields(SEQUENCE_DIGEST_DOMAIN, sequence),
        }
    }
}

/// Immutable facts for one transaction at its position in a source block.
///
/// The containing block identity and this value's vector position supply its
/// mined location. Current-schema index rows are expanded only at the writer
/// boundary and are not part of this backend-neutral contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalTransactionFacts {
    /// Public transaction identity and scalar protocol facts.
    pub public_facts: TransactionPublicFacts,
    /// Transaction-intrinsic shielded-pool balances.
    pub intrinsic_value_balances: TransactionIntrinsicValueBalances,
    /// Ordered transparent inputs observed in the transaction.
    pub transparent_inputs: Vec<TransparentInputFact>,
    /// Ordered transparent outputs created by the transaction.
    pub transparent_outputs: Vec<TransparentOutputFact>,
    /// Optional serialized consensus transaction bytes.
    pub raw_transaction_bytes: Option<Vec<u8>>,
}

/// Source-block-local canonical facts and optionally retained payloads.
///
/// Every field is immutable and computable from one source block. Chain
/// position, resolved transparent spends, and address indexes are deliberately
/// absent because they require ordered cross-block state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalBlockFacts {
    /// Canonical block-header facts.
    pub block_header: BlockHeaderArtifact,
    /// Optional serialized consensus block bytes.
    pub raw_block_bytes: Option<Vec<u8>>,
    /// Transactions in canonical block order.
    pub transactions: Vec<CanonicalTransactionFacts>,
}

impl CanonicalBlockFacts {
    /// Computes the backend-neutral reference digest for this complete value.
    ///
    /// The encoding uses explicit numeric field tags, `u64` length prefixes,
    /// little-endian integers, option-presence bytes, and ordered sequence
    /// boundaries. It never depends on Serde, `Debug`, or Rust memory layout.
    #[must_use]
    pub fn digest(&self, version: CanonicalBlockFactsDigestVersion) -> CanonicalBlockFactsDigest {
        let bytes = match version {
            CanonicalBlockFactsDigestVersion::V1 => digest_v1(self, version),
        };
        CanonicalBlockFactsDigest { version, bytes }
    }
}

/// Canonical block facts placed at an ordered commitment-tree position.
///
/// The block-local facts remain intact while the compact block and tip
/// metadata carry the chain-prefix position assigned during serial
/// finalization. Resolved spend and projection state remain outside this
/// boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PositionedCanonicalBlock {
    /// Immutable facts parsed from the source block.
    pub facts: CanonicalBlockFacts,
    /// Lightwalletd compact block with final chain metadata stamped.
    pub compact_block: CompactBlockArtifact,
    /// Running commitment-tree position after this block.
    pub tip_metadata: crate::ChainTipMetadata,
}

fn digest_v1(facts: &CanonicalBlockFacts, version: CanonicalBlockFactsDigestVersion) -> [u8; 32] {
    let CanonicalBlockFacts {
        block_header,
        raw_block_bytes,
        transactions,
    } = facts;
    let mut fields = FactsV1Encoder::new();
    fields.field(1, &version.value().to_le_bytes());
    fields.field(2, &encode_block_header(block_header));
    fields.field(3, &encode_optional_raw_bytes(raw_block_bytes.as_deref()));
    fields.field(
        4,
        &encode_sequence(transactions, encode_canonical_transaction_facts),
    );
    sha256_domain_and_fields(BLOCK_DIGEST_DOMAIN, fields)
}

fn encode_block_header(header: &BlockHeaderArtifact) -> Vec<u8> {
    let BlockHeaderArtifact {
        height,
        block_hash,
        parent_hash,
        merkle_root_hash,
        commitment_bytes,
        block_time,
        bits,
        nonce,
        version: block_version,
        block_size_bytes,
    } = header;
    let mut fields = FactsV1Encoder::new();
    fields.field(1, &height.value().to_le_bytes());
    fields.field(2, &block_hash.as_bytes());
    fields.field(3, &parent_hash.as_bytes());
    fields.field(4, merkle_root_hash);
    fields.field(5, commitment_bytes);
    fields.field(6, &block_time.to_le_bytes());
    fields.field(7, &bits.to_le_bytes());
    fields.field(8, nonce);
    fields.field(9, &block_version.to_le_bytes());
    fields.field(10, &block_size_bytes.to_le_bytes());
    fields.into_bytes()
}

fn encode_optional_raw_bytes(raw_bytes: Option<&[u8]>) -> Vec<u8> {
    let mut optional_bytes_encoding = Vec::new();
    match raw_bytes {
        None => optional_bytes_encoding.push(0),
        Some(raw_bytes) => {
            optional_bytes_encoding.push(1);
            push_length(&mut optional_bytes_encoding, raw_bytes.len());
            optional_bytes_encoding.extend_from_slice(raw_bytes);
        }
    }
    optional_bytes_encoding
}

fn encode_canonical_transaction_facts(facts: &CanonicalTransactionFacts) -> Vec<u8> {
    let CanonicalTransactionFacts {
        public_facts,
        intrinsic_value_balances,
        transparent_inputs,
        transparent_outputs,
        raw_transaction_bytes,
    } = facts;
    let mut fields = FactsV1Encoder::new();
    fields.field(1, &encode_transaction_public_facts(public_facts));
    fields.field(
        2,
        &encode_intrinsic_value_balances(intrinsic_value_balances),
    );
    fields.field(
        3,
        &encode_sequence(transparent_inputs, encode_transparent_input_fact),
    );
    fields.field(
        4,
        &encode_sequence(transparent_outputs, encode_transparent_output_fact),
    );
    fields.field(
        5,
        &encode_optional_raw_bytes(raw_transaction_bytes.as_deref()),
    );
    fields.into_bytes()
}

fn encode_transaction_public_facts(facts: &TransactionPublicFacts) -> Vec<u8> {
    let TransactionPublicFacts {
        transaction_id,
        auth_digest,
        wtxid,
        version,
        consensus_branch_id,
        lock_time,
        expiry_height,
        size_bytes,
        counts,
        orchard_value_balance_zat,
        orchard_anchor,
        ironwood_value_balance_zat,
        privacy_shape,
        is_coinbase,
        unsupported_sections,
    } = facts;
    let mut fields = FactsV1Encoder::new();
    fields.field(1, &transaction_id.as_bytes());
    fields.field(
        2,
        &encode_optional_bytes((*auth_digest).map(crate::AuthDigest::as_bytes)),
    );
    fields.field(
        3,
        &encode_optional_bytes((*wtxid).map(crate::Wtxid::as_bytes)),
    );
    fields.field(4, &encode_transaction_version(*version));
    fields.field(
        5,
        &encode_optional_u32((*consensus_branch_id).map(ConsensusBranchId::value)),
    );
    fields.field(6, &encode_lock_time(*lock_time));
    fields.field(
        7,
        &encode_optional_u32((*expiry_height).map(crate::BlockHeight::value)),
    );
    fields.field(8, &size_bytes.to_le_bytes());
    fields.field(9, &encode_transaction_counts(*counts));
    fields.field(10, &encode_optional_i64(*orchard_value_balance_zat));
    fields.field(11, &encode_optional_bytes(*orchard_anchor));
    fields.field(12, &encode_optional_i64(*ironwood_value_balance_zat));
    fields.field(13, &[privacy_shape_code(*privacy_shape)]);
    fields.field(14, &[u8::from(*is_coinbase)]);
    fields.field(
        15,
        &encode_sequence(unsupported_sections, |section| {
            encode_unsupported_section(*section)
        }),
    );
    fields.into_bytes()
}

fn encode_transaction_version(version: TransactionVersion) -> Vec<u8> {
    let mut fields = FactsV1Encoder::new();
    match version {
        TransactionVersion::V1 => fields.field(1, &[1]),
        TransactionVersion::V2 => fields.field(1, &[2]),
        TransactionVersion::V3 => fields.field(1, &[3]),
        TransactionVersion::V4 => fields.field(1, &[4]),
        TransactionVersion::V5 => fields.field(1, &[5]),
        TransactionVersion::V6 => fields.field(1, &[6]),
        TransactionVersion::Unsupported {
            effective_version,
            version_group_id,
        } => {
            fields.field(1, &[7]);
            fields.field(2, &effective_version.to_le_bytes());
            fields.field(3, &encode_optional_u32(version_group_id));
        }
    }
    fields.into_bytes()
}

fn encode_lock_time(lock_time: LockTime) -> Vec<u8> {
    let mut fields = FactsV1Encoder::new();
    match lock_time {
        LockTime::Unlocked => fields.field(1, &[1]),
        LockTime::Height(height) => {
            fields.field(1, &[2]);
            fields.field(2, &height.value().to_le_bytes());
        }
        LockTime::UnixSeconds(seconds) => {
            fields.field(1, &[3]);
            fields.field(2, &seconds.to_le_bytes());
        }
    }
    fields.into_bytes()
}

fn encode_transaction_counts(counts: TransactionComponentCounts) -> Vec<u8> {
    let TransactionComponentCounts {
        transparent_input_count,
        transparent_output_count,
        sapling_spend_count,
        sapling_output_count,
        orchard_action_count,
        ironwood_action_count,
        sprout_joinsplit_count,
    } = counts;
    let mut fields = FactsV1Encoder::new();
    fields.field(1, &transparent_input_count.to_le_bytes());
    fields.field(2, &transparent_output_count.to_le_bytes());
    fields.field(3, &sapling_spend_count.to_le_bytes());
    fields.field(4, &sapling_output_count.to_le_bytes());
    fields.field(5, &orchard_action_count.to_le_bytes());
    fields.field(6, &ironwood_action_count.to_le_bytes());
    fields.field(7, &sprout_joinsplit_count.to_le_bytes());
    fields.into_bytes()
}

const fn privacy_shape_code(privacy_shape: PrivacyShape) -> u8 {
    match privacy_shape {
        PrivacyShape::TransparentOnly => 1,
        PrivacyShape::Shielding => 2,
        PrivacyShape::Deshielding => 3,
        PrivacyShape::ShieldedOnly => 4,
        PrivacyShape::Mixed => 5,
        PrivacyShape::Coinbase => 6,
        PrivacyShape::ShieldedCoinbase => 7,
        PrivacyShape::Unclassified => 8,
    }
}

fn encode_unsupported_section(section: UnsupportedSection) -> Vec<u8> {
    let code = match section {
        UnsupportedSection::FutureVersionHeader => 1,
        UnsupportedSection::FutureShieldedProtocol => 2,
    };
    vec![code]
}

fn encode_transparent_input_fact(input: &TransparentInputFact) -> Vec<u8> {
    let TransparentInputFact {
        input_index,
        spent_outpoint,
    } = input;
    let mut fields = FactsV1Encoder::new();
    fields.field(1, &input_index.to_le_bytes());
    fields.field(2, &encode_transparent_outpoint(spent_outpoint));
    fields.into_bytes()
}

fn encode_transparent_output_fact(output: &TransparentOutputFact) -> Vec<u8> {
    let TransparentOutputFact {
        output_index,
        value_zat,
        script_pub_key,
        address_script_hash,
    } = output;
    let mut fields = FactsV1Encoder::new();
    fields.field(1, &output_index.to_le_bytes());
    fields.field(2, &value_zat.to_le_bytes());
    fields.field(3, script_pub_key);
    fields.field(4, &address_script_hash.as_bytes());
    fields.into_bytes()
}

fn encode_intrinsic_value_balances(value_balances: &TransactionIntrinsicValueBalances) -> Vec<u8> {
    let TransactionIntrinsicValueBalances {
        sprout_zat,
        sapling_zat,
        orchard_zat,
        ironwood_zat,
    } = value_balances;
    let mut fields = FactsV1Encoder::new();
    fields.field(1, &sprout_zat.to_le_bytes());
    fields.field(2, &sapling_zat.to_le_bytes());
    fields.field(3, &orchard_zat.to_le_bytes());
    fields.field(4, &ironwood_zat.to_le_bytes());
    fields.into_bytes()
}

fn encode_transparent_outpoint(outpoint: &TransparentOutPoint) -> Vec<u8> {
    let TransparentOutPoint {
        transaction_id,
        output_index,
    } = outpoint;
    let mut fields = FactsV1Encoder::new();
    fields.field(1, &transaction_id.as_bytes());
    fields.field(2, &output_index.to_le_bytes());
    fields.into_bytes()
}

fn encode_optional_bytes<const N: usize>(optional_bytes: Option<[u8; N]>) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(N.saturating_add(1));
    match optional_bytes {
        None => encoded.push(0),
        Some(bytes) => {
            encoded.push(1);
            encoded.extend_from_slice(&bytes);
        }
    }
    encoded
}

fn encode_optional_u32(optional_number: Option<u32>) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(5);
    match optional_number {
        None => encoded.push(0),
        Some(number) => {
            encoded.push(1);
            encoded.extend_from_slice(&number.to_le_bytes());
        }
    }
    encoded
}

fn encode_optional_i64(optional_number: Option<i64>) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(9);
    match optional_number {
        None => encoded.push(0),
        Some(number) => {
            encoded.push(1);
            encoded.extend_from_slice(&number.to_le_bytes());
        }
    }
    encoded
}

fn encode_sequence<T>(sequence_entries: &[T], encode_entry: impl Fn(&T) -> Vec<u8>) -> Vec<u8> {
    let mut encoded = Vec::new();
    push_length(&mut encoded, sequence_entries.len());
    for sequence_entry in sequence_entries {
        let entry_encoding = encode_entry(sequence_entry);
        push_length(&mut encoded, entry_encoding.len());
        encoded.extend_from_slice(&entry_encoding);
    }
    encoded
}

fn sha256_domain_and_fields(domain: &[u8], fields: FactsV1Encoder) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(fields.into_bytes());
    hasher.finalize().into()
}

fn push_length(output: &mut Vec<u8>, length: usize) {
    // Zinder rejects targets wider than 64 bits at crate compile time, so this
    // conversion cannot truncate on any supported target.
    let length = u64::try_from(length).unwrap_or(u64::MAX);
    output.extend_from_slice(&length.to_le_bytes());
}

struct FactsV1Encoder {
    bytes: Vec<u8>,
}

impl FactsV1Encoder {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn field(&mut self, tag: u16, field_payload: &[u8]) {
        self.bytes.extend_from_slice(&tag.to_le_bytes());
        push_length(&mut self.bytes, field_payload.len());
        self.bytes.extend_from_slice(field_payload);
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CanonicalBlockFactsDigest, CanonicalBlockFactsDigestVersion,
        CanonicalBlockFactsSequenceDigestBuilder, CanonicalBlockFactsSequenceDigestVersion,
        CanonicalBlockFactsSequenceLengthOverflow,
    };

    #[test]
    fn sequence_digest_rejects_block_count_overflow_before_hashing() {
        let mut builder = CanonicalBlockFactsSequenceDigestBuilder {
            block_count: u64::MAX,
            ..CanonicalBlockFactsSequenceDigestBuilder::new(
                CanonicalBlockFactsSequenceDigestVersion::CURRENT,
            )
        };
        let before_append = builder.clone().finish();
        let digest = CanonicalBlockFactsDigest {
            version: CanonicalBlockFactsDigestVersion::V1,
            bytes: [0xA5; 32],
        };

        assert_eq!(
            builder.try_append(digest),
            Err(CanonicalBlockFactsSequenceLengthOverflow)
        );
        assert_eq!(builder.finish(), before_append);
    }
}
