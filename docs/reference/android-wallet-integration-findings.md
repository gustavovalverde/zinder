# Findings from Android wallet integration

| Field    | Value                                                          |
| -------- | -------------------------------------------------------------- |
| Status   | Background research                                            |
| Audience | Zinder maintainers, contributors, downstream wallet developers |
| Date     | 2026-04-29                                                     |

**Sources.** First end-to-end integration test of `zinder-compat-lightwalletd` against the `zcash-android-wallet-sdk` (v2.4.8) `demo-app` and against the Zashi (zodl-android) `zcashtestnetInternalDebug` flavor. Local stack: `zinder-ingest tip-follow` + `zinder-compat-lightwalletd` on testnet (network `zcash-testnet`), backed by the testnet `z3_zebra` node. Wallet clients: Pixel 10 Pro, Android 16. Demo-app uses plaintext gRPC over LAN at `192.168.1.117:9077`. Zashi requires TLS, so it talks to Caddy at `192.168.1.117:8443` which terminates `tls internal` and reverse-proxies to compat as `h2c://127.0.0.1:9077`.

**Re-test status (2026-04-29).** A second pass against a rebuilt zinder confirms two of the original frictions are now resolved by code:

- The transparent UTXO surface is wired (`GetAddressUtxos`, `GetAddressUtxosStream` no longer return `UNIMPLEMENTED`). `GetLightdInfo` now advertises `taddrSupport: true`. The implementation lives in `services/zinder-compat-lightwalletd/src/grpc.rs` and `crates/zinder-store/src/transparent_utxo.rs`.
- A new `--wallet-serving` flag on `zinder-ingest backfill` derives the historical floor needed for lightwalletd-compatible wallets from upstream-node-advertised activation heights, instead of requiring operators to know per-network checkpoint depths. The implementation lives in `services/zinder-ingest/src/main.rs` and `services/zinder-ingest/src/config.rs`.

The `GetTreeState` store-floor behavior still applies: zinder's testnet store starts at height 3,985,924, and anything older returns `NOT_FOUND`. The re-test exercised Zashi's "Create new wallet" flow against this same store and the sync completed end-to-end (`scanProgress=1.0`, `chainTipHeight=fullyScannedHeight`, all UTXOs fetched). That flow did not trip the cutoff because the SDK fetched `GetTreeState` only at the current chain tip (3,986,063), not at the spend-before-sync lower bound (3,929,900). The lower bound is a lookback ceiling, not the tree-state anchor. A wallet whose actual tree-state anchor or scan start falls below the store floor would still fail.

**Resync (2026-04-29) confirms the residual failure mode under live use.** Triggering Settings → Advanced Settings → Resync Wallet on the same Zashi build, against the same zinder store, reproduced the predicted block:

- `processNewBlocksInSbSOrder()` produced one suggested scan range, `BlockHeight(3930001)..BlockHeight(3986069)` with `priority=ChainTip`.
- The SDK then called `fetchTreeStateForHeight(3930000)`, i.e. `scan_range_start - 1`, as the tree-state anchor for the scan.
- zinder responded `NOT_FOUND: tree-state artifact is unavailable at height BlockHeight(3930000)`.
- The SDK retried five times with exponential backoff (`Retrying (1/5) in 500 ms`, `2/5 in 1000 ms`, ...), advanced to `fetchTreeStateForHeight(3931000)` (the next batch boundary, +1000 blocks), and failed again.
- The download stage emitted `DownloadFailed(failedAtHeight=BlockHeight(3930001), ...FailedDownloadException)`. The gRPC channel then closed (`Channel shutdown invoked`), the SDK reconnected, and the loop restarted from the same suggested range.

The Zashi UI displayed `Restoring Wallet • 0.0% Complete · Keep Zodl open on active phone screen` and never progressed. There is no user-visible error; the wallet sits indefinitely in a retry-reconnect-retry cycle. Operators bringing up a freshly bootstrapped zinder against a Resync flow would see this as "the wallet is stuck at 0%" with no signal beyond the retry storm in `adb logcat`.

This is the canonical reproduction of the cutoff finding in production-shaped behavior: a real Zashi screen, not a probe. Any deployment intending to serve Resync (which is the recovery flow most users will exercise) needs the store floor below the SDK's resync anchor window. `--wallet-serving` is the operationally clean way to ensure that.

**Resolution validated end-to-end (2026-04-29).** Wiping the testnet store and re-bootstrapping with `zinder-ingest backfill --wallet-serving --to-height $((TIP - 110)) --commit-batch-blocks 200` ran cleanly from height 280,000 (Sapling activation, the floor returned by `SourceNetworkUpgradeHeights::wallet_serving_floor()`) to 3,985,968 in 47m17s on a release build, averaging ~1300 blocks/sec on this machine. The previously-documented Failure A (sapling-activation boundary) did not reproduce; the chain-epoch tree-size accumulator now keeps lockstep across the boundary. Failure B (non-zero starting tree size) was not exercised, since `--wallet-serving` always selects the earliest activation where upstream-node tree sizes are zero. After restarting `tip-follow` and `compat-lightwalletd` against the rebuilt store, the same Zashi build that had been spinning in the retry loop reconnected on its next backoff and completed the previously-blocking calls:

```text
fetchTreeStateForHeight 3930000  → OK
fetchTreeStateForHeight 3931000  → OK
fetchTreeStateForHeight 3932000  → OK
WalletSummary scanProgress 90/29613   → 0.30%
WalletSummary scanProgress 743/24968  → 2.97%
WalletSummary scanProgress 879/23688  → 3.71%
```

The Zashi UI advanced from `Restoring Wallet • 0.0% Complete` (stuck for hours) to `3.5% Complete` within ~90 seconds of zinder coming back up. `nextSaplingSubtreeIndex=4, nextOrchardSubtreeIndex=1` in the wallet summary confirms that `GetSubtreeRoots(start_index=0)` also resolved successfully, validating both halves of the original architectural finding.

For wallet-serving deployments the takeaway is the simplest possible operator gesture: pass `--wallet-serving` to `zinder-ingest backfill` and let the upstream node's activation heights pick the floor. Manual `--checkpoint-height` selection for serving is no longer needed.

**Related.** [Wallet data plane](../architecture/wallet-data-plane.md), [Protocol boundary](../architecture/protocol-boundary.md), [Chain ingestion](../architecture/chain-ingestion.md), [Service boundaries](../architecture/service-boundaries.md), [PRD-0001](../prd-0001-zinder-indexer.md), [Lessons from Zaino](lessons-from-zaino.md).

## Purpose

Capture the first observed wallet behavior against `zinder-compat-lightwalletd`. The compatibility surface exists so existing lightwalletd clients can migrate without code changes; this document is the audit trail for what actually happens when one does. It records both the friction points that surfaced and the parts of the surface that the SDK exercised cleanly, with file:line references back into the zinder tree where the friction is decided.

This is not a defect log against the SDK or against zinder. The SDK's behavior is correct against the lightwalletd protocol as published, and zinder's behavior is correct against its current advertised scope. The friction lives at the boundary, and the boundary is the part this doc owes the maintainers an honest description of.

This page is evidence, not the durable contract. The canonical Android/Zashi
serving claim lives in [Wallet data plane §External Wallet Compatibility Claims](../architecture/wallet-data-plane.md#external-wallet-compatibility-claims);
ingestion requirements live in [Chain ingestion](../architecture/chain-ingestion.md);
transport and writer-status behavior live in [Service operations](../architecture/service-operations.md).

## Method

The full stack ran on a single host:

- A pre-existing testnet `z3_zebra` container at `127.0.0.1:18232`, cookie auth (`/var/run/auth/.cookie`), tip near height 3,985,914.
- A second zinder stack alongside the existing regtest stack, distinct ports and store paths to avoid collision: ingest control `127.0.0.1:9201`, compat gRPC `0.0.0.0:9077`, store under `.tmp/testnet/`.
- `zinder-ingest backfill` from a checkpoint at `tip - 50` (height 3,985,864), then `zinder-ingest tip-follow` and `zinder-compat-lightwalletd`. Compat readiness reached `current_height = target_height` immediately.
- `zcash-android-wallet-sdk/demo-app`, `zcashtestnetDebug` flavor, with one source patch: `LightWalletEndpoint.Testnet` switched to `192.168.1.117:9077` with `isSecure = false`, and a `network_security_config.xml` allowing cleartext to that LAN IP. No SDK source changes.
- Zashi (`zodl-android`), `zcashtestnetInternalDebug` flavor, built against the local `zcash-android-wallet-sdk` via Gradle composite build (`SDK_INCLUDED_BUILD_PATH`). One repo-side patch landed: `app/src/zcashtestnetInternalDebug/res/values/bools.xml` was missing, so the variant inherited `zcash_is_testnet=false` from the mainnet default and refused to talk to `chainName: "test"` until the override was added. Network configuration uses Zashi's existing `Custom server` UI to point at `192.168.1.117:8443`; the Caddy CA root is bundled as `app/src/zcashtestnetInternalDebug/res/raw/zinder_caddy_ca.crt` and trusted via `network_security_config.xml`. Caddy was started with `tls internal` to issue a local-CA cert for `192.168.1.117`.
- The wallet was driven by `adb shell input tap`, sync attempted with both the canned "Alyssa" wallet (birthday from `WalletFixture.lastKnownBirthday`, originally testnet height 2,170,000) and a recent-birthday variant (`lastKnownBirthday` patched to 3,985,900 inside zinder's data window). Zashi sync attempted by selecting "Create new wallet" and switching the active server to the Caddy endpoint.

Logs and gRPC traffic were captured with `adb logcat`, `lsof` against port 9077, and `grpcurl -insecure` against the Caddy endpoint.

## What worked

These signals all came back clean and are worth recording because they retire a specific class of "is the wire actually compatible" question.

### Wire compatibility for the current sync surface

The vendored `cash.z.wallet.sdk.rpc.CompactTxStreamer` schema in `crates/zinder-proto/proto/compat/lightwalletd/` matched the schema the SDK was generated against. `grpcurl` against the running compat server returned valid responses for `GetLightdInfo`, `GetLatestBlock`, `GetBlock`, and `GetBlockRange` without any field-shape adjustments.

`GetLightdInfo` returned (truncated):

```json
{
  "vendor": "Zinder",
  "chainName": "test",
  "saplingActivationHeight": "280000",
  "consensusBranchId": "4dec4df0",
  "blockHeight": "3985916",
  "estimatedHeight": "3985916"
}
```

`chainName: "test"` is the value the Android SDK matches against `ZcashNetwork.ID_TESTNET`, so the wallet accepted the server as a valid testnet endpoint.

### SDK opens connections and starts sync against a non-TLS LAN endpoint

The SDK's `co.electriccoin.lightwallet.client.internal.ChannelFactory.kt:22-27` logs `"WARNING Using plaintext connection"` and proceeds. Two long-lived TCP connections were seen ESTABLISHED from the phone to compat-lightwalletd at the LAN IP, used in parallel for the streaming sync calls.

### Sync state machine reaches `updateChainTip`

The SDK's `CompactBlockProcessor` invoked the following calls in order, against zinder, all of which succeeded:

1. `getLightdInfo()` (one-shot info probe).
2. `fetchLatestBlockHeight()` returned `3985955` (live tip after a few blocks of follow-up).
3. `updateChainTip()` set the local chain tip on the Rust backend to `BlockHeight(value=3985955)`.
4. `suggestScanRanges()` emitted a Historic scan range covering wallet birthday + 1 to current tip.

The wallet's view of the chain matched zinder's view at the tip. The integration broke past the tip read but never before it.

### Zashi over Caddy `tls internal` reaches the same sync state machine

Zashi connects only to TLS endpoints (`ValidateEndpointUseCase` hardcodes `isSecure = true`). With `caddy run` configured as `192.168.1.117:8443 { tls internal; reverse_proxy h2c://127.0.0.1:9077 }` and the Caddy local-CA root added to the wallet's `network_security_config.xml` `<trust-anchors>`, Zashi:

- Resolved the custom-server entry as Active and persisted it in the wallet's settings store.
- Issued `GetLightdInfo` and accepted `chainName: "test"` once the testnet bool override was in place. The `vendor: "Zinder"` field passed through unchanged; Zashi has no allow-list on it.
- Reached the same `CompactBlockProcessor.fetchTreeStateForHeight()` call path the demo-app reached, with the same retry/backoff state machine.

The HTTPS-to-h2c hop did not require any zinder-side change. The compat process keeps speaking h2c on `0.0.0.0:9077`; Caddy handles the TLS termination and the HTTP/2 cleartext proxying. This validates the production deployment shape (TLS in front, plaintext h2c gRPC behind) end-to-end.

## Friction points

### Subtree-root continuity is required for first-sync bootstrap

`CompactBlockProcessor.getSubtreeRoots()` is called early in the sync setup with `start_index = SubtreeRootIndex(0)`, regardless of `WalletInitMode` and regardless of the wallet birthday. The SDK populates its local subtree-root table from `SubtreeRoots` responses to validate compact-block scanning later; on a fresh install with no local subtree-root rows, it asks for the full history starting at index 0. Patching `WalletFixture.lastKnownBirthday` to a height inside zinder's data window did not change this behavior; the call still arrives with `start_index = 0`.

zinder's response is set in `services/zinder-query/src/lib.rs:586-593`:

```rust
for (subtree_index, subtree_root) in available_range.into_iter().zip(subtree_roots) {
    let Some(subtree_root) = subtree_root else {
        return Err(QueryError::SubtreeRootsUnavailable {
            protocol: subtree_root_range.protocol,
            start_index: subtree_index,
        });
    };
    ...
}
```

`SubtreeRootsUnavailable` is mapped to gRPC `NOT_FOUND` by `status_from_query_error`. The SDK observes `io.grpc.StatusException: NOT_FOUND: Orchard subtree roots are unavailable at index SubtreeRootIndex(0)` and retries (3 attempts, 1s between), then surfaces a `LightWalletException$GetSubtreeRootsException` to the wallet layer.

The store-side decision that makes this fail is in `crates/zinder-store/src/subtree_root.rs`: if a requested subtree-index entry is not present in the canonical store, the function returns `None` for that index. `services/zinder-query/src/lib.rs` then refuses the entire batch with `SubtreeRootsUnavailable` rather than returning a sparse vector.

This is consistent with the documented checkpoint contract: "Reads at heights below the checkpoint return `ArtifactUnavailable`" (`services/zinder-ingest/src/main.rs:103`). It is the contract working as written. The implication for downstream is the part that needs to be loud:

> Any zinder deployment that wants to serve the Zcash Android SDK (Zashi, the demo-app, any client built on `lightwallet-client-lib`) must have a canonical store covering at minimum the height where the first shielded protocol subtree was completed for each shielded pool the wallet exercises. For Orchard on testnet, that is several tens of thousands of blocks after the NU5 activation height (2,170,000). On mainnet the first Orchard subtree completes near NU5 activation (1,687,104). Bootstrapping a serving zinder from a recent checkpoint puts every fresh wallet install into the failure mode above on first sync.

There is no SDK-side knob the wallet operator can set to avoid this. The SDK does not consult the wallet birthday before requesting subtree roots from index 0, and there is no `WalletInitMode` that defers the call.

Resolved operationally in the M2 hardening slice: wallet-serving deployments
use `zinder-ingest backfill --wallet-serving`, which derives the historical
floor from upstream-node-advertised activation heights, resolves the parent
checkpoint from the upstream node, and ingests canonical artifacts from that floor.
Recent-checkpoint stores remain valid for smokes and storage validation, but
they cannot claim Android SDK or Zashi serving compatibility.

### `GetTreeState` remains strict below the first ingested height

`CompactBlockProcessor.fetchTreeStateForHeight()` wraps below-floor failures as `co.electriccoin.lightwallet.client.model.ResponseException: Communication failure with details: 5: tree-state artifact is unavailable at height BlockHeight(...)`. With this zinder testnet instance freshly bootstrapped from `tip - 50`, probing below the first ingested tree-state height hit a hard wall:

```text
io.grpc.StatusException: NOT_FOUND: tree-state artifact is unavailable at height BlockHeight(3974000)
```

Bisecting `GetTreeState` against this same store revealed the available window:

```text
GetLightdInfo blockHeight = 3986028
GetTreeState 3985924 -> OK   (lowest available)
GetTreeState 3985923 -> NOT_FOUND
GetTreeState 3974000 -> NOT_FOUND
```

The cutoff is fixed at the height the ingest pipeline first wrote a tree-state artifact (`3,985,924` for this instance, derived from the `--checkpoint-height $((TIP - 50))` bootstrap). The cutoff does not slide forward as tip advances; it is the floor of the store's tree-state coverage, not a retention window. Anything older returns NOT_FOUND.

The decision happens in `services/zinder-query/src/lib.rs:528-538`:

```rust
let tree_state = match reader.tree_state_at(height) {
    Ok(Some(tree_state)) => tree_state,
    Ok(None)
    | Err(StoreError::ArtifactMissing {
        family: ArtifactFamily::TreeState,
        ..
    }) => {
        return Err(QueryError::ArtifactUnavailable {
            artifact: QueryArtifact::TreeState,
            height,
        });
    }
    ...
};
```

`status_from_query_error` maps `ArtifactUnavailable { artifact: TreeState, .. }` to gRPC `NOT_FOUND`. The compat shim's `get_tree_state` (`services/zinder-compat-lightwalletd/src/grpc.rs:340-352`) is a thin pass-through and adds no fallback, so the wallet sees the bare unavailability.

The root cause is the same as the subtree-roots finding above: zinder's bootstrap from a recent checkpoint produces a store whose lowest covered height is far above older wallet anchors. The two RPCs expose different facets of the same gap.

> Any zinder deployment that wants to serve the Zcash Android SDK must have a canonical store whose tree-state artifact coverage starts at or before the lowest tree-state anchor a supported wallet flow can request. For imported wallets and fixtures such as the demo-app's "Alyssa" wallet, that can be far below what `--checkpoint-height = tip - 50` produces.

The second Zashi pass narrowed this finding: the "Create new wallet" flow completed against the same near-tip store once the transparent UTXO stream existed, because that path fetched tree state at the current tip. The Resync pass then confirmed the below-floor behavior as a live wallet failure: Zashi anchored at `scan_range_start - 1`, received `NOT_FOUND`, retried, reconnected, and stayed at `0.0%`.

Resolved operationally by the same wallet-serving backfill mode as the
subtree-root finding above. Query remains strict: `GetTreeState` below the
store floor still returns `NOT_FOUND`, because synthesizing upstream node fallback
tree states would bypass the canonical store contract.

### `GetAddressUtxos` and `GetAddressUtxosStream` require stored transparent UTXO artifacts

The initial Android run found both methods returning
`Status::unimplemented(...)` from `services/zinder-compat-lightwalletd/src/grpc.rs`.
That was a real Zashi blocker because the Android SDK's
`CompactBlockProcessor.refreshUtxos()` calls `GetAddressUtxosStream` during
sync setup and routes code `12` (`UNIMPLEMENTED`) through
`LightWalletException$FetchUtxosException`.

Resolved in the M2 hardening slice: the compat adapter now serves
`GetAddressUtxos` and `GetAddressUtxosStream` from the canonical transparent
address UTXO and transparent spend artifact families. `GetLightdInfo` may set
`taddr_support=true` only because this path is indexed and epoch-bound. The
canonical contract is in
[Wallet data plane §Transparent Address UTXOs](../architecture/wallet-data-plane.md#transparent-address-utxos);
this reference page should stay as the evidence log, not as a second source of
truth for the API shape.

### Backfill from a non-zero shielded tree-size checkpoint must seed the builder

When attempting to remedy the previous finding by deep-backfilling testnet zinder from sapling activation forward, two distinct failures surfaced. Both reproduce reliably and look like real zinder defects to file separately from the integration findings above.

**Failure A: backfill across the sapling activation boundary fails at first sapling note.** Starting `zinder-ingest backfill` with `--checkpoint-height 279999` (sapling activation height minus one) succeeds for ~750 blocks, then fails when the first sapling note enters the tree:

```text
backfill_checkpoint_resolved checkpoint_height=279999 sapling_commitment_tree_size=0 orchard_commitment_tree_size=0
chain_committed ... block_range_end=280749 ...
ingest_run_failed error=compact block tree sizes sapling=1 orchard=0 do not match observed sapling=0 orchard=0
```

The compact block at the first sapling-note height declares `sapling=1`, but zinder's chain-epoch tree-size accumulator reads `sapling=0`. The error is symmetric for `--commit-batch-blocks 200` and `--commit-batch-blocks 25`, so the failure is not batch-edge sensitive. It looks like the chain-epoch tree-state observer is not seeing the sapling note that the artifact builder just wrote.

**Failure B: backfill from any non-zero sapling-tree-size checkpoint fails at the very first block.** Starting backfill with `--checkpoint-height 1499999` (well past sapling activation but before NU5):

```text
backfill_checkpoint_resolved checkpoint_height=1499999 sapling_commitment_tree_size=107795 orchard_commitment_tree_size=0
ingest_run_failed error=compact block tree sizes sapling=107795 orchard=0 do not match observed sapling=0 orchard=0
```

The upstream-node-supplied checkpoint tree state arrives intact (107,795 sapling notes at height 1,499,999), but the chain-epoch's observed sapling tree size stays at 0. The first post-checkpoint block declares `sapling=107795` (the count it inherits from the chain) and validation fails immediately. This says the backfill bootstrap path **records the upstream-node-supplied tree size as part of the checkpoint metadata but does not initialize the running chain-epoch observer with it**, so any block whose declared tree size is non-zero fails the equality check.

The two failures read as the same root cause: the chain-epoch tree-size observer initializes to zero regardless of what `backfill_checkpoint_resolved` reports. Together they make it impossible to seed a wallet-serving zinder by deep-backfilling from sapling activation, which is the only way to make the previous finding go away.

The relevant code is in `services/zinder-ingest/src/backfill.rs` and `crates/zinder-store/src/chain_store/validation.rs`; the equality check that emits the error is `validate_subtree_root_artifacts` plus the per-block tree-size check the artifact builder runs before commit. A fix likely needs the bootstrap path to initialize `ChainEpoch::shielded_tree_sizes` (or equivalent) from the upstream-node-supplied checkpoint values rather than zero.

Resolved in the M2 hardening slice for the production backfill path:
`IngestArtifactBuilder::with_initial_tip_metadata(...)` is seeded from the
resolved checkpoint metadata before the first post-checkpoint block is
validated. The regression test
`backfill_seeds_compact_metadata_from_nonzero_checkpoint` covers a non-zero
Sapling checkpoint plus a post-checkpoint block whose observed tree size
inherits that count.

The same slice also fixed the partial tree-state observation edge behind
Failure A: if Zebra exposes an explicit empty sibling pool but does not expose a
non-empty pool's size, zinder no longer defaults the unknown pool to zero. The
tree-size check now compares only pool sizes the upstream node actually
reported.

### `writer-status` polling is loud when the writer is the regtest stack on a different control endpoint

The compat process polls `storage.ingest_control_addr` for writer status. On startup before the testnet ingest had bound 9201, the compat happened to find a different listener (the regtest `zinder-query` was on 9101) and logged `WARN ... writer-status fetch failed ... Operation is not implemented or not supported` once per second until the testnet ingest came up. Once both processes were on matching ports the warnings stopped.

This is a multi-stack-on-one-host artifact, not a storage-sync defect. The M2
hardening slice maps this condition to
`ReadinessCause::WriterStatusUnavailable` and includes the configured endpoint
plus expected `zinder.v1.ingest.IngestControl/WriterStatus` method in the
structured warning, so a port collision is diagnosable from the query logs.

## Implications for deployment

The findings above translate to four concrete deployment constraints:

1. **Backfill depth.** A zinder instance serving wallet clients must use `zinder-ingest backfill --wallet-serving`, not a recent checkpoint. That mode asks the upstream node for activation heights through `getblockchaininfo`, derives the serving floor, resolves the parent checkpoint, and ingests from the floor. Tip-style bootstrapping via `--checkpoint-height = tip - 50` is appropriate for storage validation and observability smokes, not for serving. The same depth requirement applies to tree-state coverage: every anchor height a supported wallet flow can request must be at or above the store's first ingested tree-state height.
2. **Transparent UTXO stream.** Zashi-compatible serving requires the compat `GetAddressUtxosStream` path to stay backed by stored transparent UTXO artifacts. Synthetic empty responses, upstream node fallbacks, or compact-block scans are regressions.
3. **TLS termination.** The Android SDK supports plaintext at the model layer (`co.electriccoin.lightwallet.client.model.LightWalletEndpoint(... isSecure = false)`) but the Zashi wallet's `ValidateEndpointUseCase` hardcodes `isSecure = true` for any user-supplied endpoint. Production zinder serving Zashi requires a TLS-terminating front (Caddy, nginx, traefik) speaking HTTPS to the wallet and h2c to the local compat process. The path verified here is `caddy { tls internal; reverse_proxy h2c://127.0.0.1:9077 }` with the Caddy local CA root added to the wallet's `network_security_config.xml` `<trust-anchors>`; this works for development. Production needs a real cert.
4. **Wallet anchor vs. store floor.** Even with TLS in place and ingest running, any wallet flow that requests `GetTreeState` below the zinder store's first ingested tree-state height will receive `NOT_FOUND`. The re-tested Zashi "Create new wallet" flow anchored at the current tip and completed, but Zashi Resync anchored below the near-tip store floor and stayed at `0.0%`. Restored-wallet and historical-birthday flows are the same class of risk.

A downstream-Zashi note worth recording even though it is not a zinder defect: the `zcashtestnetInternalDebug` flavor in `zodl-android` ships without a `bools.xml` `zcash_is_testnet=true` override, so it inherits the mainnet default and the wallet rejects any `chainName: "test"` server with "this client expects a server using mainnet but it was test." Adding `app/src/zcashtestnetInternalDebug/res/values/bools.xml` with `<bool name="zcash_is_testnet">true</bool>` fixes the flavor's network detection. This is mentioned here so that Zashi-against-zinder reproductions on a fresh `zodl-android` checkout do not get triaged as a zinder bug.

## Reproduction

Bring up a parallel testnet zinder against an existing testnet Zebra:

```bash
mkdir -p .tmp/testnet/{config,store,secondary,logs}

# Cookie auth from the running container
COOKIE=$(docker exec z3_zebra cat /var/run/auth/.cookie)

cat > .tmp/testnet/config/zinder-ingest.toml <<TOML
[network]
name = "zcash-testnet"

[node]
source = "zebra-json-rpc"
json_rpc_addr = "http://127.0.0.1:18232"

[node.auth]
method = "basic"
username = "${COOKIE%%:*}"
password = "${COOKIE#*:}"

[storage]
path = ".tmp/testnet/store"

[ingest.control]
listen_addr = "127.0.0.1:9201"
TOML

cat > .tmp/testnet/config/zinder-compat-lightwalletd.toml <<TOML
[network]
name = "zcash-testnet"

[storage]
path = ".tmp/testnet/store"
secondary_path = ".tmp/testnet/secondary"
ingest_control_addr = "http://127.0.0.1:9201"

[compat]
listen_addr = "0.0.0.0:9077"

[node]
json_rpc_addr = "http://127.0.0.1:18232"

[node.auth]
method = "basic"
username = "${COOKIE%%:*}"
password = "${COOKIE#*:}"
TOML

# Backfill near tip
TIP=$(curl -sS -u "${COOKIE%%:*}:${COOKIE#*:}" -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":"x","method":"getblockcount","params":[]}' \
  http://127.0.0.1:18232 | jq -r '.result')
./target/debug/zinder-ingest \
  --config .tmp/testnet/config/zinder-ingest.toml \
  backfill \
    --checkpoint-height $((TIP - 50)) \
    --from-height $((TIP - 49)) \
    --to-height "$TIP" \
    --commit-batch-blocks 25 \
    --allow-near-tip-finalize

# Long-running tip follow + compat
nohup ./target/debug/zinder-ingest --config .tmp/testnet/config/zinder-ingest.toml --ops-listen-addr 0.0.0.0:9290 tip-follow >> .tmp/testnet/logs/ingest.log 2>&1 &
nohup ./target/debug/zinder-compat-lightwalletd --config .tmp/testnet/config/zinder-compat-lightwalletd.toml --ops-listen-addr 0.0.0.0:9292 >> .tmp/testnet/logs/compat.log 2>&1 &
```

The command above intentionally reproduces the store-floor failure. To build a
serving store instead, omit `--checkpoint-height` and `--from-height`, pass
`--wallet-serving`, and backfill only through a height outside the configured
reorg window. Do not use `--allow-near-tip-finalize` for a serving store.
Tip-follow catches up the replaceable near-tip suffix afterward.

```bash
REORG_WINDOW_BLOCKS=100
SAFE_TO_HEIGHT=$((TIP - REORG_WINDOW_BLOCKS))
./target/debug/zinder-ingest \
  --config .tmp/testnet/config/zinder-ingest.toml \
  backfill \
    --wallet-serving \
    --to-height "$SAFE_TO_HEIGHT" \
    --commit-batch-blocks 25
```

Build and install the demo-app pointed at the LAN IP:

```bash
cd zcash-android-wallet-sdk
# Patch demo-app/src/main/java/cash/z/ecc/android/sdk/demoapp/ext/LightWalletEndpointExt.kt:
#   LightWalletEndpoint.Testnet -> LightWalletEndpoint("<LAN_IP>", 9077, isSecure = false)
# Add demo-app/src/main/res/xml/network_security_config.xml allowing cleartext to <LAN_IP>.
# Reference it from demo-app/src/main/AndroidManifest.xml's <application> tag.
./gradlew :demo-app:installZcashtestnetDebug
adb shell am start -n cash.z.ecc.android.sdk.demoapp.testnet/cash.z.ecc.android.sdk.demoapp.ComposeActivity
```

The wallet's choice of seed (Alyssa, Ben, Generate, custom) does not change the friction; all paths trip `GetSubtreeRoots(start_index=0)` against a tip-bootstrapped store. Use `adb logcat --pid $(adb shell pidof cash.z.ecc.android.sdk.demoapp.testnet)` to watch the failure in real time.

For the Zashi half, with the demo-app stack already up:

```bash
# Caddy in front of compat
mkdir -p .tmp/testnet/caddy
cat > .tmp/testnet/caddy/Caddyfile <<'CADDY'
{
    local_certs
    admin off
    auto_https disable_redirects
}
192.168.1.117:8443 {
    tls internal
    reverse_proxy h2c://127.0.0.1:9077
}
CADDY
( cd .tmp/testnet/caddy && nohup caddy run --config Caddyfile >> caddy.log 2>&1 & )

# Bundle Caddy CA into the zodl-android testnet flavor
cp ~/Library/Application\ Support/Caddy/pki/authorities/local/root.crt \
   "$ZODL/app/src/zcashtestnetInternalDebug/res/raw/zinder_caddy_ca.crt"
# Add app/src/zcashtestnetInternalDebug/res/xml/network_security_config.xml with
#   <domain>192.168.1.117</domain> trusting @raw/zinder_caddy_ca and system.
# Reference it from app/src/zcashtestnetInternalDebug/AndroidManifest.xml
#   via android:networkSecurityConfig with tools:replace.
# Add app/src/zcashtestnetInternalDebug/res/values/bools.xml with
#   <bool name="zcash_is_testnet">true</bool> (otherwise the variant
#   inherits mainnet from app/src/main/res/values/bools.xml).

# Build against the local SDK tree
echo "SDK_INCLUDED_BUILD_PATH=$ZCASH_ANDROID_SDK" >> ~/.gradle/gradle.properties
( cd "$ZODL" && ./gradlew :app:installZcashtestnetInternalDebug )

adb shell am start -n co.electriccoin.zcash.testnet.internal.debug/co.electricoin.zcash.LauncherActivity
# In-app: Settings → Choose server → Custom → 192.168.1.117:8443 → Save.
adb logcat --pid $(adb shell pidof co.electriccoin.zcash.testnet.internal.debug) | grep -E "Twig|LightWalletClient"
```

With the rebuilt near-tip reproduction store, Zashi's "Create new wallet" flow
completed because the SDK requested `GetTreeState` at the current tip. To
reproduce the store-floor failure through a real wallet flow, trigger Settings
-> Advanced Settings -> Resync Wallet; the SDK anchors the scan below the store
floor and loops at `0.0%`. A direct `GetTreeState` probe for a height below the
first ingested tree-state artifact, for example `3,974,000` against the store
described above, reproduces the same server-side `NOT_FOUND`.

## Open questions for follow-up

- Should `services/zinder-query/src/lib.rs::subtree_roots_at_epoch` return a sparse vector of `Option<SubtreeRoot>` instead of failing the whole batch, leaving the SDK to fail gracefully on its own gaps? This trades protocol fidelity (lightwalletd does not return sparse) for operational tolerance.
- Should `GetTreeState` below the store's first ingested height stay strict forever, or should a future repair tool materialize missing canonical tree-state artifacts from upstream node observations before query sees them? Query-time fallback is intentionally not part of M2.

## M3 mempool source-code evidence

This section records why Android SDK and Zashi/Zodl appear in the M3 mempool
consumer map. The canonical product contract remains
[Wallet data plane §Mempool Snapshot and Subscription](../architecture/wallet-data-plane.md#mempool-snapshot-and-subscription-m3);
this page only keeps the local source evidence.

- The Android SDK's
  `zcash-android-wallet-sdk/lightwallet-client-lib/src/main/java/co/electriccoin/lightwallet/client/internal/LightWalletClientImpl.kt:307`
  calls `GetMempoolStream` through `WalletClient.observeMempool`.
- `zcash-android-wallet-sdk/sdk-lib/src/main/java/cash/z/ecc/android/sdk/SdkSynchronizer.kt:746` launches
  `CompactBlockProcessor.startObservingMempool()` as part of normal startup.
- `zcash-android-wallet-sdk/sdk-lib/src/main/java/cash/z/ecc/android/sdk/block/processor/CompactBlockProcessor.kt:426-440`
  observes mempool raw transactions, attempts local decryption, and triggers
  transaction checks when an observed mempool transaction matches wallet state.
- `zcash-android-wallet-sdk/sdk-lib/src/main/java/cash/z/ecc/android/sdk/internal/transaction/OutboundTransactionManagerImpl.kt:72-105`
  submits through the lightwalletd client and maps gRPC or non-zero responses
  into `TransactionSubmitResult`.
- Zodl consumes this at the SDK layer:
  `zodl-android/ui-lib/src/main/java/co/electriccoin/zcash/ui/common/datasource/ProposalDataSource.kt:240`
  calls `SdkSynchronizer.createProposedTransactions`, and
  `zodl-android/ui-lib/src/main/java/co/electriccoin/zcash/ui/common/repository/TransactionRepository.kt:199`
  maps SDK transaction state into pending UI state.

These call paths make M3 a Zashi/Zodl pending-transaction UX and parity feature.
They do not change the native Zinder contract names: `GetMempoolStream` and
`GetMempoolTx` remain compatibility adapter methods over
`WalletQuery.MempoolSnapshot` and `WalletQuery.MempoolEvents`.

The remaining transparent-address work is full history, balance, and mempool
parity; the UTXO stream and wallet-serving backfill floor are no longer release
blockers in this evidence log.
