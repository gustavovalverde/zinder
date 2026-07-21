//! Authenticated wallet chain-event projection from canonical retained events.

use zinder_core::{BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId};

use crate::{
    ChainEpochCommitted, ChainEvent, ChainEventEnvelope, ChainEventHistoryRequest,
    ChainEventStreamFamily, ChainEventStreamResume, ChainRangeReverted, EventStreamStartPosition,
    StreamCursorTokenV1,
    format::{CHAIN_EVENT_LOCATOR_MAX, ChainEventCursorAnchor, ChainEventLocator},
};

use super::{
    CanonicalEventKind, CanonicalRetainedEvent, CanonicalStoreError, RocksDbCanonicalSecondary,
    displaced_archive::first_displaced_block_hash_after_event,
    event_lifecycle::read_retained_event_from_db,
};

#[derive(Clone, Debug)]
struct WalletEventResume {
    start_sequence: u64,
    family: ChainEventStreamFamily,
    synthetic_reorg: Option<ChainEventEnvelope>,
}

impl RocksDbCanonicalSecondary {
    /// Projects retained canonical transitions into authenticated wallet events.
    pub fn wallet_chain_event_history(
        &self,
        request: ChainEventHistoryRequest<'_>,
    ) -> Result<Vec<ChainEventEnvelope>, CanonicalStoreError> {
        let current_event_sequence = self.ready_evidence.visible_event_sequence;
        let oldest_retained_sequence = self.canonical_event_retention_floor()?;
        let resume = self.wallet_event_resume(
            request.from_cursor,
            request.family,
            current_event_sequence,
            oldest_retained_sequence,
        )?;
        let mut envelopes = Vec::with_capacity(request.max_events.get() as usize);
        if let Some(synthetic_reorg) = resume.synthetic_reorg {
            envelopes.push(synthetic_reorg);
        }
        let mut sequence = resume.start_sequence;
        while sequence <= current_event_sequence
            && envelopes.len() < request.max_events.get() as usize
        {
            let event = read_retained_event_from_db(&self.bounded_open.db, sequence)?;
            if self.wallet_event_matches_family(event, resume.family)? {
                envelopes.push(self.wallet_event_envelope(event, resume.family)?);
            }
            sequence = sequence.checked_add(1).ok_or(
                CanonicalStoreError::CanonicalEventCursorMalformed {
                    reason: "event sequence cannot advance",
                },
            )?;
        }
        Ok(envelopes)
    }

    /// Resolves and authenticates one wallet subscription start position.
    pub fn resolve_wallet_chain_event_stream_start(
        &self,
        start: &EventStreamStartPosition,
        requested_family: ChainEventStreamFamily,
    ) -> Result<ChainEventStreamResume, CanonicalStoreError> {
        match start {
            EventStreamStartPosition::AfterCursor(cursor) => {
                let payload = cursor
                    .decode_chain_event(self.network(), self.cursor_auth_key)
                    .map_err(|_| CanonicalStoreError::CanonicalEventCursorMalformed {
                        reason: "cursor token failed validation",
                    })?;
                if requested_family != ChainEventStreamFamily::Visible
                    && requested_family != payload.family
                {
                    return Err(CanonicalStoreError::CanonicalEventCursorMalformed {
                        reason: "request family does not match the cursor's encoded family",
                    });
                }
                Ok(ChainEventStreamResume {
                    cursor: Some(cursor.clone()),
                    family: payload.family,
                })
            }
            EventStreamStartPosition::EarliestRetained => Ok(ChainEventStreamResume {
                cursor: None,
                family: requested_family,
            }),
            EventStreamStartPosition::LiveTail => {
                let event_sequence = self.ready_evidence.visible_event_sequence;
                if event_sequence == 0 {
                    return Ok(ChainEventStreamResume {
                        cursor: None,
                        family: requested_family,
                    });
                }
                let chain_epoch = self.chain_epoch()?;
                Ok(ChainEventStreamResume {
                    cursor: Some(self.mint_wallet_event_cursor(
                        chain_epoch,
                        requested_family,
                        event_sequence,
                    )?),
                    family: requested_family,
                })
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Cursor authentication, retention, and fork recovery form one fail-closed validation sequence."
    )]
    fn wallet_event_resume(
        &self,
        cursor: Option<&StreamCursorTokenV1>,
        requested_family: ChainEventStreamFamily,
        current_event_sequence: u64,
        oldest_retained_sequence: u64,
    ) -> Result<WalletEventResume, CanonicalStoreError> {
        let Some(cursor) = cursor else {
            return Ok(WalletEventResume {
                start_sequence: oldest_retained_sequence,
                family: requested_family,
                synthetic_reorg: None,
            });
        };
        let payload = cursor
            .decode_chain_event(self.network(), self.cursor_auth_key)
            .map_err(|_| CanonicalStoreError::CanonicalEventCursorMalformed {
                reason: "cursor token failed validation",
            })?;
        if payload.event_sequence == 0 || payload.event_sequence > current_event_sequence {
            return Err(CanonicalStoreError::CanonicalEventCursorMalformed {
                reason: "cursor sequence is outside canonical history",
            });
        }
        let current_epoch = self.chain_epoch()?;
        let mut fork_point = None;
        for anchor in payload.locator.entries() {
            if self
                .block_header_at(anchor.height)?
                .is_some_and(|header| header.block_hash == anchor.hash)
            {
                fork_point = Some(*anchor);
                break;
            }
        }
        let Some(fork_point) = fork_point else {
            return Err(CanonicalStoreError::CanonicalEventCursorExpired {
                event_sequence: payload.event_sequence,
                oldest_retained_sequence,
            });
        };
        if fork_point == payload.locator.tip() {
            let start_sequence = payload.event_sequence.checked_add(1).ok_or(
                CanonicalStoreError::CanonicalEventCursorMalformed {
                    reason: "cursor sequence cannot advance",
                },
            )?;
            if start_sequence < oldest_retained_sequence {
                return Err(CanonicalStoreError::CanonicalEventCursorExpired {
                    event_sequence: payload.event_sequence,
                    oldest_retained_sequence,
                });
            }
            return Ok(WalletEventResume {
                start_sequence,
                family: payload.family,
                synthetic_reorg: None,
            });
        }
        if payload.family == ChainEventStreamFamily::Settled {
            return Err(CanonicalStoreError::CanonicalEventCursorExpired {
                event_sequence: payload.event_sequence,
                oldest_retained_sequence,
            });
        }
        if payload.event_sequence >= oldest_retained_sequence {
            return Ok(WalletEventResume {
                start_sequence: payload.event_sequence.checked_add(1).ok_or(
                    CanonicalStoreError::CanonicalEventCursorMalformed {
                        reason: "cursor sequence cannot advance",
                    },
                )?,
                family: payload.family,
                synthetic_reorg: None,
            });
        }
        let reverted_start =
            fork_point
                .height
                .next()
                .ok_or(CanonicalStoreError::CanonicalEventCursorMalformed {
                    reason: "fork height cannot advance",
                })?;
        let reverted_epoch = self.chain_epoch_at(ChainEpochId::new(payload.event_sequence))?;
        if payload.locator.tip()
            != (ChainEventCursorAnchor {
                height: reverted_epoch.visible_tip_height,
                hash: reverted_epoch.visible_tip_hash,
            })
        {
            return Err(CanonicalStoreError::CanonicalEventCursorMalformed {
                reason: "cursor tip does not match its historical chain epoch",
            });
        }
        let cursor_sequence = oldest_retained_sequence.saturating_sub(1);
        let cursor = self.mint_wallet_event_cursor_at_anchor(
            current_epoch,
            payload.family,
            cursor_sequence,
            fork_point,
        )?;
        let event = ChainEvent::ChainReorged {
            reverted: ChainRangeReverted::new(
                reverted_epoch,
                BlockHeightRange::inclusive(reverted_start, payload.locator.tip().height),
            ),
            committed: ChainEpochCommitted::new(
                current_epoch,
                BlockHeightRange::inclusive(reverted_start, current_epoch.visible_tip_height),
            ),
        };
        Ok(WalletEventResume {
            start_sequence: oldest_retained_sequence,
            family: payload.family,
            synthetic_reorg: Some(ChainEventEnvelope::new(
                cursor,
                cursor_sequence,
                current_epoch,
                current_epoch.settled_tip_height,
                event,
            )),
        })
    }

    fn wallet_event_matches_family(
        &self,
        event: CanonicalRetainedEvent,
        family: ChainEventStreamFamily,
    ) -> Result<bool, CanonicalStoreError> {
        if family == ChainEventStreamFamily::Visible {
            return Ok(true);
        }
        let epoch = self.chain_epoch_at(event.resulting_epoch_id())?;
        Ok(event.kind() == CanonicalEventKind::Committed
            && epoch.visible_tip_height <= epoch.settled_tip_height)
    }

    fn wallet_event_envelope(
        &self,
        event: CanonicalRetainedEvent,
        family: ChainEventStreamFamily,
    ) -> Result<ChainEventEnvelope, CanonicalStoreError> {
        let chain_epoch = self.chain_epoch_at(event.resulting_epoch_id())?;
        let committed = ChainEpochCommitted::new(chain_epoch, event.committed_range());
        let wallet_event = match event.kind() {
            CanonicalEventKind::Committed => ChainEvent::ChainCommitted { committed },
            CanonicalEventKind::Reorged => {
                let previous_epoch_id = event.previous_epoch_id().ok_or_else(|| {
                    CanonicalStoreError::CanonicalEventRecordMalformed {
                        event_sequence: event.cursor().event_sequence(),
                        reason: "reorg event has no previous epoch",
                    }
                })?;
                let previous_epoch = self.chain_epoch_at(previous_epoch_id)?;
                let reverted_range = event.reverted_range().ok_or_else(|| {
                    CanonicalStoreError::CanonicalEventRecordMalformed {
                        event_sequence: event.cursor().event_sequence(),
                        reason: "reorg event has no reverted range",
                    }
                })?;
                ChainEvent::ChainReorged {
                    reverted: ChainRangeReverted::new(previous_epoch, reverted_range),
                    committed,
                }
            }
        };
        let event_sequence = event.cursor().event_sequence();
        let cursor = self.mint_wallet_event_cursor(chain_epoch, family, event_sequence)?;
        Ok(ChainEventEnvelope::new(
            cursor,
            event_sequence,
            chain_epoch,
            chain_epoch.settled_tip_height,
            wallet_event,
        ))
    }

    fn mint_wallet_event_cursor(
        &self,
        chain_epoch: ChainEpoch,
        family: ChainEventStreamFamily,
        event_sequence: u64,
    ) -> Result<StreamCursorTokenV1, CanonicalStoreError> {
        self.mint_wallet_event_cursor_at_anchor(
            chain_epoch,
            family,
            event_sequence,
            ChainEventCursorAnchor {
                height: chain_epoch.visible_tip_height,
                hash: chain_epoch.visible_tip_hash,
            },
        )
    }

    fn mint_wallet_event_cursor_at_anchor(
        &self,
        chain_epoch: ChainEpoch,
        family: ChainEventStreamFamily,
        event_sequence: u64,
        tip: ChainEventCursorAnchor,
    ) -> Result<StreamCursorTokenV1, CanonicalStoreError> {
        let mut entries = vec![tip];
        for height in locator_heights(tip.height).into_iter().skip(1) {
            if let Some(hash) = first_displaced_block_hash_after_event(
                &self.bounded_open.db,
                event_sequence,
                height,
            )? {
                entries.push(ChainEventCursorAnchor { height, hash });
            } else if let Some(header) = self.block_header_at(height)? {
                entries.push(ChainEventCursorAnchor {
                    height,
                    hash: header.block_hash,
                });
            }
        }
        let locator = ChainEventLocator::new(entries).map_err(|_| {
            CanonicalStoreError::CanonicalEventCursorMalformed {
                reason: "chain-event locator is outside its bound",
            }
        })?;
        StreamCursorTokenV1::chain_event(
            chain_epoch.network,
            family,
            event_sequence,
            &locator,
            self.cursor_auth_key,
        )
        .map_err(|_| CanonicalStoreError::CanonicalEventCursorMalformed {
            reason: "cursor authentication failed",
        })
    }
}

fn locator_heights(tip_height: BlockHeight) -> Vec<BlockHeight> {
    let mut heights = Vec::with_capacity(CHAIN_EVENT_LOCATOR_MAX);
    let mut current = tip_height.value();
    let mut step = 1_u32;
    loop {
        heights.push(BlockHeight::new(current));
        if heights.len() >= CHAIN_EVENT_LOCATOR_MAX || current == 0 {
            break;
        }
        current = current.saturating_sub(step);
        step = step.saturating_mul(2);
    }
    heights
}
