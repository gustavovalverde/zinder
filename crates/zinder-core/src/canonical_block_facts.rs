//! Backend-neutral canonical block facts and their reference digests.

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    BlockHeaderArtifact, ConsensusBranchId, LockTime, PrivacyShape, TransactionComponentCounts,
    TransactionIntrinsicValueBalances, TransactionPublicFacts, TransactionVersion,
    TransparentInputFact, TransparentOutPoint, TransparentOutputFact, UnsupportedSection,
};

const BLOCK_DIGEST_DOMAIN: &[u8] = b"zinder:canonical-block-facts:fact-only:sha256\0";
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
    /// Initial fact-only tagged encoding.
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

/// An encoded canonical-block-facts digest version this binary does not support.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("unsupported canonical block facts digest version {encoded_version}")]
pub struct UnsupportedCanonicalBlockFactsDigestVersion {
    encoded_version: u16,
}

impl TryFrom<u16> for CanonicalBlockFactsDigestVersion {
    type Error = UnsupportedCanonicalBlockFactsDigestVersion;

    fn try_from(encoded_version: u16) -> Result<Self, Self::Error> {
        match encoded_version {
            1 => Ok(Self::V1),
            _ => Err(UnsupportedCanonicalBlockFactsDigestVersion { encoded_version }),
        }
    }
}

/// Owned bytes of one versioned [`CanonicalBlockFacts`] reference encoding.
///
/// This value is the backend-neutral digest input, not a physical storage
/// schema. It can be persisted by benchmark fixtures or storage candidates,
/// then hashed again without reconstructing the original Rust aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalBlockFactsReferenceEncoding {
    version: CanonicalBlockFactsDigestVersion,
    bytes: Vec<u8>,
}

/// SHA-256 reference digest of one versioned [`CanonicalBlockFacts`] value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalBlockFactsDigest {
    version: CanonicalBlockFactsDigestVersion,
    bytes: [u8; 32],
}

/// SHA-256 commitment to one exact consensus-serialized byte sequence.
///
/// This digest is not a block hash or transaction ID. It binds optional raw
/// payload retention to the canonical facts without placing the payload bytes
/// themselves in the replay contract.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SerializedBytesDigest([u8; 32]);

impl SerializedBytesDigest {
    /// Computes the digest of one exact serialized byte sequence.
    #[must_use]
    pub fn from_serialized_bytes(serialized_bytes: &[u8]) -> Self {
        Self(Sha256::digest(serialized_bytes).into())
    }

    /// Reconstructs a digest previously stored in a validated wire record.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the committed SHA-256 bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
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

/// An encoded canonical-fact sequence digest version this binary does not support.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("unsupported canonical block facts sequence digest version {encoded_version}")]
pub struct UnsupportedCanonicalBlockFactsSequenceDigestVersion {
    encoded_version: u16,
}

impl TryFrom<u16> for CanonicalBlockFactsSequenceDigestVersion {
    type Error = UnsupportedCanonicalBlockFactsSequenceDigestVersion;

    fn try_from(encoded_version: u16) -> Result<Self, Self::Error> {
        match encoded_version {
            1 => Ok(Self::V1),
            _ => Err(UnsupportedCanonicalBlockFactsSequenceDigestVersion { encoded_version }),
        }
    }
}

impl CanonicalBlockFactsDigest {
    /// Hashes bytes that are already in the selected reference-encoding format.
    ///
    /// This operation does not parse the bytes, prove that they encode a
    /// [`CanonicalBlockFacts`] value, or recover semantic facts. It only applies
    /// the selected version's digest domain to the supplied bytes. Callers that
    /// load a stored encoding must validate its provenance separately.
    #[must_use]
    pub fn from_reference_encoding(
        version: CanonicalBlockFactsDigestVersion,
        reference_encoding: &[u8],
    ) -> Self {
        let bytes = match version {
            CanonicalBlockFactsDigestVersion::V1 => {
                sha256_domain_and_bytes(BLOCK_DIGEST_DOMAIN, reference_encoding)
            }
        };
        Self { version, bytes }
    }

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

impl CanonicalBlockFactsReferenceEncoding {
    /// Wraps stored reference-encoding bytes with their checked digest version.
    ///
    /// This constructor deliberately does not decode or semantically validate
    /// `bytes`. It exists so a storage implementation can recompute the
    /// backend-neutral reference digest over bytes it previously persisted.
    #[must_use]
    pub fn from_stored_bytes(
        version: CanonicalBlockFactsDigestVersion,
        bytes: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            version,
            bytes: bytes.into(),
        }
    }

    /// Returns the digest-contract version that defines these bytes.
    #[must_use]
    pub const fn version(&self) -> CanonicalBlockFactsDigestVersion {
        self.version
    }

    /// Borrows the complete versioned reference encoding.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the wrapper and returns the complete reference encoding.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Hashes these bytes under their selected reference-digest contract.
    ///
    /// Like [`CanonicalBlockFactsDigest::from_reference_encoding`], this does
    /// not decode or semantically validate the bytes.
    #[must_use]
    pub fn digest(&self) -> CanonicalBlockFactsDigest {
        CanonicalBlockFactsDigest::from_reference_encoding(self.version, &self.bytes)
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
        let mut block_digest_encoding = TaggedFactsEncoder::new();
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
        let mut sequence = TaggedFactsEncoder::new();
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
    /// Commitment to the exact consensus-serialized transaction bytes.
    pub serialized_bytes_digest: SerializedBytesDigest,
    /// Transaction-intrinsic shielded-pool balances.
    pub intrinsic_value_balances: TransactionIntrinsicValueBalances,
    /// Ordered transparent inputs observed in the transaction.
    pub transparent_inputs: Vec<TransparentInputFact>,
    /// Ordered transparent outputs created by the transaction.
    pub transparent_outputs: Vec<TransparentOutputFact>,
}

/// Source-block-local canonical facts used by deterministic projections.
///
/// Every field is immutable and computable from one source block. Chain
/// position, resolved transparent spends, address indexes, and retention-policy
/// payload blobs are deliberately absent. The same source block therefore has
/// one fact identity under every raw-blob retention policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalBlockFacts {
    /// Canonical block-header facts.
    pub block_header: BlockHeaderArtifact,
    /// Commitment to the exact consensus-serialized block bytes.
    pub serialized_bytes_digest: SerializedBytesDigest,
    /// Transactions in canonical block order.
    pub transactions: Vec<CanonicalTransactionFacts>,
}

impl CanonicalBlockFacts {
    /// Encodes this complete value under the backend-neutral digest contract.
    ///
    /// The returned bytes use explicit numeric field tags, `u64` length
    /// prefixes, little-endian integers, option-presence bytes, and ordered
    /// sequence boundaries. They are independent of Serde, `Debug`, Rust memory
    /// layout, and any `RocksDB` or `Postgres` physical schema.
    #[must_use]
    pub fn reference_encoding(
        &self,
        version: CanonicalBlockFactsDigestVersion,
    ) -> CanonicalBlockFactsReferenceEncoding {
        let bytes = match version {
            CanonicalBlockFactsDigestVersion::V1 => reference_encoding_v1(self, version),
        };
        CanonicalBlockFactsReferenceEncoding { version, bytes }
    }

    /// Computes the backend-neutral reference digest for this complete value.
    ///
    /// The encoding uses explicit numeric field tags, `u64` length prefixes,
    /// little-endian integers, option-presence bytes, and ordered sequence
    /// boundaries. It never depends on Serde, `Debug`, or Rust memory layout.
    #[must_use]
    pub fn digest(&self, version: CanonicalBlockFactsDigestVersion) -> CanonicalBlockFactsDigest {
        self.reference_encoding(version).digest()
    }
}

fn reference_encoding_v1(
    facts: &CanonicalBlockFacts,
    version: CanonicalBlockFactsDigestVersion,
) -> Vec<u8> {
    let CanonicalBlockFacts {
        block_header,
        serialized_bytes_digest,
        transactions,
    } = facts;
    let mut fields = TaggedFactsEncoder::new();
    fields.field(1, &version.value().to_le_bytes());
    fields.field(2, &encode_block_header(block_header));
    fields.field(3, &serialized_bytes_digest.as_bytes());
    fields.field(
        4,
        &encode_sequence(transactions, encode_canonical_transaction_facts),
    );
    fields.into_bytes()
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
    let mut fields = TaggedFactsEncoder::new();
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

fn encode_canonical_transaction_facts(facts: &CanonicalTransactionFacts) -> Vec<u8> {
    let CanonicalTransactionFacts {
        public_facts,
        serialized_bytes_digest,
        intrinsic_value_balances,
        transparent_inputs,
        transparent_outputs,
    } = facts;
    let mut fields = TaggedFactsEncoder::new();
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
    fields.field(5, &serialized_bytes_digest.as_bytes());
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
    let mut fields = TaggedFactsEncoder::new();
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
    let mut fields = TaggedFactsEncoder::new();
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
    let mut fields = TaggedFactsEncoder::new();
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
    let mut fields = TaggedFactsEncoder::new();
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
    let mut fields = TaggedFactsEncoder::new();
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
    let mut fields = TaggedFactsEncoder::new();
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
    let mut fields = TaggedFactsEncoder::new();
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
    let mut fields = TaggedFactsEncoder::new();
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

fn sha256_domain_and_fields(domain: &[u8], fields: TaggedFactsEncoder) -> [u8; 32] {
    sha256_domain_and_bytes(domain, &fields.into_bytes())
}

fn sha256_domain_and_bytes(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    hasher.finalize().into()
}

fn push_length(output: &mut Vec<u8>, length: usize) {
    // Zinder rejects targets wider than 64 bits at crate compile time, so this
    // conversion cannot truncate on any supported target.
    let length = u64::try_from(length).unwrap_or(u64::MAX);
    output.extend_from_slice(&length.to_le_bytes());
}

struct TaggedFactsEncoder {
    bytes: Vec<u8>,
}

impl TaggedFactsEncoder {
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
        CanonicalBlockFacts, CanonicalBlockFactsDigest, CanonicalBlockFactsDigestVersion,
        CanonicalBlockFactsReferenceEncoding, CanonicalBlockFactsSequenceDigestBuilder,
        CanonicalBlockFactsSequenceDigestVersion, CanonicalBlockFactsSequenceLengthOverflow,
    };
    use crate::{BlockHash, BlockHeaderArtifact, BlockHeight, SerializedBytesDigest};

    #[test]
    fn reference_encoding_recomputes_the_canonical_digest_from_stored_bytes() {
        let facts = sample_block_facts();
        let encoding = facts.reference_encoding(CanonicalBlockFactsDigestVersion::CURRENT);
        let stored_encoding = CanonicalBlockFactsReferenceEncoding::from_stored_bytes(
            encoding.version(),
            encoding.as_bytes(),
        );

        assert_eq!(stored_encoding, encoding);
        assert_eq!(stored_encoding.digest(), facts.digest(encoding.version()));
        assert_eq!(
            stored_encoding.digest(),
            CanonicalBlockFactsDigest::from_reference_encoding(
                encoding.version(),
                encoding.as_bytes(),
            )
        );

        let mut changed_bytes = encoding.into_bytes();
        changed_bytes.push(0xA5);
        assert_ne!(
            CanonicalBlockFactsDigest::from_reference_encoding(
                CanonicalBlockFactsDigestVersion::CURRENT,
                &changed_bytes,
            ),
            facts.digest(CanonicalBlockFactsDigestVersion::CURRENT)
        );
    }

    #[test]
    fn persisted_digest_versions_fail_closed_when_unknown() {
        assert_eq!(
            CanonicalBlockFactsDigestVersion::try_from(1),
            Ok(CanonicalBlockFactsDigestVersion::V1)
        );
        assert!(CanonicalBlockFactsDigestVersion::try_from(0).is_err());
        assert!(CanonicalBlockFactsDigestVersion::try_from(2).is_err());
        assert!(CanonicalBlockFactsDigestVersion::try_from(3).is_err());
        assert_eq!(
            CanonicalBlockFactsSequenceDigestVersion::try_from(1),
            Ok(CanonicalBlockFactsSequenceDigestVersion::V1)
        );
        assert!(CanonicalBlockFactsSequenceDigestVersion::try_from(0).is_err());
        assert!(CanonicalBlockFactsSequenceDigestVersion::try_from(2).is_err());
    }

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

    fn sample_block_facts() -> CanonicalBlockFacts {
        CanonicalBlockFacts {
            block_header: BlockHeaderArtifact::new(
                BlockHeight::new(7),
                BlockHash::from_bytes([0x11; 32]),
                BlockHash::from_bytes([0x22; 32]),
                [0x33; 32],
                [0x44; 32],
                1_700_000_000,
                0x1f07_ffff,
                [0x55; 32],
                4,
                128,
            ),
            serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(&[]),
            transactions: Vec::new(),
        }
    }
}
