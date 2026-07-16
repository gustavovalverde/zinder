//! Wallet projection build, readiness, and control contracts.

use zinder_core::{
    BlockHash, BlockHeight, BlockId, CanonicalBlockFactsSequenceDigest,
    CanonicalBlockFactsSequenceDigestVersion, ChainEpochId, Network, TransparentUtxoSetCommitment,
    UTXO_SET_COMMITMENT_LEN, UtxoSetCommitmentScheme,
};

use crate::{
    REQUIRED_CANONICAL_FACTS_DIGEST_VERSION, REQUIRED_CANONICAL_REPLAY_FORMAT_VERSION,
    REQUIRED_CANONICAL_SEQUENCE_DIGEST_VERSION, REQUIRED_CANONICAL_STORE_SCHEMA_VERSION,
    WALLET_PROJECTION_SCHEMA_VERSION, WALLET_PROJECTION_STORE_IDENTITY,
    WALLET_PROJECTION_VALUE_ENCODING_VERSION, WalletProjectionContractError,
};

/// Exact canonical source position represented by wallet projection state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalletProjectionSourcePosition {
    /// Monotonic canonical epoch visible at this position.
    pub chain_epoch_id: ChainEpochId,
    /// Canonical tip projected into wallet state.
    pub tip: BlockId,
    /// Monotonic event-stream sequence projected through this position.
    pub event_sequence: u64,
}

impl WalletProjectionSourcePosition {
    /// Creates an exact wallet projection source position.
    #[must_use]
    pub const fn new(chain_epoch_id: ChainEpochId, tip: BlockId, event_sequence: u64) -> Self {
        Self {
            chain_epoch_id,
            tip,
            event_sequence,
        }
    }
}

/// Immutable plan recorded before a wallet projection build starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletProjectionBuildPlan {
    /// Canonical source position the build must reach before readiness publication.
    pub target_source_position: WalletProjectionSourcePosition,
}

impl WalletProjectionBuildPlan {
    /// Creates a complete-history build plan.
    #[must_use]
    pub fn complete_history(target_source_position: WalletProjectionSourcePosition) -> Self {
        Self {
            target_source_position,
        }
    }
}

/// SHA-256 commitment to every version-1 wallet projection row in key order.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WalletProjectionDigest([u8; 32]);

impl WalletProjectionDigest {
    /// Reconstructs a digest from exact committed bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the exact digest bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Row counts published with wallet readiness evidence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WalletProjectionFamilyRowCounts {
    /// Rows in `transparent_unspent_output`.
    pub transparent_unspent_output_count: u64,
    /// Rows in `transparent_unspent_output_by_address`.
    pub transparent_unspent_output_by_address_count: u64,
    /// Rows in `transparent_spent_output`.
    pub transparent_spent_output_count: u64,
    /// Rows in `transparent_address_transaction`.
    pub transparent_address_transaction_count: u64,
    /// Non-zero rows in `transparent_address_balance`.
    pub transparent_address_balance_count: u64,
    /// Rows in the bounded `reorg_undo` window.
    pub reorg_undo_count: u64,
}

/// Complete UTXO aggregate published with wallet readiness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletUtxoSetSummary {
    /// Number of currently unspent transparent outputs.
    pub utxo_count: u64,
    /// Sum of all currently unspent transparent output values.
    pub total_value_zat: u64,
    /// Full order-independent `LtHash16` accumulator.
    pub commitment: TransparentUtxoSetCommitment,
}

/// Evidence required before wallet queries may serve a projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletProjectionReadyEvidence {
    /// Exact source position of the ready projection.
    pub source_position: WalletProjectionSourcePosition,
    /// Ordered digest of every canonical block-facts record through `source_position`.
    pub source_sequence_digest: CanonicalBlockFactsSequenceDigest,
    /// Digest of every logical projection row.
    pub projection_digest: WalletProjectionDigest,
    /// Exact logical row counts by family.
    pub row_counts: WalletProjectionFamilyRowCounts,
    /// Complete current UTXO aggregate.
    pub utxo_summary: WalletUtxoSetSummary,
}

/// Durable wallet store lifecycle state.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WalletProjectionBuildState {
    /// Rows are incomplete and query admission must refuse the store.
    Building(WalletProjectionBuildPlan),
    /// Evidence is complete and query admission may validate the source fence.
    Ready(WalletProjectionReadyEvidence),
}

/// Singleton durable control record for one wallet projection store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletStoreControl {
    /// Network whose canonical facts produced the projection.
    pub network: Network,
    /// Maximum rollback depth represented by `reorg_undo`.
    pub supported_reorg_depth: u32,
    /// Monotonic single-writer generation.
    pub writer_generation: u64,
    /// Build or ready lifecycle state.
    pub build_state: WalletProjectionBuildState,
}

impl WalletStoreControl {
    /// Encodes the exact, single-version wallet control record.
    pub fn encode(&self) -> Result<Vec<u8>, WalletProjectionContractError> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(WALLET_PROJECTION_STORE_IDENTITY);
        bytes.extend_from_slice(&WALLET_PROJECTION_SCHEMA_VERSION.to_be_bytes());
        bytes.extend_from_slice(&WALLET_PROJECTION_VALUE_ENCODING_VERSION.to_be_bytes());
        bytes.extend_from_slice(&self.network.id().to_be_bytes());
        bytes.extend_from_slice(&REQUIRED_CANONICAL_STORE_SCHEMA_VERSION.to_be_bytes());
        bytes.extend_from_slice(&REQUIRED_CANONICAL_REPLAY_FORMAT_VERSION.to_be_bytes());
        bytes.extend_from_slice(&REQUIRED_CANONICAL_FACTS_DIGEST_VERSION.to_be_bytes());
        bytes.extend_from_slice(&REQUIRED_CANONICAL_SEQUENCE_DIGEST_VERSION.to_be_bytes());
        bytes.extend_from_slice(&self.supported_reorg_depth.to_be_bytes());
        bytes.extend_from_slice(&self.writer_generation.to_be_bytes());
        match &self.build_state {
            WalletProjectionBuildState::Building(plan) => {
                bytes.push(1);
                encode_build_plan(plan, &mut bytes);
            }
            WalletProjectionBuildState::Ready(evidence) => {
                bytes.push(2);
                encode_ready_evidence(evidence, self.supported_reorg_depth, &mut bytes)?;
            }
        }
        Ok(bytes)
    }

    /// Decodes and validates one exact version-1 wallet control record.
    pub fn decode(encoded: &[u8]) -> Result<Self, WalletProjectionContractError> {
        let mut decoder = WalletControlDecoder::new(encoded);
        let identity = decoder.read_bytes(WALLET_PROJECTION_STORE_IDENTITY.len())?;
        if identity != WALLET_PROJECTION_STORE_IDENTITY {
            return Err(WalletProjectionContractError::DurableIdentityMismatch {
                field: "wallet store control",
            });
        }
        decoder.require_u16(
            "wallet projection schema version",
            WALLET_PROJECTION_SCHEMA_VERSION,
        )?;
        decoder.require_u16(
            "wallet projection value encoding version",
            WALLET_PROJECTION_VALUE_ENCODING_VERSION,
        )?;
        let network_id = decoder.read_u32()?;
        let network = Network::from_id(network_id).ok_or_else(|| {
            WalletProjectionContractError::UnsupportedEncodedValue {
                field: "wallet projection network",
                encoded: u64::from(network_id),
            }
        })?;
        decoder.require_u16(
            "required canonical store schema version",
            REQUIRED_CANONICAL_STORE_SCHEMA_VERSION,
        )?;
        decoder.require_u32(
            "required canonical replay format version",
            REQUIRED_CANONICAL_REPLAY_FORMAT_VERSION,
        )?;
        decoder.require_u16(
            "required canonical facts digest version",
            REQUIRED_CANONICAL_FACTS_DIGEST_VERSION,
        )?;
        decoder.require_u16(
            "required canonical sequence digest version",
            REQUIRED_CANONICAL_SEQUENCE_DIGEST_VERSION,
        )?;
        let supported_reorg_depth = decoder.read_u32()?;
        let writer_generation = decoder.read_u64()?;
        let build_state = match decoder.read_u8()? {
            1 => WalletProjectionBuildState::Building(WalletProjectionBuildPlan::complete_history(
                decoder.read_source_position()?,
            )),
            2 => WalletProjectionBuildState::Ready(
                decoder.read_ready_evidence(supported_reorg_depth)?,
            ),
            encoded_state => {
                return Err(WalletProjectionContractError::UnsupportedEncodedValue {
                    field: "wallet projection build state",
                    encoded: u64::from(encoded_state),
                });
            }
        };
        decoder.finish()?;
        let control = Self {
            network,
            supported_reorg_depth,
            writer_generation,
            build_state,
        };
        if control.encode()? != encoded {
            return Err(WalletProjectionContractError::DurableNonCanonicalEncoding {
                field: "wallet store control canonical encoding",
            });
        }
        Ok(control)
    }
}

struct WalletControlDecoder<'a> {
    encoded: &'a [u8],
    offset: usize,
}

impl<'a> WalletControlDecoder<'a> {
    const fn new(encoded: &'a [u8]) -> Self {
        Self { encoded, offset: 0 }
    }

    fn read_ready_evidence(
        &mut self,
        supported_reorg_depth: u32,
    ) -> Result<WalletProjectionReadyEvidence, WalletProjectionContractError> {
        let source_position = self.read_source_position()?;
        self.require_u16(
            "wallet source sequence digest version",
            REQUIRED_CANONICAL_SEQUENCE_DIGEST_VERSION,
        )?;
        let sequence_block_count = self.read_u64()?;
        let sequence_digest = self.read_array::<32>()?;
        let source_sequence_digest =
            CanonicalBlockFactsSequenceDigest::from_admitted_checkpoint_parts(
                CanonicalBlockFactsSequenceDigestVersion::V1,
                sequence_block_count,
                sequence_digest,
            );
        let projection_digest = WalletProjectionDigest::from_bytes(self.read_array::<32>()?);
        let row_counts = WalletProjectionFamilyRowCounts {
            transparent_unspent_output_count: self.read_u64()?,
            transparent_unspent_output_by_address_count: self.read_u64()?,
            transparent_spent_output_count: self.read_u64()?,
            transparent_address_transaction_count: self.read_u64()?,
            transparent_address_balance_count: self.read_u64()?,
            reorg_undo_count: self.read_u64()?,
        };
        let utxo_count = self.read_u64()?;
        let total_value_zat = self.read_u64()?;
        let scheme_id = self.read_u32()?;
        let commitment_scheme = UtxoSetCommitmentScheme::from_id(scheme_id).ok_or_else(|| {
            WalletProjectionContractError::UnsupportedEncodedValue {
                field: "wallet UTXO commitment scheme",
                encoded: u64::from(scheme_id),
            }
        })?;
        let commitment_bytes = self.read_bytes(UTXO_SET_COMMITMENT_LEN)?;
        let commitment =
            TransparentUtxoSetCommitment::from_parts(commitment_scheme, commitment_bytes).ok_or(
                WalletProjectionContractError::DurableFieldLengthMismatch {
                    field: "wallet UTXO commitment accumulator",
                    expected: UTXO_SET_COMMITMENT_LEN,
                    actual: commitment_bytes.len(),
                },
            )?;
        let evidence = WalletProjectionReadyEvidence {
            source_position,
            source_sequence_digest,
            projection_digest,
            row_counts,
            utxo_summary: WalletUtxoSetSummary {
                utxo_count,
                total_value_zat,
                commitment,
            },
        };
        validate_ready_evidence(&evidence, supported_reorg_depth)?;
        Ok(evidence)
    }

    fn read_source_position(
        &mut self,
    ) -> Result<WalletProjectionSourcePosition, WalletProjectionContractError> {
        Ok(WalletProjectionSourcePosition::new(
            ChainEpochId::new(self.read_u64()?),
            BlockId::new(
                BlockHeight::new(self.read_u32()?),
                BlockHash::from_bytes(self.read_array::<32>()?),
            ),
            self.read_u64()?,
        ))
    }

    fn require_u16(
        &mut self,
        field: &'static str,
        expected: u16,
    ) -> Result<(), WalletProjectionContractError> {
        let encoded = self.read_u16()?;
        if encoded != expected {
            return Err(WalletProjectionContractError::UnsupportedEncodedValue {
                field,
                encoded: u64::from(encoded),
            });
        }
        Ok(())
    }

    fn require_u32(
        &mut self,
        field: &'static str,
        expected: u32,
    ) -> Result<(), WalletProjectionContractError> {
        let encoded = self.read_u32()?;
        if encoded != expected {
            return Err(WalletProjectionContractError::UnsupportedEncodedValue {
                field,
                encoded: u64::from(encoded),
            });
        }
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8, WalletProjectionContractError> {
        Ok(self.read_array::<1>()?[0])
    }

    fn read_u16(&mut self) -> Result<u16, WalletProjectionContractError> {
        Ok(u16::from_be_bytes(self.read_array::<2>()?))
    }

    fn read_u32(&mut self) -> Result<u32, WalletProjectionContractError> {
        Ok(u32::from_be_bytes(self.read_array::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64, WalletProjectionContractError> {
        Ok(u64::from_be_bytes(self.read_array::<8>()?))
    }

    fn read_array<const LEN: usize>(&mut self) -> Result<[u8; LEN], WalletProjectionContractError> {
        self.read_bytes(LEN)?.try_into().map_err(|_| {
            WalletProjectionContractError::DurableFieldLengthMismatch {
                field: "wallet store control field",
                expected: LEN,
                actual: self.encoded.len().saturating_sub(self.offset),
            }
        })
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], WalletProjectionContractError> {
        let end = self.offset.checked_add(len).ok_or(
            WalletProjectionContractError::DurableLengthPrefixMismatch {
                field: "wallet store control",
            },
        )?;
        let Some(bytes) = self.encoded.get(self.offset..end) else {
            return Err(WalletProjectionContractError::DurableValueTooShort {
                field: "wallet store control",
                minimum: end,
                actual: self.encoded.len(),
            });
        };
        self.offset = end;
        Ok(bytes)
    }

    fn finish(self) -> Result<(), WalletProjectionContractError> {
        if self.offset != self.encoded.len() {
            return Err(WalletProjectionContractError::DurableTrailingBytes {
                field: "wallet store control",
            });
        }
        Ok(())
    }
}

fn encode_build_plan(plan: &WalletProjectionBuildPlan, bytes: &mut Vec<u8>) {
    encode_source_position(&plan.target_source_position, bytes);
}

fn encode_source_position(source_position: &WalletProjectionSourcePosition, bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&source_position.chain_epoch_id.value().to_be_bytes());
    bytes.extend_from_slice(&source_position.tip.height.value().to_be_bytes());
    bytes.extend_from_slice(&source_position.tip.hash.as_bytes());
    bytes.extend_from_slice(&source_position.event_sequence.to_be_bytes());
}

fn encode_ready_evidence(
    evidence: &WalletProjectionReadyEvidence,
    supported_reorg_depth: u32,
    bytes: &mut Vec<u8>,
) -> Result<(), WalletProjectionContractError> {
    validate_ready_evidence(evidence, supported_reorg_depth)?;
    encode_source_position(&evidence.source_position, bytes);
    encode_source_sequence_digest(evidence.source_sequence_digest, bytes);
    bytes.extend_from_slice(&evidence.projection_digest.as_bytes());
    let counts = evidence.row_counts;
    bytes.extend_from_slice(&counts.transparent_unspent_output_count.to_be_bytes());
    bytes.extend_from_slice(
        &counts
            .transparent_unspent_output_by_address_count
            .to_be_bytes(),
    );
    bytes.extend_from_slice(&counts.transparent_spent_output_count.to_be_bytes());
    bytes.extend_from_slice(&counts.transparent_address_transaction_count.to_be_bytes());
    bytes.extend_from_slice(&counts.transparent_address_balance_count.to_be_bytes());
    bytes.extend_from_slice(&counts.reorg_undo_count.to_be_bytes());
    bytes.extend_from_slice(&evidence.utxo_summary.utxo_count.to_be_bytes());
    bytes.extend_from_slice(&evidence.utxo_summary.total_value_zat.to_be_bytes());
    bytes.extend_from_slice(&evidence.utxo_summary.commitment.scheme().id().to_be_bytes());
    bytes.extend_from_slice(evidence.utxo_summary.commitment.accumulator());
    Ok(())
}

fn validate_ready_evidence(
    evidence: &WalletProjectionReadyEvidence,
    supported_reorg_depth: u32,
) -> Result<(), WalletProjectionContractError> {
    if evidence.row_counts.transparent_unspent_output_count
        != evidence
            .row_counts
            .transparent_unspent_output_by_address_count
    {
        return Err(WalletProjectionContractError::ReadyUnspentOutputIndexCountMismatch);
    }
    if evidence.row_counts.transparent_unspent_output_count != evidence.utxo_summary.utxo_count {
        return Err(WalletProjectionContractError::ReadyUtxoCountMismatch);
    }
    if evidence.source_sequence_digest.version().value()
        != REQUIRED_CANONICAL_SEQUENCE_DIGEST_VERSION
    {
        return Err(WalletProjectionContractError::ReadySourceSequenceVersionMismatch);
    }
    if evidence.source_sequence_digest.block_count()
        != u64::from(evidence.source_position.tip.height.value())
    {
        return Err(WalletProjectionContractError::ReadySourceSequenceLengthMismatch);
    }
    let expected_reorg_undo_count = u64::from(supported_reorg_depth)
        .min(u64::from(evidence.source_position.tip.height.value()));
    if evidence.row_counts.reorg_undo_count != expected_reorg_undo_count {
        return Err(WalletProjectionContractError::ReadyReorgUndoCountMismatch);
    }
    if evidence.utxo_summary.commitment.scheme() != UtxoSetCommitmentScheme::LtHash16 {
        return Err(WalletProjectionContractError::ReadyUtxoCommitmentSchemeMismatch);
    }
    Ok(())
}

fn encode_source_sequence_digest(digest: CanonicalBlockFactsSequenceDigest, bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&digest.version().value().to_be_bytes());
    bytes.extend_from_slice(&digest.block_count().to_be_bytes());
    bytes.extend_from_slice(&digest.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use zinder_core::{
        BlockHash, BlockHeight, CanonicalBlockFactsDigest, CanonicalBlockFactsDigestVersion,
        CanonicalBlockFactsSequenceDigestBuilder, CanonicalBlockFactsSequenceDigestVersion,
    };

    fn sample_source_position(height: u32) -> WalletProjectionSourcePosition {
        WalletProjectionSourcePosition::new(
            ChainEpochId::new(0x1112_1314_1516_1718),
            BlockId::new(BlockHeight::new(height), BlockHash::from_bytes([0x33; 32])),
            0x2122_2324_2526_2728,
        )
    }

    fn sequence_digest(block_count: u32) -> CanonicalBlockFactsSequenceDigest {
        let mut builder = CanonicalBlockFactsSequenceDigestBuilder::new(
            CanonicalBlockFactsSequenceDigestVersion::V1,
        );
        for index in 0..block_count {
            let digest = CanonicalBlockFactsDigest::from_reference_encoding(
                CanonicalBlockFactsDigestVersion::V1,
                &index.to_be_bytes(),
            );
            assert!(builder.try_append(digest).is_ok());
        }
        builder.finish()
    }

    fn ready_evidence() -> WalletProjectionReadyEvidence {
        let source_position = sample_source_position(1);
        WalletProjectionReadyEvidence {
            source_position,
            source_sequence_digest: sequence_digest(1),
            projection_digest: WalletProjectionDigest::from_bytes([0x77; 32]),
            row_counts: WalletProjectionFamilyRowCounts {
                transparent_unspent_output_count: 1,
                transparent_unspent_output_by_address_count: 1,
                transparent_spent_output_count: 2,
                transparent_address_transaction_count: 3,
                transparent_address_balance_count: 1,
                reorg_undo_count: 1,
            },
            utxo_summary: WalletUtxoSetSummary {
                utxo_count: 1,
                total_value_zat: 9,
                commitment: TransparentUtxoSetCommitment::empty(),
            },
        }
    }

    fn sample_control(build_state: WalletProjectionBuildState) -> WalletStoreControl {
        WalletStoreControl {
            network: Network::ZcashRegtest,
            supported_reorg_depth: 100,
            writer_generation: 0x0102_0304_0506_0708,
            build_state,
        }
    }

    #[test]

    fn building_control_has_exact_version_one_bytes() {
        let control = sample_control(WalletProjectionBuildState::Building(
            WalletProjectionBuildPlan::complete_history(sample_source_position(0x0a0b_0c0d)),
        ));
        let encoded = control
            .encode()
            .unwrap_or_else(|error| unreachable!("valid building control: {error}"));
        assert_eq!(
            hex::encode(&encoded),
            concat!(
                "77616c6c65742d70726f6a656374696f6e",
                "0001",
                "0001",
                "00000003",
                "0001",
                "00000001",
                "0001",
                "0001",
                "00000064",
                "0102030405060708",
                "01",
                "1112131415161718",
                "0a0b0c0d",
                "3333333333333333333333333333333333333333333333333333333333333333",
                "2122232425262728"
            )
        );
        assert_eq!(WalletStoreControl::decode(&encoded), Ok(control));
    }

    #[test]
    fn ready_control_has_exact_version_one_bytes() {
        let evidence = ready_evidence();
        let control = sample_control(WalletProjectionBuildState::Ready(evidence.clone()));
        let mut expected = Vec::new();
        expected.extend_from_slice(WALLET_PROJECTION_STORE_IDENTITY);
        expected.extend_from_slice(&WALLET_PROJECTION_SCHEMA_VERSION.to_be_bytes());
        expected.extend_from_slice(&WALLET_PROJECTION_VALUE_ENCODING_VERSION.to_be_bytes());
        expected.extend_from_slice(&Network::ZcashRegtest.id().to_be_bytes());
        expected.extend_from_slice(&REQUIRED_CANONICAL_STORE_SCHEMA_VERSION.to_be_bytes());
        expected.extend_from_slice(&REQUIRED_CANONICAL_REPLAY_FORMAT_VERSION.to_be_bytes());
        expected.extend_from_slice(&REQUIRED_CANONICAL_FACTS_DIGEST_VERSION.to_be_bytes());
        expected.extend_from_slice(&REQUIRED_CANONICAL_SEQUENCE_DIGEST_VERSION.to_be_bytes());
        expected.extend_from_slice(&100u32.to_be_bytes());
        expected.extend_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        expected.push(2);
        expected.extend_from_slice(
            &evidence
                .source_position
                .chain_epoch_id
                .value()
                .to_be_bytes(),
        );
        expected.extend_from_slice(&evidence.source_position.tip.height.value().to_be_bytes());
        expected.extend_from_slice(&evidence.source_position.tip.hash.as_bytes());
        expected.extend_from_slice(&evidence.source_position.event_sequence.to_be_bytes());
        encode_source_sequence_digest(evidence.source_sequence_digest, &mut expected);
        expected.extend_from_slice(&evidence.projection_digest.as_bytes());
        let counts = evidence.row_counts;
        expected.extend_from_slice(&counts.transparent_unspent_output_count.to_be_bytes());
        expected.extend_from_slice(
            &counts
                .transparent_unspent_output_by_address_count
                .to_be_bytes(),
        );
        expected.extend_from_slice(&counts.transparent_spent_output_count.to_be_bytes());
        expected.extend_from_slice(&counts.transparent_address_transaction_count.to_be_bytes());
        expected.extend_from_slice(&counts.transparent_address_balance_count.to_be_bytes());
        expected.extend_from_slice(&counts.reorg_undo_count.to_be_bytes());
        expected.extend_from_slice(&evidence.utxo_summary.utxo_count.to_be_bytes());
        expected.extend_from_slice(&evidence.utxo_summary.total_value_zat.to_be_bytes());
        expected.extend_from_slice(&evidence.utxo_summary.commitment.scheme().id().to_be_bytes());
        expected.extend_from_slice(evidence.utxo_summary.commitment.accumulator());

        assert_eq!(control.encode(), Ok(expected.clone()));
        assert_eq!(WalletStoreControl::decode(&expected), Ok(control));
    }

    #[test]
    fn control_decode_rejects_unknown_versions_states_and_trailing_bytes() {
        let control = sample_control(WalletProjectionBuildState::Building(
            WalletProjectionBuildPlan::complete_history(sample_source_position(1)),
        ));
        let encoded = control
            .encode()
            .unwrap_or_else(|error| unreachable!("valid building control: {error}"));

        let mut wrong_schema = encoded.clone();
        let schema_offset = WALLET_PROJECTION_STORE_IDENTITY.len();
        wrong_schema[schema_offset..schema_offset + 2].copy_from_slice(&2_u16.to_be_bytes());
        assert!(matches!(
            WalletStoreControl::decode(&wrong_schema),
            Err(WalletProjectionContractError::UnsupportedEncodedValue { .. })
        ));

        let mut unknown_state = encoded.clone();
        let state_offset =
            WALLET_PROJECTION_STORE_IDENTITY.len() + 2 + 2 + 4 + 2 + 4 + 2 + 2 + 4 + 8;
        unknown_state[state_offset] = 9;
        assert!(matches!(
            WalletStoreControl::decode(&unknown_state),
            Err(WalletProjectionContractError::UnsupportedEncodedValue { .. })
        ));

        let mut trailing = encoded;
        trailing.push(0);
        assert!(matches!(
            WalletStoreControl::decode(&trailing),
            Err(WalletProjectionContractError::DurableTrailingBytes { .. })
        ));
    }

    #[test]

    fn ready_control_rejects_inconsistent_evidence() {
        let mut cases = Vec::new();

        let mut evidence = ready_evidence();
        evidence
            .row_counts
            .transparent_unspent_output_by_address_count = 2;
        cases.push((
            evidence,
            WalletProjectionContractError::ReadyUnspentOutputIndexCountMismatch,
        ));

        let mut evidence = ready_evidence();
        evidence.utxo_summary.utxo_count = 2;
        cases.push((
            evidence,
            WalletProjectionContractError::ReadyUtxoCountMismatch,
        ));

        let mut evidence = ready_evidence();
        evidence.row_counts.reorg_undo_count = 0;
        cases.push((
            evidence,
            WalletProjectionContractError::ReadyReorgUndoCountMismatch,
        ));

        let mut evidence = ready_evidence();
        evidence.source_sequence_digest = sequence_digest(0);
        cases.push((
            evidence,
            WalletProjectionContractError::ReadySourceSequenceLengthMismatch,
        ));

        for (evidence, expected_error) in cases {
            let control = sample_control(WalletProjectionBuildState::Ready(evidence));
            assert_eq!(control.encode(), Err(expected_error));
        }
    }
}
