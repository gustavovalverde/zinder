//! Wallet-serving secondary-pair lifecycle for the lightwalletd adapter.
//!
//! The compatibility process owns no canonical or wallet primary handle. It
//! catches up only an inactive generation, authenticates that generation
//! against the canonical writer control plane, and atomically publishes it as
//! one immutable canonical/wallet pair. The prior generation stays open until
//! every in-flight request has dropped its captured pair `Arc`.

use std::{
    num::NonZeroU8,
    path::PathBuf,
    sync::{Arc, Weak},
    time::Duration,
};

use arc_swap::ArcSwap;
use thiserror::Error;
use tokio::{task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;
use zinder_core::{Network, NetworkUpgradeActivations, wire::encode_zinder_native_chain_name};
use zinder_proto::v1::ingest::{
    CanonicalWriterStatusRequest, CanonicalWriterStatusResponse,
    canonical_control_client::CanonicalControlClient,
};
use zinder_query::{
    CanonicalReader, WalletProjectionReader, WalletServingAdmissionError, WalletServingReadPair,
};
use zinder_runtime::{
    AuthenticatedChannel, BearerToken, BearerTokenConnectError, Readiness, ReadinessCause,
    ReadinessState, connect_zinder_grpc,
};
use zinder_store::{
    CanonicalReorgPolicy, CanonicalStoreError, CanonicalStoreWorkload, RocksDbCanonicalSecondary,
    RocksDbResourceBudget,
};
use zinder_wallet_projection::WalletCanonicalSourceIdentity;
use zinder_wallet_rocksdb::{RocksDbWalletError, RocksDbWalletSecondary};

const SECONDARY_GENERATION_COUNT: usize = 2;
const CONVERGENCE_RETRY_DELAY_CAP: Duration = Duration::from_millis(100);

/// Immutable configuration for the bounded secondary-pair lifecycle.
#[derive(Clone, Debug)]
pub(crate) struct WalletServingPairConfig {
    pub(crate) canonical_primary_path: PathBuf,
    pub(crate) canonical_secondary_root: PathBuf,
    pub(crate) wallet_primary_path: PathBuf,
    pub(crate) wallet_secondary_root: PathBuf,
    pub(crate) network: Network,
    pub(crate) network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    pub(crate) canonical_reorg_policy: CanonicalReorgPolicy,
    pub(crate) canonical_resource_budget: RocksDbResourceBudget,
    pub(crate) wallet_resource_budget: RocksDbResourceBudget,
    pub(crate) catchup_interval: Duration,
    pub(crate) convergence_timeout: Duration,
    pub(crate) convergence_attempts: NonZeroU8,
    pub(crate) replica_lag_threshold_chain_epochs: u64,
}

/// Slot captured once by each request before it reads canonical or wallet data.
pub(crate) type WalletServingPairSlot = Arc<ArcSwap<WalletServingReadPair>>;

/// Errors that stop bootstrap or make a refresh generation ineligible.
#[derive(Debug, Error)]
pub(crate) enum WalletServingPairError {
    /// The canonical secondary failed admission or catch-up.
    #[error(transparent)]
    Canonical(#[from] CanonicalStoreError),
    /// The wallet secondary failed admission or catch-up.
    #[error(transparent)]
    Wallet(#[from] RocksDbWalletError),
    /// A bounded secondary generation directory could not be created before opening it.
    #[error("failed to create wallet-serving secondary generation directory {path}: {source}")]
    SecondaryGenerationDirectoryCreate {
        /// Per-generation directory that must exist before `RocksDB` can create its leaf.
        path: PathBuf,
        /// Filesystem failure returned while creating the generation directory.
        #[source]
        source: std::io::Error,
    },
    /// The private canonical-control endpoint could not be connected.
    #[error(transparent)]
    WriterStatusConnect(#[from] BearerTokenConnectError),
    /// The private canonical-control RPC failed.
    #[error("canonical writer-status RPC failed: {0}")]
    WriterStatusRpc(tonic::Status),
    /// Writer status did not contain an exact, usable canonical fence.
    #[error("canonical writer-status response did not contain a valid fence")]
    WriterStatusInvalid,
    /// Writer status disagreed with a same-epoch pair, indicating a fence or
    /// primary-path replacement inconsistency rather than normal lag.
    #[error("canonical writer-status fence disagrees with the serving pair")]
    WriterFenceMismatch,
    /// A blocking secondary operation stopped without a result.
    #[error("secondary candidate task failed: {0}")]
    CandidateTask(#[from] tokio::task::JoinError),
    /// A pair was unexpectedly absent after successful bootstrap.
    #[error("wallet-serving secondary pair slot is unavailable")]
    PairSlotUnavailable,
    /// A candidate was not present in the expected inactive generation.
    #[error("inactive secondary generation {generation} has no candidate")]
    CandidateUnavailable {
        /// Bounded generation slot that was expected to hold a candidate.
        generation: usize,
    },
    /// Convergence exhausted its time or attempt bound without a wallet-serving pair.
    #[error(
        "canonical and wallet secondaries did not converge within the configured bounds; last outcome={last_outcome:?}"
    )]
    ConvergenceTimedOut {
        /// Typed final mismatch observed before the bound expired.
        last_outcome: WalletServingConvergence,
    },
    /// Pair construction changed after pre-publication validation.
    #[error("wallet-serving read pair changed during publication: {0}")]
    PairPublication(zinder_query::QueryError),
}

/// Typed candidate outcome used for readiness and metrics classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WalletServingConvergence {
    /// Canonical and wallet secondaries are exact, but the writer advanced.
    ReplicaBehind,
    /// Canonical is current for the candidate but wallet projection trails it.
    ProjectionBehind,
    /// The admitted networks, canonical fence, or local schema evidence is invalid.
    SchemaOrFenceMismatch,
}

impl WalletServingConvergence {
    const fn label(self) -> &'static str {
        match self {
            Self::ReplicaBehind => "replica_behind",
            Self::ProjectionBehind => "projection_behind",
            Self::SchemaOrFenceMismatch => "schema_or_fence_mismatch",
        }
    }
}

/// Owns the two bounded reader generations and their publication slot.
pub(crate) struct WalletServingPairPublisher {
    config: WalletServingPairConfig,
    writer_status: CanonicalWriterStatusClient,
    readiness: Readiness,
    serving_pair_slot: Option<WalletServingPairSlot>,
    generations: [SecondaryGeneration; SECONDARY_GENERATION_COUNT],
    published_generation: Option<usize>,
}

impl WalletServingPairPublisher {
    /// Opens, converges, and authenticates the first serving pair before the
    /// gRPC listener starts. Bootstrap fails closed instead of serving a
    /// primary handle or a mixed secondary view.
    pub(crate) async fn bootstrap(
        config: WalletServingPairConfig,
        readiness: Readiness,
        writer_status_endpoint: &str,
        bearer_token: Option<&BearerToken>,
    ) -> Result<(Self, WalletServingPairSlot), WalletServingPairError> {
        let writer_status =
            CanonicalWriterStatusClient::connect(writer_status_endpoint, bearer_token).await?;
        let generations =
            std::array::from_fn(|generation| SecondaryGeneration::new(&config, generation));
        let mut publisher = Self {
            config,
            writer_status,
            readiness,
            serving_pair_slot: None,
            generations,
            published_generation: None,
        };
        publisher.prepare_candidate(0).await?;
        publisher.converge_and_publish(0).await?;
        let Some(serving_pair_slot) = publisher.serving_pair_slot.clone() else {
            return Err(WalletServingPairError::PairSlotUnavailable);
        };
        Ok((publisher, serving_pair_slot))
    }

    /// Runs bounded refreshes until shutdown. Every failure leaves the
    /// current immutable pair untouched and updates readiness with its typed
    /// cause; it never falls back to a primary store.
    #[must_use = "await the publisher on shutdown so secondary handles close before process exit"]
    pub(crate) fn spawn(mut self, cancel: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    () = tokio::time::sleep(self.config.catchup_interval) => {
                        if let Err(error) = self.refresh_once().await {
                            self.record_refresh_failure(&error);
                            tracing::warn!(
                                target: "zinder::compat_lightwalletd",
                                event = "wallet_serving_pair_publisher_refresh_failed",
                                error = %error,
                                "inactive wallet-serving pair refresh failed; retaining the prior pair"
                            );
                        }
                    }
                }
            }
        })
    }

    async fn refresh_once(&mut self) -> Result<(), WalletServingPairError> {
        let serving_pair_slot = self
            .serving_pair_slot
            .as_ref()
            .ok_or(WalletServingPairError::PairSlotUnavailable)?;
        let active_pair = serving_pair_slot.load_full();
        let writer_status = self.writer_status.fetch().await?;
        let relation = self.update_active_readiness(&active_pair, &writer_status)?;
        if relation == ActiveWriterRelation::Exact {
            return Ok(());
        }

        let generation = self.inactive_generation();
        if !self.prepare_candidate(generation).await? {
            return Ok(());
        }
        self.converge_and_publish(generation).await
    }

    fn inactive_generation(&self) -> usize {
        self.published_generation.map_or(0, |generation| {
            (generation + 1) % SECONDARY_GENERATION_COUNT
        })
    }

    /// Opens a generation only after the prior pair in that exact path has no
    /// strong references. It deliberately does not delete the metadata path:
    /// opening an inactive path is safe and bounded, while deleting it would
    /// widen the failure surface without improving correctness.
    async fn prepare_candidate(
        &mut self,
        generation: usize,
    ) -> Result<bool, WalletServingPairError> {
        let secondary_generation = &mut self.generations[generation];
        match &secondary_generation.state {
            SecondaryGenerationState::Candidate { .. } => return Ok(true),
            SecondaryGenerationState::Published { lease } if !lease.is_reusable() => {
                metrics::counter!(
                    "zinder_compat_lightwalletd_wallet_serving_pair_publisher_generation_wait_total",
                    "reason" => "in_flight_requests"
                )
                .increment(1);
                return Ok(false);
            }
            SecondaryGenerationState::Vacant | SecondaryGenerationState::Published { .. } => {}
        }
        secondary_generation.state = SecondaryGenerationState::Vacant;
        let canonical_primary_path = self.config.canonical_primary_path.clone();
        let canonical_generation_path = secondary_generation.canonical_generation_path.clone();
        let canonical_secondary_path = secondary_generation.canonical_secondary_path.clone();
        let wallet_primary_path = self.config.wallet_primary_path.clone();
        let wallet_generation_path = secondary_generation.wallet_generation_path.clone();
        let wallet_secondary_path = secondary_generation.wallet_secondary_path.clone();
        let network_upgrade_activations = Arc::clone(&self.config.network_upgrade_activations);
        let canonical_reorg_policy = self.config.canonical_reorg_policy;
        let canonical_resource_budget = self.config.canonical_resource_budget;
        let wallet_resource_budget = self.config.wallet_resource_budget;
        let network = self.config.network;
        let candidate = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&canonical_generation_path).map_err(|source| {
                WalletServingPairError::SecondaryGenerationDirectoryCreate {
                    path: canonical_generation_path.clone(),
                    source,
                }
            })?;
            std::fs::create_dir_all(&wallet_generation_path).map_err(|source| {
                WalletServingPairError::SecondaryGenerationDirectoryCreate {
                    path: wallet_generation_path.clone(),
                    source,
                }
            })?;
            let canonical = RocksDbCanonicalSecondary::open_ready(
                canonical_primary_path,
                canonical_secondary_path,
                network_upgrade_activations.as_ref(),
                CanonicalStoreWorkload::Wallet,
                canonical_reorg_policy,
                canonical_resource_budget,
            )?;
            let wallet = RocksDbWalletSecondary::open_ready(
                wallet_primary_path,
                wallet_secondary_path,
                network,
                wallet_resource_budget,
            )?;
            Ok::<_, WalletServingPairError>(Box::new(SecondaryPairCandidate { canonical, wallet }))
        })
        .await??;
        self.generations[generation].state = SecondaryGenerationState::Candidate { candidate };
        Ok(true)
    }

    async fn converge_and_publish(
        &mut self,
        generation: usize,
    ) -> Result<(), WalletServingPairError> {
        let deadline = Instant::now() + self.config.convergence_timeout;
        let mut last_outcome = WalletServingConvergence::ReplicaBehind;
        for attempt in 0..u32::from(self.config.convergence_attempts.get()) {
            self.catch_up_candidate(generation).await?;
            match self.candidate_convergence(generation)? {
                Ok(()) => {
                    let source = self.candidate_wallet_source(generation)?;
                    let writer_status = self.writer_status.fetch().await?;
                    if writer_status_matches_source(&writer_status, source, self.config.network) {
                        self.publish_candidate(generation)?;
                        return Ok(());
                    }
                    last_outcome = WalletServingConvergence::ReplicaBehind;
                    record_pair_convergence(last_outcome);
                }
                Err(outcome) => {
                    last_outcome = outcome;
                    record_pair_convergence(outcome);
                }
            }
            if Instant::now() >= deadline
                || attempt + 1 == u32::from(self.config.convergence_attempts.get())
            {
                break;
            }
            tokio::time::sleep(
                self.config
                    .catchup_interval
                    .min(CONVERGENCE_RETRY_DELAY_CAP),
            )
            .await;
        }
        Err(WalletServingPairError::ConvergenceTimedOut { last_outcome })
    }

    async fn catch_up_candidate(
        &mut self,
        generation: usize,
    ) -> Result<(), WalletServingPairError> {
        let state = std::mem::replace(
            &mut self.generations[generation].state,
            SecondaryGenerationState::Vacant,
        );
        let SecondaryGenerationState::Candidate { mut candidate } = state else {
            return Err(WalletServingPairError::CandidateUnavailable { generation });
        };
        let started_at = Instant::now();
        let candidate_task_outcome = tokio::task::spawn_blocking(move || {
            candidate.canonical.try_catch_up()?;
            candidate.wallet.try_catch_up()?;
            Ok::<_, WalletServingPairError>(candidate)
        })
        .await;
        match candidate_task_outcome {
            Ok(Ok(candidate)) => {
                metrics::histogram!(
                    "zinder_compat_lightwalletd_wallet_serving_pair_publisher_catchup_duration_seconds",
                    "status" => "ok"
                )
                .record(started_at.elapsed());
                self.generations[generation].state =
                    SecondaryGenerationState::Candidate { candidate };
                Ok(())
            }
            Ok(Err(error)) => {
                metrics::histogram!(
                    "zinder_compat_lightwalletd_wallet_serving_pair_publisher_catchup_duration_seconds",
                    "status" => "error"
                )
                .record(started_at.elapsed());
                Err(error)
            }
            Err(error) => Err(WalletServingPairError::CandidateTask(error)),
        }
    }

    fn candidate_convergence(
        &self,
        generation: usize,
    ) -> Result<Result<(), WalletServingConvergence>, WalletServingPairError> {
        let SecondaryGenerationState::Candidate { candidate } = &self.generations[generation].state
        else {
            return Err(WalletServingPairError::CandidateUnavailable { generation });
        };
        match WalletServingReadPair::validate_readers(&candidate.canonical, &candidate.wallet) {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(classify_pair_admission(&error))),
        }
    }

    fn candidate_wallet_source(
        &self,
        generation: usize,
    ) -> Result<WalletCanonicalSourceIdentity, WalletServingPairError> {
        let SecondaryGenerationState::Candidate { candidate } = &self.generations[generation].state
        else {
            return Err(WalletServingPairError::CandidateUnavailable { generation });
        };
        Ok(WalletCanonicalSourceIdentity::from_ready_evidence(
            candidate.wallet.ready_evidence(),
        ))
    }

    fn publish_candidate(&mut self, generation: usize) -> Result<(), WalletServingPairError> {
        let state = std::mem::replace(
            &mut self.generations[generation].state,
            SecondaryGenerationState::Vacant,
        );
        let SecondaryGenerationState::Candidate { candidate } = state else {
            return Err(WalletServingPairError::CandidateUnavailable { generation });
        };
        let canonical: Arc<dyn CanonicalReader> = Arc::new(candidate.canonical);
        let wallet: Arc<dyn WalletProjectionReader> = Arc::new(candidate.wallet);
        let pair = Arc::new(
            WalletServingReadPair::new(canonical, wallet)
                .map_err(WalletServingPairError::PairPublication)?,
        );
        publish_serving_pair(&mut self.serving_pair_slot, Arc::clone(&pair));
        self.generations[generation].state = SecondaryGenerationState::Published {
            lease: GenerationLease::new(&pair),
        };
        self.published_generation = Some(generation);
        let visible_height = Some(pair.canonical_fence().visible_tip().height.value());
        self.readiness.set(ReadinessState::ready(visible_height));
        metrics::counter!(
            "zinder_compat_lightwalletd_wallet_serving_pair_publisher_publications_total"
        )
        .increment(1);
        tracing::info!(
            target: "zinder::compat_lightwalletd",
            event = "wallet_serving_pair_publisher_published",
            generation,
            chain_epoch = pair.canonical_fence().chain_epoch_id().value(),
            event_sequence = pair.canonical_fence().chain_event_sequence(),
            visible_height = pair.canonical_fence().visible_tip().height.value(),
            "published exact immutable canonical and wallet secondary pair"
        );
        Ok(())
    }

    fn update_active_readiness(
        &self,
        active_pair: &WalletServingReadPair,
        writer_status: &CanonicalWriterStatusResponse,
    ) -> Result<ActiveWriterRelation, WalletServingPairError> {
        let Some(writer_fence) = writer_status.fence.as_ref() else {
            return Err(WalletServingPairError::WriterStatusInvalid);
        };
        if writer_status.network_name != encode_zinder_native_chain_name(self.config.network) {
            return Err(WalletServingPairError::WriterStatusInvalid);
        }
        let active_source = active_pair.wallet_source();
        let active_epoch = active_source.source_position().chain_epoch_id.value();
        if writer_fence.chain_epoch_id < active_epoch {
            return Err(WalletServingPairError::WriterFenceMismatch);
        }
        let visible_height = Some(active_source.source_position().tip.height.value());
        if writer_fence.chain_epoch_id == active_epoch {
            if !writer_status_matches_source(writer_status, active_source, self.config.network) {
                return Err(WalletServingPairError::WriterFenceMismatch);
            }
            record_replica_lag(0);
            self.readiness.set(ReadinessState::ready(visible_height));
            return Ok(ActiveWriterRelation::Exact);
        }
        let lag = writer_fence.chain_epoch_id.saturating_sub(active_epoch);
        record_replica_lag(lag);
        if lag > self.config.replica_lag_threshold_chain_epochs {
            self.readiness
                .set(ReadinessState::replica_lagging(lag, visible_height));
        } else {
            self.readiness.set(ReadinessState::ready_with_target(
                visible_height,
                Some(writer_fence.visible_tip_height),
            ));
        }
        Ok(ActiveWriterRelation::Behind)
    }

    fn record_refresh_failure(&self, error: &WalletServingPairError) {
        let visible_height = self.serving_pair_slot.as_ref().map(|serving_pair_slot| {
            serving_pair_slot
                .load_full()
                .canonical_fence()
                .visible_tip()
                .height
                .value()
        });
        if let Some(cause) = refresh_failure_not_ready_cause(error) {
            self.readiness.set(ReadinessState::not_ready(cause));
        } else {
            self.readiness.set(ReadinessState::replica_lagging(
                self.config
                    .replica_lag_threshold_chain_epochs
                    .saturating_add(1),
                visible_height,
            ));
        }
        metrics::counter!(
            "zinder_compat_lightwalletd_wallet_serving_pair_publisher_refresh_total",
            "status" => "error",
            "error_class" => wallet_serving_pair_error_class(error)
        )
        .increment(1);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveWriterRelation {
    Exact,
    Behind,
}

struct SecondaryGeneration {
    canonical_generation_path: PathBuf,
    canonical_secondary_path: PathBuf,
    wallet_generation_path: PathBuf,
    wallet_secondary_path: PathBuf,
    state: SecondaryGenerationState,
}

impl SecondaryGeneration {
    fn new(config: &WalletServingPairConfig, generation: usize) -> Self {
        let generation_name = format!("generation-{generation}");
        let canonical_generation_path = config.canonical_secondary_root.join(&generation_name);
        let wallet_generation_path = config.wallet_secondary_root.join(generation_name);
        Self {
            canonical_secondary_path: canonical_generation_path.join("canonical"),
            canonical_generation_path,
            wallet_secondary_path: wallet_generation_path.join("wallet"),
            wallet_generation_path,
            state: SecondaryGenerationState::Vacant,
        }
    }
}

enum SecondaryGenerationState {
    Vacant,
    Candidate {
        candidate: Box<SecondaryPairCandidate>,
    },
    Published {
        lease: GenerationLease<WalletServingReadPair>,
    },
}

struct SecondaryPairCandidate {
    canonical: RocksDbCanonicalSecondary,
    wallet: RocksDbWalletSecondary,
}

/// Weak ownership token used to prove a path has no active request readers.
struct GenerationLease<T: ?Sized> {
    pair: Weak<T>,
}

impl<T: ?Sized> GenerationLease<T> {
    fn new(pair: &Arc<T>) -> Self {
        Self {
            pair: Arc::downgrade(pair),
        }
    }

    fn is_reusable(&self) -> bool {
        self.pair.strong_count() == 0
    }
}

/// Publishes a fully admitted immutable reader pair without affecting any
/// request that already captured the prior pair from the slot.
fn publish_serving_pair<T: Send + Sync + 'static>(
    slot: &mut Option<Arc<ArcSwap<T>>>,
    pair: Arc<T>,
) -> Arc<ArcSwap<T>> {
    if let Some(slot) = slot {
        slot.store(pair);
        Arc::clone(slot)
    } else {
        let published_slot = Arc::new(ArcSwap::from(pair));
        *slot = Some(Arc::clone(&published_slot));
        published_slot
    }
}

/// Narrow authenticated reader used only to bind a secondary candidate to the
/// canonical writer at the service boundary immediately before publication.
struct CanonicalWriterStatusClient {
    client: CanonicalControlClient<AuthenticatedChannel>,
}

impl CanonicalWriterStatusClient {
    async fn connect(
        endpoint: &str,
        bearer_token: Option<&BearerToken>,
    ) -> Result<Self, WalletServingPairError> {
        let channel = connect_zinder_grpc(endpoint, bearer_token).await?;
        Ok(Self {
            client: CanonicalControlClient::new(channel),
        })
    }

    async fn fetch(&mut self) -> Result<CanonicalWriterStatusResponse, WalletServingPairError> {
        let started_at = Instant::now();
        let outcome = self
            .client
            .writer_status(CanonicalWriterStatusRequest {})
            .await
            .map(tonic::Response::into_inner)
            .map_err(WalletServingPairError::WriterStatusRpc);
        metrics::histogram!(
            "zinder_compat_lightwalletd_writer_status_duration_seconds",
            "status" => if outcome.is_ok() { "ok" } else { "error" }
        )
        .record(started_at.elapsed());
        metrics::counter!(
            "zinder_compat_lightwalletd_writer_status_total",
            "status" => if outcome.is_ok() { "ok" } else { "error" }
        )
        .increment(1);
        metrics::gauge!("zinder_compat_lightwalletd_writer_status_available")
            .set(if outcome.is_ok() { 1.0 } else { 0.0 });
        outcome
    }
}

fn classify_pair_admission(error: &WalletServingAdmissionError) -> WalletServingConvergence {
    match error {
        WalletServingAdmissionError::WalletSourceMismatch { canonical, wallet } => {
            let canonical_event_sequence = canonical.source_position().event_sequence;
            let wallet_event_sequence = wallet.source_position().event_sequence;
            match canonical_event_sequence.cmp(&wallet_event_sequence) {
                std::cmp::Ordering::Less => WalletServingConvergence::ReplicaBehind,
                std::cmp::Ordering::Greater => WalletServingConvergence::ProjectionBehind,
                std::cmp::Ordering::Equal => WalletServingConvergence::SchemaOrFenceMismatch,
            }
        }
        WalletServingAdmissionError::NetworkMismatch { .. }
        | WalletServingAdmissionError::CanonicalRead { .. }
        | WalletServingAdmissionError::CanonicalFenceMismatch => {
            WalletServingConvergence::SchemaOrFenceMismatch
        }
    }
}

fn writer_status_matches_source(
    status: &CanonicalWriterStatusResponse,
    source: WalletCanonicalSourceIdentity,
    network: Network,
) -> bool {
    let Some(fence) = status.fence.as_ref() else {
        return false;
    };
    let source_position = source.source_position();
    status.network_name == encode_zinder_native_chain_name(network)
        && fence.chain_epoch_id == source_position.chain_epoch_id.value()
        && fence.event_sequence == source_position.event_sequence
        && fence.visible_tip_height == source_position.tip.height.value()
        && fence.visible_tip_hash == source_position.tip.hash.as_bytes()
        && fence.visible_block_count == source.source_sequence_digest().block_count()
        && fence.canonical_sequence_digest == source.source_sequence_digest().as_bytes()
}

fn record_pair_convergence(outcome: WalletServingConvergence) {
    metrics::counter!(
        "zinder_compat_lightwalletd_wallet_serving_pair_publisher_convergence_total",
        "outcome" => outcome.label()
    )
    .increment(1);
}

fn record_replica_lag(lag_chain_epochs: u64) {
    #[allow(
        clippy::cast_precision_loss,
        reason = "The metric is a bounded operational lag signal; readiness compares the original integer."
    )]
    let lag = lag_chain_epochs as f64;
    metrics::gauge!(
        "zinder_compat_lightwalletd_wallet_serving_pair_publisher_replica_lag_chain_epochs"
    )
    .set(lag);
}

/// Returns the fail-closed readiness cause for a refresh error. `None` is
/// reserved for a normally typed replica-behind outcome.
fn refresh_failure_not_ready_cause(error: &WalletServingPairError) -> Option<ReadinessCause> {
    match error {
        WalletServingPairError::WriterStatusConnect(_)
        | WalletServingPairError::WriterStatusRpc(_) => {
            Some(ReadinessCause::WriterStatusUnavailable)
        }
        WalletServingPairError::WriterStatusInvalid
        | WalletServingPairError::WriterFenceMismatch
        | WalletServingPairError::ConvergenceTimedOut {
            last_outcome: WalletServingConvergence::SchemaOrFenceMismatch,
        } => Some(ReadinessCause::SchemaMismatch),
        WalletServingPairError::ConvergenceTimedOut {
            last_outcome: WalletServingConvergence::ReplicaBehind,
        } => None,
        WalletServingPairError::ConvergenceTimedOut {
            last_outcome: WalletServingConvergence::ProjectionBehind,
        }
        | WalletServingPairError::Canonical(_)
        | WalletServingPairError::Wallet(_)
        | WalletServingPairError::SecondaryGenerationDirectoryCreate { .. }
        | WalletServingPairError::CandidateTask(_)
        | WalletServingPairError::PairPublication(_)
        | WalletServingPairError::PairSlotUnavailable
        | WalletServingPairError::CandidateUnavailable { .. } => {
            Some(ReadinessCause::StorageUnavailable)
        }
    }
}

fn wallet_serving_pair_error_class(error: &WalletServingPairError) -> &'static str {
    match error {
        WalletServingPairError::Canonical(_) => "canonical_secondary",
        WalletServingPairError::Wallet(_) => "wallet_secondary",
        WalletServingPairError::SecondaryGenerationDirectoryCreate { .. } => {
            "generation_directory_create"
        }
        WalletServingPairError::WriterStatusConnect(_) => "writer_status_connect",
        WalletServingPairError::WriterStatusRpc(_) => "writer_status_rpc",
        WalletServingPairError::WriterStatusInvalid => "writer_status_invalid",
        WalletServingPairError::WriterFenceMismatch => "writer_fence_mismatch",
        WalletServingPairError::CandidateTask(_) => "candidate_task",
        WalletServingPairError::PairSlotUnavailable => "pair_slot_unavailable",
        WalletServingPairError::CandidateUnavailable { .. } => "candidate_unavailable",
        WalletServingPairError::ConvergenceTimedOut {
            last_outcome: WalletServingConvergence::ReplicaBehind,
        } => "replica_behind",
        WalletServingPairError::ConvergenceTimedOut {
            last_outcome: WalletServingConvergence::ProjectionBehind,
        } => "projection_behind",
        WalletServingPairError::ConvergenceTimedOut {
            last_outcome: WalletServingConvergence::SchemaOrFenceMismatch,
        } => "schema_or_fence_mismatch",
        WalletServingPairError::PairPublication(_) => "pair_publication",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        num::{NonZeroU32, NonZeroU64},
        path::Path,
        sync::Arc,
        time::Duration,
    };

    use arc_swap::ArcSwap;
    use parking_lot::Mutex;
    use prost::Message as _;
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tokio_util::sync::CancellationToken;
    use tonic::{Request, Response, Status, transport::Server};
    use zinder_core::{
        BlockHash, BlockHeaderArtifact, BlockHeight, BlockHeightRange, BlockId,
        CanonicalBlockFacts, CanonicalBlockFactsDigestVersion,
        CanonicalBlockFactsSequenceDigestVersion, CanonicalBlockReplayFormatVersion, ChainEpochId,
        ChainTipMetadata, CommitmentTreeCheckpoint, CommitmentTreeFrontiers, ConsensusBranchId,
        Network, NetworkUpgradeActivation, NetworkUpgradeActivations, SerializedBytesDigest,
        UnixTimestampMillis, encode_canonical_block_replay,
        wire::{encode_internal_block_hash, encode_zinder_native_chain_name},
    };
    use zinder_proto::{
        compat::lightwalletd::{ChainMetadata, CompactBlock as LightwalletdCompactBlock},
        v1::ingest::{
            AcquireCanonicalProjectionBuildLeaseRequest, CanonicalEventPageRequest,
            CanonicalEventPageResponse, CanonicalProjectionBuildLeaseResponse,
            CanonicalWriterFence, CanonicalWriterStatusRequest, CanonicalWriterStatusResponse,
            CreateCanonicalOwnerCheckpointRequest, CreateCanonicalOwnerCheckpointResponse,
            ReadmitCanonicalOwnerCheckpointRequest, ReleaseCanonicalProjectionBuildLeaseRequest,
            ReleaseCanonicalProjectionBuildLeaseResponse,
            RenewCanonicalProjectionBuildLeaseRequest,
            canonical_control_server::{CanonicalControl, CanonicalControlServer},
        },
    };
    use zinder_store::{
        CanonicalBaselinePublication, CanonicalBuildBlock, CanonicalEventFence,
        CanonicalEventHistoryRequest, CanonicalLiveAppend, CanonicalReorgPolicy,
        CanonicalStoreBuildPlan, CanonicalStoreWorkload, RocksDbCanonicalBuilder,
        RocksDbCanonicalSecondary, RocksDbCanonicalStore, RocksDbResourceBudget,
    };
    use zinder_wallet_projection::{WalletCanonicalSourceIdentity, WalletProjectionSourcePosition};
    use zinder_wallet_rocksdb::{
        RocksDbWalletBuildOptions, RocksDbWalletStore, build_wallet_from_canonical,
    };

    use super::{
        GenerationLease, SecondaryGenerationState, WalletServingConvergence,
        WalletServingPairConfig, WalletServingPairError, WalletServingPairPublisher,
        classify_pair_admission, publish_serving_pair, refresh_failure_not_ready_cause,
        writer_status_matches_source,
    };

    #[test]
    fn exact_source_is_eligible_for_wallet_serving_pair_publisher_publication() {
        let source = source_identity(3, 0x33);

        assert!(writer_status_matches_source(
            &writer_status(source),
            source,
            Network::ZcashRegtest,
        ));
    }

    #[test]
    fn mutated_writer_fence_is_rejected_before_wallet_serving_pair_publisher_publication() {
        let source = source_identity(3, 0x33);
        let mut status = writer_status(source);
        if let Some(fence) = status.fence.as_mut() {
            fence.canonical_sequence_digest[0] ^= 0xff;
        }

        assert!(!writer_status_matches_source(
            &status,
            source,
            Network::ZcashRegtest,
        ));
    }

    #[test]
    fn equal_cursor_but_mutated_pair_evidence_is_a_schema_or_fence_mismatch() {
        let canonical = source_identity(3, 0x33);
        let wallet = source_identity(3, 0x44);
        let error = zinder_query::WalletServingAdmissionError::WalletSourceMismatch {
            canonical: Box::new(canonical),
            wallet: Box::new(wallet),
        };

        assert_eq!(
            classify_pair_admission(&error),
            WalletServingConvergence::SchemaOrFenceMismatch
        );
    }

    #[test]
    fn atomic_publication_keeps_a_retired_generation_until_every_request_arc_drains() {
        let published_pair = Arc::new(());
        let lease = GenerationLease::new(&published_pair);
        let initial_slot = Arc::new(ArcSwap::from(Arc::clone(&published_pair)));
        let in_flight_request = initial_slot.load_full();
        let mut slot = Some(initial_slot);
        let replacement_pair = Arc::new(());

        let active_slot = publish_serving_pair(&mut slot, Arc::clone(&replacement_pair));
        let active_pair = active_slot.load_full();
        assert!(Arc::ptr_eq(&active_pair, &replacement_pair));

        // This simulates ArcSwap replacing the published pair. The old
        // request retains a strong Arc, so its secondary paths are not safe
        // to reopen or delete yet.
        drop(published_pair);
        assert!(!lease.is_reusable());

        drop(in_flight_request);
        assert!(lease.is_reusable());
    }

    #[test]
    fn schema_or_fence_convergence_failure_is_not_ready() {
        assert_eq!(
            refresh_failure_not_ready_cause(&WalletServingPairError::ConvergenceTimedOut {
                last_outcome: WalletServingConvergence::SchemaOrFenceMismatch,
            }),
            Some(zinder_runtime::ReadinessCause::SchemaMismatch)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[expect(
        clippy::too_many_lines,
        reason = "the cold process lifecycle test keeps primary advancement, secondary rotation, and fail-closed publication in one causal scenario"
    )]
    async fn cold_lifecycle_rotates_exact_secondaries_and_rejects_a_mutated_writer_fence()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let activations = lifecycle_upgrade_activations()?;
        let canonical_primary_path = temporary.path().join("canonical-primary");
        let wallet_primary_path = temporary.path().join("wallet-primary");
        let canonical_secondary_root = temporary.path().join("canonical-secondaries");
        let wallet_secondary_root = temporary.path().join("wallet-secondaries");
        let mut canonical_primary =
            build_lifecycle_canonical_primary(&canonical_primary_path, &activations)?;
        let wallet_outcome = build_wallet_from_canonical(
            &canonical_primary,
            &wallet_primary_path,
            RocksDbWalletBuildOptions {
                supported_reorg_depth: 100,
                ..RocksDbWalletBuildOptions::for_local_tests()
            },
        )?;
        let initial_source = wallet_outcome.report.canonical_source_identity();
        let mut wallet_primary = wallet_outcome.store;
        let control = MutableCanonicalControl::new(writer_status_for_store(&canonical_primary));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        let server_task = tokio::spawn({
            let control = control.clone();
            async move {
                Server::builder()
                    .add_service(CanonicalControlServer::new(control))
                    .serve_with_incoming_shutdown(
                        TcpListenerStream::new(listener),
                        server_cancel.cancelled_owned(),
                    )
                    .await
            }
        });

        assert!(!canonical_secondary_root.exists());
        assert!(!wallet_secondary_root.exists());
        assert!(
            !canonical_secondary_root
                .join("generation-0")
                .join("canonical")
                .exists()
        );
        assert!(
            !wallet_secondary_root
                .join("generation-0")
                .join("wallet")
                .exists()
        );
        let readiness = zinder_runtime::Readiness::default();
        let (mut publisher, slot) = WalletServingPairPublisher::bootstrap(
            wallet_serving_pair_config(
                canonical_primary_path.clone(),
                canonical_secondary_root.clone(),
                wallet_primary_path.clone(),
                wallet_secondary_root.clone(),
                Arc::new(activations.clone()),
            )?,
            readiness.clone(),
            &endpoint,
            None,
        )
        .await?;
        let old_pair = slot.load_full();
        assert_eq!(old_pair.canonical_fence(), canonical_primary.event_fence());
        assert_eq!(old_pair.wallet_source(), initial_source);
        assert!(
            canonical_secondary_root
                .join("generation-0")
                .join("canonical")
                .exists()
        );
        assert!(
            wallet_secondary_root
                .join("generation-0")
                .join("wallet")
                .exists()
        );

        let initial_fence = canonical_primary.event_fence();
        let initial_settled_tip = initial_fence.visible_tip();
        let (next_canonical_primary, append_fence) = canonical_primary.commit_live_append(
            CanonicalLiveAppend::new(
                initial_fence,
                lifecycle_build_block(
                    BlockHeight::new(2),
                    BlockHash::from_bytes([0x22; 32]),
                    initial_fence.visible_tip().hash,
                ),
                Vec::new(),
                initial_settled_tip,
                UnixTimestampMillis::new(1_750_000_000_001),
            ),
            &activations,
        )?;
        canonical_primary = next_canonical_primary;
        let wallet_reconcile_secondary = RocksDbCanonicalSecondary::open_ready(
            &canonical_primary_path,
            temporary
                .path()
                .join("wallet-reconcile-canonical-secondary"),
            &activations,
            CanonicalStoreWorkload::Wallet,
            CanonicalReorgPolicy::new(100)?,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        let updated_source = reconcile_wallet_primary(
            &mut wallet_primary,
            &wallet_reconcile_secondary,
            initial_source,
            append_fence,
            initial_settled_tip,
        )?;
        assert_eq!(
            WalletCanonicalSourceIdentity::from_ready_evidence(wallet_primary.ready_evidence()),
            updated_source
        );
        drop(wallet_reconcile_secondary);

        control.set_writer_status(writer_status_for_store(&canonical_primary));
        publisher.refresh_once().await?;
        let refreshed_pair = slot.load_full();
        assert!(!Arc::ptr_eq(&old_pair, &refreshed_pair));
        assert_eq!(refreshed_pair.canonical_fence(), append_fence);
        assert_eq!(refreshed_pair.wallet_source(), updated_source);
        assert_eq!(published_generation_is_reusable(&publisher, 0), Some(false));
        assert!(
            canonical_secondary_root
                .join("generation-1")
                .join("canonical")
                .exists()
        );
        assert!(
            wallet_secondary_root
                .join("generation-1")
                .join("wallet")
                .exists()
        );

        drop(old_pair);
        assert_eq!(published_generation_is_reusable(&publisher, 0), Some(true));

        let mut mutated_status = writer_status_for_store(&canonical_primary);
        let writer_fence = mutated_status
            .fence
            .as_mut()
            .ok_or("writer status fixture must contain a fence")?;
        let digest_byte = writer_fence
            .canonical_sequence_digest
            .first_mut()
            .ok_or("writer status fixture must contain a digest")?;
        *digest_byte ^= 0xff;
        control.set_writer_status(mutated_status);
        let failure = publisher
            .refresh_once()
            .await
            .err()
            .ok_or("mutated writer fence must fail refresh")?;
        assert!(matches!(
            failure,
            WalletServingPairError::WriterFenceMismatch
        ));
        publisher.record_refresh_failure(&failure);
        let pair_after_failure = slot.load_full();
        assert!(Arc::ptr_eq(&refreshed_pair, &pair_after_failure));
        assert!(matches!(
            readiness.report().cause,
            zinder_runtime::ReadinessCause::SchemaMismatch
        ));

        drop(pair_after_failure);
        drop(refreshed_pair);
        drop(publisher);
        drop(slot);
        drop(wallet_primary);
        drop(canonical_primary);
        cancel.cancel();
        server_task.await??;
        Ok(())
    }

    fn writer_status(source: WalletCanonicalSourceIdentity) -> CanonicalWriterStatusResponse {
        let position = source.source_position();
        CanonicalWriterStatusResponse {
            network_name: "zcash-regtest".to_owned(),
            fence: Some(CanonicalWriterFence {
                chain_epoch_id: position.chain_epoch_id.value(),
                event_sequence: position.event_sequence,
                visible_tip_height: position.tip.height.value(),
                visible_tip_hash: position.tip.hash.as_bytes().to_vec(),
                visible_block_count: source.source_sequence_digest().block_count(),
                canonical_sequence_digest: source.source_sequence_digest().as_bytes().to_vec(),
            }),
            oldest_retained_event_sequence: 1,
        }
    }

    fn source_identity(
        event_sequence: u64,
        sequence_digest_byte: u8,
    ) -> WalletCanonicalSourceIdentity {
        let tip = BlockId::new(BlockHeight::new(3), BlockHash::from_bytes([0x33; 32]));
        WalletCanonicalSourceIdentity::new(
            WalletProjectionSourcePosition::new(ChainEpochId::new(3), tip, event_sequence),
            zinder_core::CanonicalBlockFactsSequenceDigest::from_admitted_checkpoint_parts(
                CanonicalBlockFactsSequenceDigestVersion::V1,
                3,
                [sequence_digest_byte; 32],
            ),
            tip,
        )
    }

    fn wallet_serving_pair_config(
        canonical_primary_path: std::path::PathBuf,
        canonical_secondary_root: std::path::PathBuf,
        wallet_primary_path: std::path::PathBuf,
        wallet_secondary_root: std::path::PathBuf,
        network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    ) -> Result<WalletServingPairConfig, Box<dyn std::error::Error>> {
        Ok(WalletServingPairConfig {
            canonical_primary_path,
            canonical_secondary_root,
            wallet_primary_path,
            wallet_secondary_root,
            network: Network::ZcashRegtest,
            network_upgrade_activations,
            canonical_reorg_policy: CanonicalReorgPolicy::new(100)?,
            canonical_resource_budget: RocksDbResourceBudget::for_local_tests(),
            wallet_resource_budget: RocksDbResourceBudget::for_local_tests(),
            catchup_interval: Duration::from_millis(1),
            convergence_timeout: Duration::from_secs(2),
            convergence_attempts: NonZeroU32::new(4)
                .and_then(|attempts| u8::try_from(attempts.get()).ok())
                .and_then(std::num::NonZeroU8::new)
                .ok_or("wallet-serving pair convergence attempts must be non-zero")?,
            replica_lag_threshold_chain_epochs: 4,
        })
    }

    fn build_lifecycle_canonical_primary(
        canonical_primary_path: &Path,
        activations: &NetworkUpgradeActivations,
    ) -> Result<RocksDbCanonicalStore, Box<dyn std::error::Error>> {
        let tip = BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([0x11; 32]));
        let mut builder = RocksDbCanonicalBuilder::create_fresh(
            canonical_primary_path,
            CanonicalStoreWorkload::Wallet,
            CanonicalStoreBuildPlan::complete(
                activations,
                0,
                tip,
                CanonicalReorgPolicy::new(100)?,
            )?,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        builder.bulk_load_blocks(std::iter::once(Ok::<_, std::io::Error>(
            lifecycle_build_block(tip.height, tip.hash, Network::ZcashRegtest.genesis_hash()),
        )))?;
        builder.load_subtree_roots(std::iter::empty())?;
        builder.confirm_source_tip_checkpoint(&CommitmentTreeCheckpoint::new(
            tip,
            1,
            CommitmentTreeFrontiers::default(),
        ))?;
        let validated = builder.prepare_cold_certified_publication()?;
        let publication = validated.prepare_baseline(CanonicalBaselinePublication::new(
            tip,
            UnixTimestampMillis::new(1_750_000_000_000),
        ))?;
        Ok(validated.publish_baseline(publication)?)
    }

    fn lifecycle_build_block(
        height: BlockHeight,
        block_hash: BlockHash,
        parent_hash: BlockHash,
    ) -> CanonicalBuildBlock {
        let facts = CanonicalBlockFacts {
            block_header: BlockHeaderArtifact::new(
                height,
                block_hash,
                parent_hash,
                [0; 32],
                [0; 32],
                i64::from(height.value()),
                0,
                [0; 32],
                0,
                0,
            ),
            serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(
                &block_hash.as_bytes(),
            ),
            transactions: Vec::new(),
        };
        let compact_payload = LightwalletdCompactBlock {
            height: u64::from(height.value()),
            hash: encode_internal_block_hash(block_hash).to_vec(),
            prev_hash: encode_internal_block_hash(parent_hash).to_vec(),
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 0,
                ironwood_commitment_tree_size: 0,
            }),
            ..Default::default()
        }
        .encode_to_vec();
        CanonicalBuildBlock {
            replay_envelope: encode_canonical_block_replay(
                &facts,
                CanonicalBlockReplayFormatVersion::V1,
                CanonicalBlockFactsDigestVersion::V1,
            ),
            compact_block: zinder_core::CompactBlockArtifact::new(
                height,
                block_hash,
                compact_payload,
            ),
            tip_metadata: ChainTipMetadata::new(0, 0, 0),
            tree_state_checkpoint: Some(CommitmentTreeCheckpoint::new(
                BlockId::new(height, block_hash),
                height.value(),
                CommitmentTreeFrontiers::default(),
            )),
            block_final_note_commitment_roots: None,
            transaction_blobs: Vec::new(),
            block_blob: None,
            facts,
        }
    }

    fn reconcile_wallet_primary(
        wallet_primary: &mut RocksDbWalletStore,
        canonical_secondary: &RocksDbCanonicalSecondary,
        initial_source: WalletCanonicalSourceIdentity,
        target_fence: CanonicalEventFence,
        target_settled_tip: BlockId,
    ) -> Result<WalletCanonicalSourceIdentity, Box<dyn std::error::Error>> {
        let source_cursor = initial_source.source_position().event_cursor.as_bytes();
        let retained_events =
            canonical_secondary.canonical_event_history(CanonicalEventHistoryRequest::new(
                Some(&source_cursor),
                NonZeroU32::new(16).ok_or("canonical event page limit must be non-zero")?,
            ))?;
        let replay_range =
            BlockHeightRange::inclusive(BlockHeight::new(2), target_fence.visible_tip().height);
        wallet_primary.reconcile_canonical_event_sequence(
            initial_source,
            &retained_events,
            target_fence,
            target_settled_tip,
            None,
            replay_range,
            NonZeroU64::new(512_u64 * 1024 * 1024)
                .ok_or("wallet transition byte limit must be non-zero")?,
            canonical_secondary.scan_canonical_replay_range(replay_range)?,
        )?;
        Ok(WalletCanonicalSourceIdentity::from_ready_evidence(
            wallet_primary.ready_evidence(),
        ))
    }

    fn writer_status_for_store(store: &RocksDbCanonicalStore) -> CanonicalWriterStatusResponse {
        writer_status_for_fence(store.event_fence())
    }

    fn writer_status_for_fence(fence: CanonicalEventFence) -> CanonicalWriterStatusResponse {
        CanonicalWriterStatusResponse {
            network_name: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
            fence: Some(CanonicalWriterFence {
                chain_epoch_id: fence.chain_epoch_id().value(),
                event_sequence: fence.chain_event_sequence(),
                visible_tip_height: fence.visible_tip().height.value(),
                visible_tip_hash: fence.visible_tip().hash.as_bytes().to_vec(),
                visible_block_count: fence.sequence_digest().block_count(),
                canonical_sequence_digest: fence.sequence_digest().as_bytes().to_vec(),
            }),
            oldest_retained_event_sequence: 1,
        }
    }

    fn published_generation_is_reusable(
        publisher: &WalletServingPairPublisher,
        generation: usize,
    ) -> Option<bool> {
        match &publisher.generations[generation].state {
            SecondaryGenerationState::Published { lease } => Some(lease.is_reusable()),
            SecondaryGenerationState::Vacant | SecondaryGenerationState::Candidate { .. } => None,
        }
    }

    fn lifecycle_upgrade_activations()
    -> Result<NetworkUpgradeActivations, Box<dyn std::error::Error>> {
        let activations = [
            ("Overwinter", 1_u32),
            ("Sapling", 2),
            ("Blossom", 3),
            ("Heartwood", 4),
            ("Canopy", 5),
            ("NU5", 6),
            ("NU6", 7),
            ("NU6.1", 8),
            ("NU6.2", 9),
            ("NU6.3", 10),
        ]
        .into_iter()
        .map(|(name, branch_id)| NetworkUpgradeActivation {
            branch_id: ConsensusBranchId::new(branch_id),
            activation_height: BlockHeight::new(100),
            name: name.to_owned(),
        })
        .collect();
        Ok(NetworkUpgradeActivations::new(
            Network::ZcashRegtest,
            activations,
        )?)
    }

    #[derive(Clone)]
    struct MutableCanonicalControl {
        writer_status: Arc<Mutex<CanonicalWriterStatusResponse>>,
    }

    impl MutableCanonicalControl {
        fn new(writer_status: CanonicalWriterStatusResponse) -> Self {
            Self {
                writer_status: Arc::new(Mutex::new(writer_status)),
            }
        }

        fn set_writer_status(&self, writer_status: CanonicalWriterStatusResponse) {
            *self.writer_status.lock() = writer_status;
        }
    }

    #[tonic::async_trait]
    impl CanonicalControl for MutableCanonicalControl {
        async fn writer_status(
            &self,
            _request: Request<CanonicalWriterStatusRequest>,
        ) -> Result<Response<CanonicalWriterStatusResponse>, Status> {
            Ok(Response::new(self.writer_status.lock().clone()))
        }

        async fn event_page(
            &self,
            _request: Request<CanonicalEventPageRequest>,
        ) -> Result<Response<CanonicalEventPageResponse>, Status> {
            Err(Status::unimplemented("fixture only serves writer status"))
        }

        async fn create_owner_checkpoint(
            &self,
            _request: Request<CreateCanonicalOwnerCheckpointRequest>,
        ) -> Result<Response<CreateCanonicalOwnerCheckpointResponse>, Status> {
            Err(Status::unimplemented("fixture only serves writer status"))
        }

        async fn readmit_owner_checkpoint(
            &self,
            _request: Request<ReadmitCanonicalOwnerCheckpointRequest>,
        ) -> Result<Response<CreateCanonicalOwnerCheckpointResponse>, Status> {
            Err(Status::unimplemented("fixture only serves writer status"))
        }

        async fn acquire_projection_build_lease(
            &self,
            _request: Request<AcquireCanonicalProjectionBuildLeaseRequest>,
        ) -> Result<Response<CanonicalProjectionBuildLeaseResponse>, Status> {
            Err(Status::unimplemented("fixture only serves writer status"))
        }

        async fn renew_projection_build_lease(
            &self,
            _request: Request<RenewCanonicalProjectionBuildLeaseRequest>,
        ) -> Result<Response<CanonicalProjectionBuildLeaseResponse>, Status> {
            Err(Status::unimplemented("fixture only serves writer status"))
        }

        async fn release_projection_build_lease(
            &self,
            _request: Request<ReleaseCanonicalProjectionBuildLeaseRequest>,
        ) -> Result<Response<ReleaseCanonicalProjectionBuildLeaseResponse>, Status> {
            Err(Status::unimplemented("fixture only serves writer status"))
        }
    }
}
