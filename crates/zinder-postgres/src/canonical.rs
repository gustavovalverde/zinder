use thiserror::Error;
use tokio_postgres::Row;
use zinder_core::{
    BlockHash, BlockHeight, BlockId, CanonicalBlockFacts, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsSequenceDigestBuilder, CanonicalBlockFactsSequenceDigestVersion,
    CanonicalBlockReplayEnvelope,
};

use crate::{
    database::{DatabaseConfig, DatabaseConnection, DatabaseError},
    migration::{DatabaseState, MigrationError},
};

const LOCK_WRITER_SQL: &str = r"
SELECT writer_term
FROM canonical.writer_fence
WHERE singleton = TRUE
FOR UPDATE
";

const ADVANCE_WRITER_FENCE_SQL: &str = r"
UPDATE canonical.writer_fence
SET writer_term = $1
WHERE singleton = TRUE
";

const READ_CONTROL_SQL: &str = r"
SELECT visible_epoch_id, event_sequence, visible_tip_height, visible_tip_hash
FROM canonical.control
WHERE singleton = TRUE
";

const INSERT_BLOCK_SQL: &str = r"
INSERT INTO canonical.block_facts (
    height,
    block_hash,
    parent_hash,
    facts_digest_version,
    facts_digest,
    facts_reference_encoding,
    replay_format_version,
    replay_envelope
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
";

const INSERT_EPOCH_SQL: &str = r"
INSERT INTO canonical.chain_epochs (
    epoch_id,
    previous_epoch_id,
    visible_tip_height,
    visible_tip_hash,
    committed_from_height,
    committed_through_height,
    writer_term,
    sequence_digest_version,
    sequence_block_count,
    sequence_digest
)
VALUES ($1, $2, $3, $4, $3, $3, $5, $6, $7, $8)
";

const INSERT_EVENT_SQL: &str = r"
INSERT INTO canonical.chain_events (
    event_sequence,
    resulting_epoch_id,
    previous_epoch_id,
    event_kind,
    committed_height,
    committed_hash,
    writer_term
)
VALUES ($1, $2, $3, 'append', $4, $5, $6)
";

const INSERT_CONTROL_SQL: &str = r"
INSERT INTO canonical.control (
    singleton,
    visible_epoch_id,
    event_sequence,
    visible_tip_height,
    visible_tip_hash,
    history_predecessor_height,
    history_predecessor_hash,
    writer_term,
    sequence_digest_version,
    sequence_block_count,
    sequence_digest
)
VALUES (TRUE, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
ON CONFLICT (singleton) DO UPDATE SET
    visible_epoch_id = EXCLUDED.visible_epoch_id,
    event_sequence = EXCLUDED.event_sequence,
    visible_tip_height = EXCLUDED.visible_tip_height,
    visible_tip_hash = EXCLUDED.visible_tip_hash,
    history_predecessor_height = EXCLUDED.history_predecessor_height,
    history_predecessor_hash = EXCLUDED.history_predecessor_hash,
    writer_term = EXCLUDED.writer_term,
    sequence_digest_version = EXCLUDED.sequence_digest_version,
    sequence_block_count = EXCLUDED.sequence_block_count,
    sequence_digest = EXCLUDED.sequence_digest
";

/// One backend-neutral canonical fact append prepared by ingest.
#[derive(Debug)]
pub struct CanonicalAppend {
    expected_predecessor: BlockId,
    facts: CanonicalBlockFacts,
    replay_envelope: CanonicalBlockReplayEnvelope,
}

impl CanonicalAppend {
    /// Creates a single-block canonical append from source-derived facts.
    #[must_use]
    pub const fn new(
        expected_predecessor: BlockId,
        facts: CanonicalBlockFacts,
        replay_envelope: CanonicalBlockReplayEnvelope,
    ) -> Self {
        Self {
            expected_predecessor,
            facts,
            replay_envelope,
        }
    }
}

/// Durable result of a canonical append request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalAppendOutcome {
    /// This invocation committed the fact, epoch, event, and control state.
    Committed(CanonicalState),
    /// The exact block was already the durable visible tip.
    AlreadyCommitted(CanonicalState),
}

/// Exact `PostgreSQL` canonical control state returned to operators.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalState {
    visible_epoch_id: u64,
    event_sequence: u64,
    visible_tip: BlockId,
}

impl CanonicalState {
    /// Returns the committed chain epoch.
    #[must_use]
    pub const fn visible_epoch_id(self) -> u64 {
        self.visible_epoch_id
    }

    /// Returns the ordered durable event position.
    #[must_use]
    pub const fn event_sequence(self) -> u64 {
        self.event_sequence
    }

    /// Returns the exact visible canonical tip.
    #[must_use]
    pub const fn visible_tip(self) -> BlockId {
        self.visible_tip
    }
}

/// Concrete `PostgreSQL` canonical writer and probe boundary.
pub struct CanonicalStore {
    connection: DatabaseConnection,
    database_state: DatabaseState,
}

impl CanonicalStore {
    /// Admits the database identity and opens a canonical connection.
    pub async fn open(config: &DatabaseConfig) -> Result<Self, CanonicalPersistenceError> {
        let connection = DatabaseConnection::connect(config).await?;
        admit_writer_role(&connection).await?;
        let database_state =
            DatabaseState::read_from_connection(&connection, config.network).await?;
        Ok(Self {
            connection,
            database_state,
        })
    }

    /// Returns the admitted database identity for this connection.
    #[must_use]
    pub const fn database_state(&self) -> &DatabaseState {
        &self.database_state
    }

    /// Reads the exact visible canonical state, when a transition exists.
    pub async fn state(&self) -> Result<Option<CanonicalState>, CanonicalPersistenceError> {
        read_control(&self.connection.client).await
    }

    /// Atomically commits one block fact, chain epoch, event, and control update.
    #[expect(
        clippy::too_many_lines,
        reason = "the transaction keeps writer fencing, fact/event/control mutation, release, and commit outcome in one auditable atomic boundary"
    )]
    pub async fn commit_append(
        &mut self,
        append: CanonicalAppend,
    ) -> Result<CanonicalAppendOutcome, CanonicalPersistenceError> {
        validate_append(&append)?;
        let transaction = self
            .connection
            .client
            .transaction()
            .await
            .map_err(|source| DatabaseError::operation("canonical transaction start", source))?;
        transaction
            .batch_execute(
                "SET LOCAL synchronous_commit = on;\
                 SET LOCAL statement_timeout = '30s';\
                 SET LOCAL lock_timeout = '5s';",
            )
            .await
            .map_err(|source| DatabaseError::operation("canonical transaction policy", source))?;
        let ownership = transaction
            .query_one(LOCK_WRITER_SQL, &[])
            .await
            .map_err(|source| DatabaseError::operation("canonical writer fence lock", source))?;
        let current_term = ownership
            .try_get::<_, i64>(0)
            .map_err(|source| DatabaseError::operation("canonical writer term decode", source))?;

        let current_state = read_control(&transaction).await?;
        let block_id = append_block_id(&append);
        if let Some(state) = current_state.filter(|state| state.visible_tip == block_id) {
            transaction.commit().await.map_err(|source| {
                DatabaseError::operation("idempotent canonical transaction commit", source)
            })?;
            return Ok(CanonicalAppendOutcome::AlreadyCommitted(state));
        }
        validate_predecessor(current_state, append.expected_predecessor)?;
        let writer_term = current_term
            .checked_add(1)
            .ok_or(CanonicalPersistenceError::WriterTermExhausted)?;
        transaction
            .execute(ADVANCE_WRITER_FENCE_SQL, &[&writer_term])
            .await
            .map_err(|source| DatabaseError::operation("canonical writer fence advance", source))?;

        let reference_encoding = append
            .facts
            .reference_encoding(CanonicalBlockFactsDigestVersion::CURRENT);
        let block_digest = reference_encoding.digest();
        let mut sequence = CanonicalBlockFactsSequenceDigestBuilder::new(
            CanonicalBlockFactsSequenceDigestVersion::CURRENT,
        );
        sequence
            .try_append(block_digest)
            .map_err(|_| CanonicalPersistenceError::SequenceLengthExhausted)?;
        let sequence = sequence.finish();
        let sequence_version = i16::try_from(sequence.version().value())
            .map_err(|_| CanonicalPersistenceError::VersionOutsidePostgresRange)?;
        let sequence_block_count = i64::try_from(sequence.block_count())
            .map_err(|_| CanonicalPersistenceError::SequenceLengthOutsidePostgresRange)?;
        let height = i64::from(block_id.height.value());
        let predecessor_height = i64::from(append.expected_predecessor.height.value());
        let block_hash = block_id.hash.as_bytes();
        let parent_hash = append.expected_predecessor.hash.as_bytes();
        let block_digest_bytes = block_digest.as_bytes();
        let sequence_digest = sequence.as_bytes();
        let (epoch_id, event_sequence, previous_epoch_id) = next_canonical_position(current_state)?;
        transaction
            .execute(
                INSERT_BLOCK_SQL,
                &[
                    &height,
                    &&block_hash[..],
                    &&parent_hash[..],
                    &i16::try_from(block_digest.version().value())
                        .map_err(|_| CanonicalPersistenceError::VersionOutsidePostgresRange)?,
                    &&block_digest_bytes[..],
                    &reference_encoding.as_bytes(),
                    &i32::try_from(append.replay_envelope.format_version().value())
                        .map_err(|_| CanonicalPersistenceError::VersionOutsidePostgresRange)?,
                    &append.replay_envelope.as_bytes(),
                ],
            )
            .await
            .map_err(|source| DatabaseError::operation("canonical fact insert", source))?;
        transaction
            .execute(
                INSERT_EPOCH_SQL,
                &[
                    &epoch_id,
                    &previous_epoch_id,
                    &height,
                    &&block_hash[..],
                    &writer_term,
                    &sequence_version,
                    &sequence_block_count,
                    &&sequence_digest[..],
                ],
            )
            .await
            .map_err(|source| DatabaseError::operation("canonical epoch insert", source))?;
        transaction
            .execute(
                INSERT_EVENT_SQL,
                &[
                    &event_sequence,
                    &epoch_id,
                    &previous_epoch_id,
                    &height,
                    &&block_hash[..],
                    &writer_term,
                ],
            )
            .await
            .map_err(|source| DatabaseError::operation("canonical event insert", source))?;
        transaction
            .execute(
                INSERT_CONTROL_SQL,
                &[
                    &epoch_id,
                    &event_sequence,
                    &height,
                    &&block_hash[..],
                    &predecessor_height,
                    &&parent_hash[..],
                    &writer_term,
                    &sequence_version,
                    &sequence_block_count,
                    &&sequence_digest[..],
                ],
            )
            .await
            .map_err(|source| DatabaseError::operation("canonical control insert", source))?;
        let committed_state = CanonicalState {
            visible_epoch_id: u64::try_from(epoch_id)
                .map_err(|_| CanonicalPersistenceError::CanonicalPositionExhausted)?,
            event_sequence: u64::try_from(event_sequence)
                .map_err(|_| CanonicalPersistenceError::CanonicalPositionExhausted)?,
            visible_tip: block_id,
        };
        transaction.commit().await.map_err(|source| {
            DatabaseError::operation("canonical transaction commit outcome unknown", source)
        })?;
        Ok(CanonicalAppendOutcome::Committed(committed_state))
    }

    /// Closes the `PostgreSQL` connection after all operations complete.
    pub async fn close(self) -> Result<(), CanonicalPersistenceError> {
        self.connection.close().await?;
        Ok(())
    }
}

/// Failure while admitting, writing, or probing `PostgreSQL` canonical state.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CanonicalPersistenceError {
    /// Database connection or operation failure.
    #[error(transparent)]
    Database(#[from] DatabaseError),
    /// Database identity or schema admission failure.
    #[error(transparent)]
    Migration(#[from] MigrationError),
    /// The connected runtime role is not the least-privilege canonical writer.
    #[error("connected database role is not admitted as the canonical writer")]
    WriterRoleRejected,
    /// The block does not immediately follow the declared predecessor.
    #[error("canonical append block does not immediately follow its expected predecessor")]
    NonContiguousHeight,
    /// The block parent does not match the declared predecessor.
    #[error("canonical append parent hash differs from its expected predecessor")]
    ParentHashMismatch,
    /// The replay envelope does not identify the supplied block facts.
    #[error("canonical replay envelope identity differs from supplied block facts")]
    ReplayIdentityMismatch,
    /// Durable state does not match the caller's expected predecessor.
    #[error("canonical append expected predecessor is stale")]
    StalePredecessor,
    /// The durable writer term cannot advance.
    #[error("canonical writer term is exhausted")]
    WriterTermExhausted,
    /// The ordered epoch or event position cannot advance.
    #[error("canonical epoch or event position is exhausted")]
    CanonicalPositionExhausted,
    /// The ordered canonical digest cannot represent another block.
    #[error("canonical fact sequence length is exhausted")]
    SequenceLengthExhausted,
    /// A version cannot be represented by the `PostgreSQL` schema.
    #[error("canonical format version is outside the PostgreSQL schema range")]
    VersionOutsidePostgresRange,
    /// The ordered sequence length cannot be represented by `PostgreSQL`.
    #[error("canonical sequence length is outside the PostgreSQL schema range")]
    SequenceLengthOutsidePostgresRange,
    /// A persisted fixed-size value has an invalid length.
    #[error("persisted canonical {field} must contain exactly 32 bytes")]
    InvalidFixedBytes {
        /// Corrupt persisted field.
        field: &'static str,
    },
    /// A persisted positive integer is outside the domain range.
    #[error("persisted canonical {field} is outside the supported range")]
    InvalidInteger {
        /// Corrupt persisted field.
        field: &'static str,
    },
}

async fn read_control(
    client: &(impl tokio_postgres::GenericClient + Sync),
) -> Result<Option<CanonicalState>, CanonicalPersistenceError> {
    client
        .query_opt(READ_CONTROL_SQL, &[])
        .await
        .map_err(|source| DatabaseError::operation("canonical control read", source))?
        .map(|row| canonical_state_from_row(&row))
        .transpose()
}

fn canonical_state_from_row(row: &Row) -> Result<CanonicalState, CanonicalPersistenceError> {
    let visible_epoch_id = positive_i64_to_u64(
        row.try_get::<_, i64>(0)
            .map_err(|source| DatabaseError::operation("visible epoch decode", source))?,
        "visible epoch",
    )?;
    let event_sequence = positive_i64_to_u64(
        row.try_get::<_, i64>(1)
            .map_err(|source| DatabaseError::operation("event sequence decode", source))?,
        "event sequence",
    )?;
    let height = row
        .try_get::<_, i64>(2)
        .map_err(|source| DatabaseError::operation("visible tip height decode", source))?;
    let height = u32::try_from(height).map_err(|_| CanonicalPersistenceError::InvalidInteger {
        field: "tip height",
    })?;
    let hash = row
        .try_get::<_, Vec<u8>>(3)
        .map_err(|source| DatabaseError::operation("visible tip hash decode", source))?;
    let hash = fixed_32(hash, "tip hash")?;
    Ok(CanonicalState {
        visible_epoch_id,
        event_sequence,
        visible_tip: BlockId::new(BlockHeight::new(height), BlockHash::from_bytes(hash)),
    })
}

fn validate_append(append: &CanonicalAppend) -> Result<(), CanonicalPersistenceError> {
    let block_id = append_block_id(append);
    if append.expected_predecessor.height.next() != Some(block_id.height) {
        return Err(CanonicalPersistenceError::NonContiguousHeight);
    }
    if append.facts.block_header.parent_hash != append.expected_predecessor.hash {
        return Err(CanonicalPersistenceError::ParentHashMismatch);
    }
    if append.replay_envelope.block_height() != block_id.height
        || append.replay_envelope.block_hash() != block_id.hash
        || append.replay_envelope.parent_hash() != append.expected_predecessor.hash
        || append.replay_envelope.reference_digest()
            != append
                .facts
                .digest(CanonicalBlockFactsDigestVersion::CURRENT)
    {
        return Err(CanonicalPersistenceError::ReplayIdentityMismatch);
    }
    Ok(())
}

fn validate_predecessor(
    current: Option<CanonicalState>,
    expected: BlockId,
) -> Result<(), CanonicalPersistenceError> {
    if current.is_some_and(|state| state.visible_tip != expected) {
        Err(CanonicalPersistenceError::StalePredecessor)
    } else {
        Ok(())
    }
}

fn append_block_id(append: &CanonicalAppend) -> BlockId {
    BlockId::new(
        append.facts.block_header.height,
        append.facts.block_header.block_hash,
    )
}

fn next_canonical_position(
    current: Option<CanonicalState>,
) -> Result<(i64, i64, Option<i64>), CanonicalPersistenceError> {
    let (next_epoch_id, next_event_sequence, previous_epoch_id) = match current {
        Some(state) => (
            state
                .visible_epoch_id
                .checked_add(1)
                .ok_or(CanonicalPersistenceError::CanonicalPositionExhausted)?,
            state
                .event_sequence
                .checked_add(1)
                .ok_or(CanonicalPersistenceError::CanonicalPositionExhausted)?,
            Some(state.visible_epoch_id),
        ),
        None => (1, 1, None),
    };
    Ok((
        i64::try_from(next_epoch_id)
            .map_err(|_| CanonicalPersistenceError::CanonicalPositionExhausted)?,
        i64::try_from(next_event_sequence)
            .map_err(|_| CanonicalPersistenceError::CanonicalPositionExhausted)?,
        previous_epoch_id
            .map(i64::try_from)
            .transpose()
            .map_err(|_| CanonicalPersistenceError::CanonicalPositionExhausted)?,
    ))
}

fn fixed_32(bytes: Vec<u8>, field: &'static str) -> Result<[u8; 32], CanonicalPersistenceError> {
    bytes
        .try_into()
        .map_err(|_| CanonicalPersistenceError::InvalidFixedBytes { field })
}

fn positive_i64_to_u64(
    encoded_integer: i64,
    field: &'static str,
) -> Result<u64, CanonicalPersistenceError> {
    if encoded_integer <= 0 {
        return Err(CanonicalPersistenceError::InvalidInteger { field });
    }
    u64::try_from(encoded_integer).map_err(|_| CanonicalPersistenceError::InvalidInteger { field })
}

async fn admit_writer_role(
    connection: &DatabaseConnection,
) -> Result<(), CanonicalPersistenceError> {
    let row = connection
        .client
        .query_one(
            r"
SELECT
    pg_has_role(current_user, 'zinder_ingest', 'member'),
    has_schema_privilege(current_user, 'canonical', 'CREATE'),
    has_schema_privilege(current_user, 'zinder_metadata', 'USAGE')
        AND has_schema_privilege(current_user, 'canonical', 'USAGE')
        AND has_table_privilege(current_user, 'canonical.writer_fence', 'SELECT')
        AND has_table_privilege(current_user, 'canonical.writer_fence', 'UPDATE')
        AND has_table_privilege(current_user, 'canonical.block_facts', 'INSERT')
        AND has_table_privilege(current_user, 'canonical.chain_epochs', 'INSERT')
        AND has_table_privilege(current_user, 'canonical.chain_events', 'INSERT')
        AND has_table_privilege(current_user, 'canonical.control', 'SELECT')
        AND has_table_privilege(current_user, 'canonical.control', 'INSERT')
        AND has_table_privilege(current_user, 'canonical.control', 'UPDATE')
",
            &[],
        )
        .await
        .map_err(|source| DatabaseError::operation("canonical writer role admission", source))?;
    let is_writer = row
        .try_get::<_, bool>(0)
        .map_err(|source| DatabaseError::operation("canonical writer membership decode", source))?;
    let can_create = row
        .try_get::<_, bool>(1)
        .map_err(|source| DatabaseError::operation("canonical DDL privilege decode", source))?;
    let has_required_privileges = row
        .try_get::<_, bool>(2)
        .map_err(|source| DatabaseError::operation("canonical privilege decode", source))?;
    if is_writer && !can_create && has_required_privileges {
        Ok(())
    } else {
        Err(CanonicalPersistenceError::WriterRoleRejected)
    }
}
