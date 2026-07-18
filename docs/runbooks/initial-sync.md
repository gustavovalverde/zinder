# Initial Sync

Zinder's production-shaped wallet path has three state owners:

1. `zinder-ingest` constructs and follows the schema-v4 canonical store.
2. `zinder-projector` constructs and follows the schema-v1 wallet store from an
   authenticated canonical event fence.
3. `zinder-compat-lightwalletd` serves immutable canonical/wallet RocksDB
   secondary pairs at one exact fence.

There is no production query fallback, derive-store writer, dual-write mode, or
legacy store migration. A store that does not satisfy the current physical
schema and network contract is rejected without mutation and must be rebuilt in
a fresh path.

## Before starting

- Sync Zebra to the target network tip and enable the JSON-RPC capabilities in
  the checked-in service configurations.
- Put canonical, projector-secondary, wallet, compatibility canonical-secondary,
  and compatibility wallet-secondary data in distinct paths. Nested or aliased
  paths are rejected.
- Give the projector one stable 32-character hexadecimal build-owner identity.
- Size the host for the canonical store, wallet store, one coherent checkpoint,
  worst-case compaction/restore workspace, and chain-growth reserve. The prior
  500 GB mainnet canary did not establish that capacity envelope.
- Keep `CanonicalControl` and `IngestControl` on loopback in the single-host
  topology. If either control plane is non-loopback, configure the same bearer
  token for every client before starting.

The supported configurations are:

- [`deploy/config/ingest.toml`](../../deploy/config/ingest.toml)
- [`deploy/config/projector.toml`](../../deploy/config/projector.toml)
- [`deploy/config/compat-lightwalletd.toml`](../../deploy/config/compat-lightwalletd.toml)

Use [`deploy/docker-compose.yml`](../../deploy/docker-compose.yml) or the systemd
unit for the complete single-host topology. Railway is an ingest-only diagnostic
canary and cannot certify wallet production because it does not provide the
required shared-host filesystem contract.

## Start order

Start the canonical writer first:

```bash
zinder-ingest --config /etc/zinder/ingest.toml
```

The writer probes Zebra, enters bulk construction when needed, publishes READY,
and then follows the tip. It owns the canonical event log, reorg replacement,
retention leases, durable mempool history, and both control services.

Start the projector after ingest is healthy:

```bash
ZINDER_PROJECTOR__BUILD_OWNER_HEX=00112233445566778899aabbccddeeff \
  zinder-projector --config /etc/zinder/projector.toml
```

The projector takes a durable build lease, constructs the wallet store at a
fixed canonical fence, publishes READY only after cold validation, and then
follows canonical append and replacement events. Settlement advances the
wallet undo floor atomically. A restart resumes from the persisted canonical
source position instead of rebuilding a second truth.

Start compatibility only after the projector is healthy:

```bash
zinder-compat-lightwalletd --config /etc/zinder/compat-lightwalletd.toml
```

Compatibility catches inactive canonical and wallet secondary generations up
to one authenticated fence, validates the pair, then atomically publishes it.
Requests retain the generation they started with; replacement generations are
removed only after their last request releases them. Compatibility must stay
not-ready when no exact pair exists.

Compose encodes this order and keeps projector and compatibility in ingest's
network namespace so the control listener remains loopback-only:

```bash
docker compose \
  --env-file deploy/.env.mainnet \
  -f deploy/docker-compose.yml \
  up -d
```

Substitute the checked-in testnet or regtest environment when targeting those
networks.

## Readiness sequence

Probe every runtime independently:

```bash
curl -fsS http://127.0.0.1:9105/readyz
curl -fsS http://127.0.0.1:9110/readyz
curl -fsS http://127.0.0.1:9107/readyz
```

Admission requires all of the following:

- ingest reports canonical READY at the current authenticated event sequence;
- the mempool source has emitted a complete initial snapshot for the current
  connection generation;
- projector reports the same canonical source fence and a READY wallet digest;
- compatibility reports a validated exact pair at that fence;
- canonical and projection lag are each within their published bounds.

An HTTP health response proves only that a process is alive. Do not route
wallet traffic until every readiness condition above is true. A projection-
behind, replica-behind, hydration-incomplete, lease-loss, or fence-mismatch
cause is a failed admission, not an empty-data condition.

## Reorgs and reconnects

The canonical writer may replace only an unsettled suffix within its persisted
nonzero reorg window. It authenticates the connected replacement and commits
the replacement blocks, displaced archive, event, and visible fence in one
batch. Attempts to cross settlement or exceed the window fail without mutation.

The projector consumes that exact replacement event and either applies its
persisted undo suffix or fails closed. Compatibility keeps serving its previous
immutable exact pair until a newer exact pair passes validation. It never
combines a newer canonical secondary with an older wallet secondary.

When the mempool source reconnects, ingest immediately withdraws mempool-backed
readiness and pending-transaction visibility. It restores them only after the
new connection emits a complete snapshot marker; partial hydration is never
published as an empty mempool.

## Restart and recovery

For an ordinary restart, preserve the same owner identities and paths and start
services in the normal ownership order. Each owner validates its persisted
network, schema, lease, event position, and digest before serving.

A production restore requires one coherent canonical/wallet checkpoint bundle
with an authenticated cross-store fence. Independently timed directory copies
are not a supported restore procedure. Until the coherent bundle implementation
and the 10,000-block-tail restore gate pass, restore remains a production
blocker; do not substitute the old derive backup manifest or a fixed-fence
primary-store handoff.

If an old or mismatched store is found, keep it outside the active paths for
forensics and start canonical schema-v4 and wallet schema-v1 construction in
fresh directories. Do not add a migration reader, compatibility alias, or
second writer to make it open.

## Production acceptance

Initial sync is production-admissible only when the release evidence shows:

- fresh mainnet canonical construction in at most 3 hours;
- wallet projection in at most 2 hours and the complete wallet-ready lifecycle
  in at most 4 hours;
- coherent checkpoint restore plus a 10,000-block tail in at most 15 minutes;
- canonical lag no greater than 2 blocks and wallet lag no greater than 2
  canonical epochs under sustained following;
- sufficient measured disk and memory headroom for the complete topology;
- live replacement, restart, exact-pair drain, mempool reconnect, and real
  Android create/restore/transparent/send flows.

The measured mainnet canary took about 7 hours 47 minutes for canonical
construction and validation, so the current code and deployment shape must not
be described as production-certified until that performance gate and the
restore/capacity/client gates are closed. See
[the wallet-serving cutover plan](../plans/fact-first-wallet-serving-cutover.md)
for the current evidence and execution order.

## References

- [ADR-0035: Fact-first storage selection and lifecycle](../adrs/0035-fact-first-storage-selection-and-lifecycle.md)
- [Wallet-serving cutover](../plans/fact-first-wallet-serving-cutover.md)
- [Deploying on a VM](deploying-on-a-vm.md)
- [Testing](testing.md)
