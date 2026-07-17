#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tonic::Request;
use zinder_core::{
    BlockHash, BlockHeight, BlockId, ChainValuePool, ChainValuePools, Network, ShieldedProtocol,
    SubtreeRootIndex,
};
use zinder_derive::{
    ProjectionPreset, TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
};
use zinder_ingest::IngestControlGrpcAdapter;
use zinder_proto::capabilities::INGEST_CONTROL_CHAIN_VALUE_POOLS_AT_TIP_V1;
use zinder_proto::v1::{
    ingest::{ServerInfoRequest, ingest_control_server::IngestControl as IngestControlService},
    wallet::ChainValuePoolsAtTipRequest,
};
use zinder_source::{
    NodeCapabilities, NodeCapability, NodeSource, SourceBlock, SourceError, SourceSubtreeRoots,
};
use zinder_testkit::StoreFixture;

#[tokio::test]
async fn ingest_control_reports_the_selected_projection_workload() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let adapter = IngestControlGrpcAdapter::new(
        Network::ZcashRegtest,
        store_fixture.chain_store().clone(),
        zinder_runtime::Readiness::default(),
    )
    .with_projection_preset(ProjectionPreset::Wallet);

    let info = IngestControlService::server_info(&adapter, Request::new(ServerInfoRequest {}))
        .await?
        .into_inner()
        .server_info
        .ok_or_else(|| eyre::eyre!("server_info missing"))?;

    assert_eq!(info.projection_preset, "wallet");
    assert_eq!(
        info.projection_identities,
        [
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME
                .as_str()
                .to_owned(),
            TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME.as_str().to_owned(),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn ingest_control_advertises_value_pools_only_when_source_supports_them() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let adapter_without_source = IngestControlGrpcAdapter::new(
        Network::ZcashRegtest,
        store_fixture.chain_store().clone(),
        zinder_runtime::Readiness::default(),
    );
    let info_without_source = IngestControlService::server_info(
        &adapter_without_source,
        Request::new(ServerInfoRequest {}),
    )
    .await?
    .into_inner()
    .server_info
    .ok_or_else(|| eyre::eyre!("server_info missing"))?;

    assert!(
        !info_without_source
            .capabilities
            .iter()
            .any(|capability| capability == INGEST_CONTROL_CHAIN_VALUE_POOLS_AT_TIP_V1)
    );

    let adapter_with_source = IngestControlGrpcAdapter::new(
        Network::ZcashRegtest,
        store_fixture.chain_store().clone(),
        zinder_runtime::Readiness::default(),
    )
    .with_node_source(Arc::new(StaticValuePoolSource::new(
        NodeCapabilities::new([NodeCapability::ChainValuePools])?,
        ChainValuePools::new(
            BlockId::new(BlockHeight::new(2), BlockHash::from_bytes([2; 32])),
            Vec::new(),
        ),
    )));
    let info_with_source =
        IngestControlService::server_info(&adapter_with_source, Request::new(ServerInfoRequest {}))
            .await?
            .into_inner()
            .server_info
            .ok_or_else(|| eyre::eyre!("server_info missing"))?;

    assert!(
        info_with_source
            .capabilities
            .iter()
            .any(|capability| capability == INGEST_CONTROL_CHAIN_VALUE_POOLS_AT_TIP_V1)
    );
    Ok(())
}

#[tokio::test]
async fn ingest_control_chain_value_pools_at_tip_uses_node_source() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let expected_epoch = store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let adapter = IngestControlGrpcAdapter::new(
        Network::ZcashRegtest,
        store_fixture.chain_store().clone(),
        zinder_runtime::Readiness::default(),
    )
    .with_node_source(Arc::new(StaticValuePoolSource::new(
        NodeCapabilities::new([NodeCapability::ChainValuePools])?,
        ChainValuePools::new(
            BlockId::new(
                BlockHeight::new(1_234),
                BlockHash::from_bytes([
                    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
                    0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19,
                    0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
                ]),
            ),
            vec![
                ChainValuePool::new("transparent", true, Some(100)),
                ChainValuePool::new("lockbox", false, None),
            ],
        ),
    )));

    let response = IngestControlService::chain_value_pools_at_tip(
        &adapter,
        Request::new(ChainValuePoolsAtTipRequest {}),
    )
    .await?
    .into_inner();

    let chain_epoch = response
        .chain_view
        .clone()
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| eyre::eyre!("chain_view.chain_epoch missing"))?;
    assert_eq!(chain_epoch.chain_epoch_id, expected_epoch.id.value());
    let source_tip = response
        .source_tip
        .ok_or_else(|| eyre::eyre!("source_tip missing"))?;
    assert_eq!(source_tip.height, 1_234);
    assert_eq!(
        source_tip.hash,
        "1f1e1d1c1b1a191817161514131211100f0e0d0c0b0a09080706050403020100"
    );
    assert_eq!(response.pools.len(), 2);
    assert_eq!(response.pools[0].id, "transparent");
    assert!(response.pools[0].monitored);
    assert_eq!(response.pools[0].chain_value_zat, Some(100));
    assert_eq!(response.pools[1].id, "lockbox");
    assert!(!response.pools[1].monitored);
    assert_eq!(response.pools[1].chain_value_zat, None);
    Ok(())
}

#[derive(Clone)]
struct StaticValuePoolSource {
    capabilities: NodeCapabilities,
    value_pools: ChainValuePools,
}

impl StaticValuePoolSource {
    fn new(capabilities: NodeCapabilities, value_pools: ChainValuePools) -> Self {
        Self {
            capabilities,
            value_pools,
        }
    }
}

#[async_trait]
impl NodeSource for StaticValuePoolSource {
    fn capabilities(&self) -> NodeCapabilities {
        self.capabilities
    }

    async fn fetch_block_at(
        &self,
        height: BlockHeight,
    ) -> std::result::Result<SourceBlock, SourceError> {
        Err(SourceError::BlockUnavailable {
            height,
            reason: "static source does not serve blocks".to_owned(),
        })
    }

    async fn tip_id(&self) -> std::result::Result<BlockId, SourceError> {
        Err(SourceError::NodeUnavailable {
            reason: "static source does not serve tip ids".to_owned(),
        })
    }

    async fn fetch_subtree_roots(
        &self,
        protocol: ShieldedProtocol,
        start_index: SubtreeRootIndex,
        max_entries: std::num::NonZeroU32,
    ) -> std::result::Result<SourceSubtreeRoots, SourceError> {
        let _ = (protocol, start_index, max_entries);
        Err(SourceError::NodeCapabilityMissing {
            capability: NodeCapability::SubtreeRoots,
        })
    }

    async fn fetch_chain_value_pools_at_tip(
        &self,
    ) -> std::result::Result<ChainValuePools, SourceError> {
        Ok(self.value_pools.clone())
    }
}
