use zinder_core::{
    BlockHash, BlockHeight, BlockId, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsSequenceDigestVersion, CanonicalBlockReplayFormatVersion,
    CanonicalHistoryBounds, ChainEpochId, Network,
};

use super::{
    CANONICAL_STORE_IDENTITY, CANONICAL_STORE_SCHEMA_VERSION, CanonicalStoreBuildPlan,
    CanonicalStoreBuildState, CanonicalStoreError, CanonicalStoreReadyEvidence,
    CanonicalStoreWorkload,
};

const BUILDING_STATE: u8 = 1;
const READY_STATE: u8 = 2;
const COMPLETE_HISTORY: u8 = 0;
const CHECKPOINTED_HISTORY: u8 = 1;
const WALLET_WORKLOAD: u8 = 1;
const EXPLORER_WORKLOAD: u8 = 2;
const HISTORY_FIELDS_LENGTH: usize = 1 + 4 + 32;
const BUILD_TIP_FIELDS_LENGTH: usize = 4 + 32;
const READY_FIELDS_LENGTH: usize = 4 + 32 + 4 + 32 + 8 + 8 + 2 + 4 + 2 + 32 + 8;
const STORE_CONTROL_LENGTH: usize = CANONICAL_STORE_IDENTITY.len()
    + 2
    + 4
    + 1
    + HISTORY_FIELDS_LENGTH
    + BUILD_TIP_FIELDS_LENGTH
    + 32
    + 1
    + READY_FIELDS_LENGTH;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DecodedStoreControl {
    pub(super) network: Network,
    pub(super) workload: CanonicalStoreWorkload,
    pub(super) build_plan: CanonicalStoreBuildPlan,
    pub(super) cursor_auth_key: [u8; 32],
    pub(super) build_state: CanonicalStoreBuildState,
}

pub(super) fn encode_building_store_control(
    workload: CanonicalStoreWorkload,
    build_plan: CanonicalStoreBuildPlan,
    cursor_auth_key: [u8; 32],
) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(STORE_CONTROL_LENGTH);
    encoded.extend_from_slice(CANONICAL_STORE_IDENTITY.as_bytes());
    encoded.extend_from_slice(&CANONICAL_STORE_SCHEMA_VERSION.to_le_bytes());
    encoded.extend_from_slice(&build_plan.network().id().to_le_bytes());
    encoded.push(match workload {
        CanonicalStoreWorkload::Wallet => WALLET_WORKLOAD,
        CanonicalStoreWorkload::Explorer => EXPLORER_WORKLOAD,
    });
    match build_plan.history_bounds().preceding_checkpoint() {
        None => {
            encoded.push(COMPLETE_HISTORY);
            encoded.extend_from_slice(
                &build_plan
                    .history_predecessor()
                    .height
                    .value()
                    .to_le_bytes(),
            );
            encoded.extend_from_slice(&build_plan.history_predecessor().hash.as_bytes());
        }
        Some(checkpoint) => {
            encoded.push(CHECKPOINTED_HISTORY);
            encoded.extend_from_slice(&checkpoint.height.value().to_le_bytes());
            encoded.extend_from_slice(&checkpoint.hash.as_bytes());
        }
    }
    encoded.extend_from_slice(&build_plan.build_tip().height.value().to_le_bytes());
    encoded.extend_from_slice(&build_plan.build_tip().hash.as_bytes());
    encoded.extend_from_slice(&cursor_auth_key);
    encoded.push(BUILDING_STATE);
    encoded.resize(STORE_CONTROL_LENGTH, 0);
    encoded
}

pub(super) fn decode_store_control(
    path: &std::path::Path,
    encoded: &[u8],
) -> Result<DecodedStoreControl, CanonicalStoreError> {
    let mut decoder = Decoder::new(path, encoded);
    let identity = decoder.read_bytes(CANONICAL_STORE_IDENTITY.len(), "identity")?;
    if identity != CANONICAL_STORE_IDENTITY.as_bytes() {
        return Err(CanonicalStoreError::admission(
            path,
            format!("store identity must be exactly {CANONICAL_STORE_IDENTITY:?}"),
        ));
    }
    let schema_version = decoder.read_u16("schema version")?;
    if schema_version != CANONICAL_STORE_SCHEMA_VERSION {
        return Err(CanonicalStoreError::admission(
            path,
            format!(
                "store schema version {schema_version} does not equal required version {CANONICAL_STORE_SCHEMA_VERSION}"
            ),
        ));
    }
    let network_id = decoder.read_u32("network")?;
    let network = Network::from_id(network_id).ok_or_else(|| {
        CanonicalStoreError::admission(
            path,
            format!("store control contains unknown network id {network_id}"),
        )
    })?;
    let workload = match decoder.read_u8("workload")? {
        WALLET_WORKLOAD => CanonicalStoreWorkload::Wallet,
        EXPLORER_WORKLOAD => CanonicalStoreWorkload::Explorer,
        workload => {
            return Err(CanonicalStoreError::admission(
                path,
                format!("store control contains unknown workload {workload}"),
            ));
        }
    };
    let (history_bounds, history_predecessor) = decode_history_bounds(&mut decoder)?;
    let build_tip = BlockId::new(
        BlockHeight::new(decoder.read_u32("build tip height")?),
        BlockHash::from_bytes(decoder.read_array::<32>("build tip hash")?),
    );
    let build_plan = CanonicalStoreBuildPlan::from_parts(
        network,
        history_bounds,
        history_predecessor,
        build_tip,
    )
    .map_err(|source| CanonicalStoreError::admission(path, source.to_string()))?;
    let cursor_auth_key = decoder.read_array::<32>("cursor authentication key")?;
    let build_state = match decoder.read_u8("build state")? {
        BUILDING_STATE => {
            decoder.reject_nonzero_building_fields()?;
            CanonicalStoreBuildState::Building
        }
        READY_STATE => CanonicalStoreBuildState::Ready(
            decoder.decode_ready_evidence(history_bounds, build_tip)?,
        ),
        state => {
            return Err(CanonicalStoreError::admission(
                path,
                format!("store control contains unknown build state {state}"),
            ));
        }
    };
    decoder.reject_trailing_bytes()?;
    Ok(DecodedStoreControl {
        network,
        workload,
        build_plan,
        cursor_auth_key,
        build_state,
    })
}

fn decode_history_bounds(
    decoder: &mut Decoder<'_>,
) -> Result<(CanonicalHistoryBounds, BlockId), CanonicalStoreError> {
    let kind = decoder.read_u8("history kind")?;
    let predecessor_height = decoder.read_u32("history predecessor height")?;
    let predecessor_hash = decoder.read_array::<32>("history predecessor hash")?;
    let history_predecessor = BlockId::new(
        BlockHeight::new(predecessor_height),
        BlockHash::from_bytes(predecessor_hash),
    );
    match kind {
        COMPLETE_HISTORY if predecessor_height == 0 => {
            Ok((CanonicalHistoryBounds::complete(), history_predecessor))
        }
        COMPLETE_HISTORY => Err(CanonicalStoreError::admission(
            decoder.path,
            "complete history predecessor must be the height-zero block",
        )),
        CHECKPOINTED_HISTORY => CanonicalHistoryBounds::checkpointed(history_predecessor)
            .map(|history_bounds| (history_bounds, history_predecessor))
            .map_err(|source| CanonicalStoreError::admission(decoder.path, source.to_string())),
        _ => Err(CanonicalStoreError::admission(
            decoder.path,
            format!("store control contains unknown history kind {kind}"),
        )),
    }
}

struct Decoder<'encoded> {
    path: &'encoded std::path::Path,
    encoded: &'encoded [u8],
    position: usize,
}

impl<'encoded> Decoder<'encoded> {
    const fn new(path: &'encoded std::path::Path, encoded: &'encoded [u8]) -> Self {
        Self {
            path,
            encoded,
            position: 0,
        }
    }

    fn decode_ready_evidence(
        &mut self,
        history_bounds: CanonicalHistoryBounds,
        build_tip: BlockId,
    ) -> Result<CanonicalStoreReadyEvidence, CanonicalStoreError> {
        let first_height = self.read_u32("first height")?;
        let first_hash = self.read_array::<32>("first hash")?;
        let tip_height = self.read_u32("tip height")?;
        let tip_hash = self.read_array::<32>("tip hash")?;
        let visible_epoch = self.read_u64("visible epoch")?;
        let block_count = self.read_u64("block count")?;
        let block_digest_version =
            CanonicalBlockFactsDigestVersion::try_from(self.read_u16("block digest version")?)
                .map_err(|source| CanonicalStoreError::admission(self.path, source.to_string()))?;
        let replay_format_version =
            CanonicalBlockReplayFormatVersion::try_from(self.read_u32("replay format version")?)
                .map_err(|source| CanonicalStoreError::admission(self.path, source.to_string()))?;
        let sequence_digest_version = CanonicalBlockFactsSequenceDigestVersion::try_from(
            self.read_u16("sequence digest version")?,
        )
        .map_err(|source| CanonicalStoreError::admission(self.path, source.to_string()))?;
        let sequence_digest = self.read_array::<32>("sequence digest")?;
        let logical_fact_bytes = self.read_u64("logical fact bytes")?;

        let expected_block_count = tip_height
            .checked_sub(first_height)
            .map(u64::from)
            .and_then(|height_span| height_span.checked_add(1));
        if first_height != history_bounds.first_available_height().value()
            || tip_height != build_tip.height.value()
            || tip_hash != build_tip.hash.as_bytes()
            || visible_epoch == 0
            || expected_block_count != Some(block_count)
        {
            return Err(CanonicalStoreError::admission(
                self.path,
                "ready store control has an invalid chain position",
            ));
        }
        if logical_fact_bytes == 0 {
            return Err(CanonicalStoreError::admission(
                self.path,
                "ready store control is missing validation evidence",
            ));
        }
        if block_digest_version != CanonicalBlockFactsDigestVersion::V1
            || replay_format_version != CanonicalBlockReplayFormatVersion::V1
            || sequence_digest_version != CanonicalBlockFactsSequenceDigestVersion::V1
        {
            return Err(CanonicalStoreError::admission(
                self.path,
                "ready store control contracts must all be version 1",
            ));
        }
        Ok(CanonicalStoreReadyEvidence {
            first_height: BlockHeight::new(first_height),
            first_hash: BlockHash::from_bytes(first_hash),
            tip_height: BlockHeight::new(tip_height),
            tip_hash: BlockHash::from_bytes(tip_hash),
            visible_epoch: ChainEpochId::new(visible_epoch),
            block_count,
            block_digest_version,
            replay_format_version,
            sequence_digest_version,
            sequence_digest,
            logical_fact_bytes,
        })
    }

    fn reject_nonzero_building_fields(&mut self) -> Result<(), CanonicalStoreError> {
        let ready_fields = self.read_bytes(READY_FIELDS_LENGTH, "reserved ready fields")?;
        if ready_fields.iter().any(|byte| *byte != 0) {
            return Err(CanonicalStoreError::admission(
                self.path,
                "building store control contains ready-state evidence",
            ));
        }
        Ok(())
    }

    fn read_u8(&mut self, field: &'static str) -> Result<u8, CanonicalStoreError> {
        Ok(self.read_bytes(1, field)?[0])
    }

    fn read_u16(&mut self, field: &'static str) -> Result<u16, CanonicalStoreError> {
        Ok(u16::from_le_bytes(self.read_array(field)?))
    }

    fn read_u32(&mut self, field: &'static str) -> Result<u32, CanonicalStoreError> {
        Ok(u32::from_le_bytes(self.read_array(field)?))
    }

    fn read_u64(&mut self, field: &'static str) -> Result<u64, CanonicalStoreError> {
        Ok(u64::from_le_bytes(self.read_array(field)?))
    }

    fn read_array<const LENGTH: usize>(
        &mut self,
        field: &'static str,
    ) -> Result<[u8; LENGTH], CanonicalStoreError> {
        let mut array = [0; LENGTH];
        array.copy_from_slice(self.read_bytes(LENGTH, field)?);
        Ok(array)
    }

    fn read_bytes(
        &mut self,
        length: usize,
        field: &'static str,
    ) -> Result<&'encoded [u8], CanonicalStoreError> {
        let end = self.position.checked_add(length).ok_or_else(|| {
            CanonicalStoreError::admission(self.path, "store control offset overflow")
        })?;
        let bytes = self.encoded.get(self.position..end).ok_or_else(|| {
            CanonicalStoreError::admission(self.path, format!("store control {field} is truncated"))
        })?;
        self.position = end;
        Ok(bytes)
    }

    fn reject_trailing_bytes(&self) -> Result<(), CanonicalStoreError> {
        if self.position != self.encoded.len() {
            return Err(CanonicalStoreError::admission(
                self.path,
                format!(
                    "store control contains {} trailing bytes",
                    self.encoded.len().saturating_sub(self.position)
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const CURSOR_AUTH_KEY: [u8; 32] = [7; 32];

    #[test]
    fn building_control_round_trips_every_network() -> Result<(), Box<dyn std::error::Error>> {
        for network in [
            Network::ZcashMainnet,
            Network::ZcashTestnet,
            Network::ZcashRegtest,
        ] {
            for workload in [
                CanonicalStoreWorkload::Wallet,
                CanonicalStoreWorkload::Explorer,
            ] {
                let encoded = encode_building_store_control(
                    workload,
                    complete_build_plan(network),
                    CURSOR_AUTH_KEY,
                );
                assert_eq!(
                    decode_store_control(Path::new("canonical"), &encoded)?,
                    DecodedStoreControl {
                        network,
                        workload,
                        build_plan: complete_build_plan(network),
                        cursor_auth_key: CURSOR_AUTH_KEY,
                        build_state: CanonicalStoreBuildState::Building,
                    }
                );
            }
        }

        let checkpoint = BlockId::new(BlockHeight::new(99), BlockHash::from_bytes([9; 32]));
        let history_bounds = CanonicalHistoryBounds::checkpointed(checkpoint)?;
        let encoded = encode_building_store_control(
            CanonicalStoreWorkload::Wallet,
            checkpointed_build_plan(checkpoint, history_bounds),
            CURSOR_AUTH_KEY,
        );
        assert_eq!(
            decode_store_control(Path::new("canonical"), &encoded)?
                .build_plan
                .history_bounds(),
            history_bounds
        );
        Ok(())
    }

    #[test]
    fn ready_control_requires_complete_version_one_evidence() -> Result<(), CanonicalStoreError> {
        let encoded = valid_ready_control();
        let decoded_ready = decode_store_control(Path::new("canonical"), &encoded)?;
        assert_eq!(decoded_ready, expected_ready_control());

        let decoded_building = decode_store_control(
            Path::new("canonical"),
            &encode_building_store_control(
                CanonicalStoreWorkload::Explorer,
                complete_build_plan(Network::ZcashTestnet),
                CURSOR_AUTH_KEY,
            ),
        )?;
        assert_ne!(decoded_building, decoded_ready);

        let ready_fields = CANONICAL_STORE_IDENTITY.len()
            + 2
            + 4
            + 1
            + HISTORY_FIELDS_LENGTH
            + BUILD_TIP_FIELDS_LENGTH
            + 32
            + 1;
        let visible_epoch = ready_fields + 4 + 32 + 4 + 32;
        let mut changed_epoch = encoded.clone();
        changed_epoch[visible_epoch..visible_epoch + 8].copy_from_slice(&2_u64.to_le_bytes());
        assert_ne!(
            decode_store_control(Path::new("canonical"), &changed_epoch)?,
            decoded_ready
        );

        let block_count = visible_epoch + 8;
        let mut wrong_count = encoded.clone();
        wrong_count[block_count..block_count + 8].copy_from_slice(&1_u64.to_le_bytes());
        let error = decode_store_control(Path::new("canonical"), &wrong_count)
            .err()
            .ok_or_else(|| {
                CanonicalStoreError::admission(Path::new("canonical"), "expected error")
            })?;
        assert!(error.to_string().contains("invalid chain position"));

        let mut wrong_version = encoded;
        let block_digest_version = block_count + 8;
        wrong_version[block_digest_version..block_digest_version + 2]
            .copy_from_slice(&2_u16.to_le_bytes());
        let error = decode_store_control(Path::new("canonical"), &wrong_version)
            .err()
            .ok_or_else(|| {
                CanonicalStoreError::admission(Path::new("canonical"), "expected error")
            })?;
        assert!(
            error
                .to_string()
                .contains("unsupported canonical block facts digest version 2")
        );
        Ok(())
    }

    #[test]
    fn malformed_control_variants_fail_closed() -> Result<(), CanonicalStoreError> {
        let base = encode_building_store_control(
            CanonicalStoreWorkload::Explorer,
            complete_build_plan(Network::ZcashTestnet),
            CURSOR_AUTH_KEY,
        );
        let identity_end = CANONICAL_STORE_IDENTITY.len();
        let network_start = identity_end + 2;
        let workload_offset = network_start + 4;
        let history_kind = workload_offset + 1;
        let build_tip = history_kind + HISTORY_FIELDS_LENGTH;
        let cursor_auth_key = build_tip + BUILD_TIP_FIELDS_LENGTH;
        let state_offset = cursor_auth_key + 32;
        let ready_fields_start = state_offset + 1;
        let cases = [
            {
                let mut encoded = base.clone();
                encoded[0] ^= 0xff;
                (encoded, "identity")
            },
            {
                let mut encoded = base.clone();
                encoded[network_start..network_start + 4].copy_from_slice(&99_u32.to_le_bytes());
                (encoded, "unknown network")
            },
            {
                let mut encoded = base.clone();
                encoded[network_start..network_start + 4]
                    .copy_from_slice(&Network::ZcashMainnet.id().to_le_bytes());
                (encoded, "history predecessor")
            },
            {
                let mut encoded = base.clone();
                encoded[workload_offset] = 99;
                (encoded, "unknown workload")
            },
            {
                let mut encoded = base.clone();
                encoded[history_kind] = 99;
                (encoded, "unknown history kind")
            },
            {
                let mut encoded = base.clone();
                encoded[state_offset] = 99;
                (encoded, "unknown build state")
            },
            {
                let mut encoded = base.clone();
                encoded[ready_fields_start] = 1;
                (encoded, "ready-state evidence")
            },
            {
                let mut encoded = base.clone();
                encoded.pop();
                (encoded, "truncated")
            },
            {
                let mut encoded = base;
                encoded.push(0);
                (encoded, "trailing bytes")
            },
        ];

        for (encoded, expected_reason) in cases {
            let error = decode_store_control(Path::new("canonical"), &encoded)
                .err()
                .ok_or_else(|| {
                    CanonicalStoreError::admission(Path::new("canonical"), "expected error")
                })?;
            assert!(error.to_string().contains(expected_reason), "{error}");
        }
        Ok(())
    }

    fn valid_ready_control() -> Vec<u8> {
        let mut encoded = Vec::with_capacity(STORE_CONTROL_LENGTH);
        encoded.extend_from_slice(CANONICAL_STORE_IDENTITY.as_bytes());
        encoded.extend_from_slice(&CANONICAL_STORE_SCHEMA_VERSION.to_le_bytes());
        encoded.extend_from_slice(&Network::ZcashTestnet.id().to_le_bytes());
        encoded.push(EXPLORER_WORKLOAD);
        encoded.push(COMPLETE_HISTORY);
        encoded.extend_from_slice(&0_u32.to_le_bytes());
        encoded.extend_from_slice(&Network::ZcashTestnet.genesis_hash().as_bytes());
        encoded.extend_from_slice(&2_u32.to_le_bytes());
        encoded.extend_from_slice(&[2; 32]);
        encoded.extend_from_slice(&CURSOR_AUTH_KEY);
        encoded.push(READY_STATE);
        encoded.extend_from_slice(&1_u32.to_le_bytes());
        encoded.extend_from_slice(&[1; 32]);
        encoded.extend_from_slice(&2_u32.to_le_bytes());
        encoded.extend_from_slice(&[2; 32]);
        encoded.extend_from_slice(&1_u64.to_le_bytes());
        encoded.extend_from_slice(&2_u64.to_le_bytes());
        encoded.extend_from_slice(&CanonicalBlockFactsDigestVersion::V1.value().to_le_bytes());
        encoded.extend_from_slice(&CanonicalBlockReplayFormatVersion::V1.value().to_le_bytes());
        encoded.extend_from_slice(
            &CanonicalBlockFactsSequenceDigestVersion::V1
                .value()
                .to_le_bytes(),
        );
        encoded.extend_from_slice(&[3; 32]);
        encoded.extend_from_slice(&1_u64.to_le_bytes());
        assert_eq!(encoded.len(), STORE_CONTROL_LENGTH);
        encoded
    }

    fn expected_ready_control() -> DecodedStoreControl {
        DecodedStoreControl {
            network: Network::ZcashTestnet,
            workload: CanonicalStoreWorkload::Explorer,
            build_plan: complete_build_plan(Network::ZcashTestnet),
            cursor_auth_key: CURSOR_AUTH_KEY,
            build_state: CanonicalStoreBuildState::Ready(CanonicalStoreReadyEvidence {
                first_height: BlockHeight::new(1),
                first_hash: BlockHash::from_bytes([1; 32]),
                tip_height: BlockHeight::new(2),
                tip_hash: BlockHash::from_bytes([2; 32]),
                visible_epoch: ChainEpochId::new(1),
                block_count: 2,
                block_digest_version: CanonicalBlockFactsDigestVersion::V1,
                replay_format_version: CanonicalBlockReplayFormatVersion::V1,
                sequence_digest_version: CanonicalBlockFactsSequenceDigestVersion::V1,
                sequence_digest: [3; 32],
                logical_fact_bytes: 1,
            }),
        }
    }

    fn complete_build_plan(network: Network) -> CanonicalStoreBuildPlan {
        CanonicalStoreBuildPlan {
            network,
            history_bounds: CanonicalHistoryBounds::complete(),
            history_predecessor: BlockId::new(BlockHeight::new(0), network.genesis_hash()),
            build_tip: BlockId::new(BlockHeight::new(2), BlockHash::from_bytes([2; 32])),
        }
    }

    fn checkpointed_build_plan(
        checkpoint: BlockId,
        history_bounds: CanonicalHistoryBounds,
    ) -> CanonicalStoreBuildPlan {
        CanonicalStoreBuildPlan {
            network: Network::ZcashTestnet,
            history_bounds,
            history_predecessor: checkpoint,
            build_tip: BlockId::new(BlockHeight::new(100), BlockHash::from_bytes([10; 32])),
        }
    }
}
