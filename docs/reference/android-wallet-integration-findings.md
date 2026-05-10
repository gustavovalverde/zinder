# Findings from Android wallet integration

This page is the operator-facing companion to the canonical Android SDK and Zashi/Zodl serving contract in [Wallet data plane §External Wallet Compatibility Claims](../architecture/wallet-data-plane.md#external-wallet-compatibility-claims). It records the wire-compat surface that has been verified end-to-end against `zcash-android-wallet-sdk` v2.4.8 (`demo-app`, `zcashtestnetDebug`) and Zashi (`zodl-android`, `zcashtestnetInternalDebug`) on Pixel 10 Pro / Android 16, plus the durable deployment constraints those runs surfaced.

The compatibility surface exists so existing lightwalletd clients can migrate without code changes. The SDK behaves correctly against the lightwalletd protocol as published, and Zinder behaves correctly against its advertised scope.

## Wire-compat surface verified

The vendored `cash.z.wallet.sdk.rpc.CompactTxStreamer` schema in `crates/zinder-proto/proto/compat/lightwalletd/` matches the schema the Android SDK was generated against. `grpcurl` returns valid responses for `GetLightdInfo`, `GetLatestBlock`, `GetBlock`, and `GetBlockRange` without any field-shape adjustments. `chainName: "test"` is the value the SDK matches against `ZcashNetwork.ID_TESTNET`, so the wallet accepts the server as a valid testnet endpoint.

The SDK's `CompactBlockProcessor` advances cleanly through `getLightdInfo → fetchLatestBlockHeight → updateChainTip → suggestScanRanges`. The wallet's view of the chain matches Zinder's view at the tip, on both plaintext (`demo-app`) and TLS (Zashi over Caddy `tls internal`) transports.

`GetMempoolTx` and `GetMempoolStream` produce byte-shape-correct responses against an in-flight self-send. `GetMempoolTx` returns a `CompactTx` with the canonical Orchard-action shape (32-byte `nullifier`, 32-byte `cmx`, 32-byte `ephemeralKey`, 52-byte compact-note `ciphertext` prefix). `GetMempoolStream` opens a server-streaming subscription, emits `RawTransaction { data, height }` for the in-flight tx, keeps the stream open while the tx sits in mempool, and closes cleanly when the tx mines into the next block. This is the lightwalletd Go server's de-facto contract verbatim.

The Send path runs end-to-end from tx construction through compat broadcast, `sendrawtransaction`, mempool acceptance, mining, and wallet scan-back. The wallet correctly attributes only the fee to user-visible Activity entries when the recipient is one of the wallet's own diversified UAs.

Zashi's TLS-only `ValidateEndpointUseCase` reaches the same sync state machine when fronted by Caddy with `tls internal` and the Caddy local-CA root added to the wallet's `network_security_config.xml` `<trust-anchors>`. The compat process keeps speaking h2c on `0.0.0.0:9077`; Caddy handles TLS termination and the HTTP/2 cleartext proxying. This validates the production deployment shape (TLS in front, plaintext h2c gRPC behind).

## Implications for deployment

The four operator constraints below are the durable output of this integration:

1. **Backfill depth.** A Zinder instance serving wallet clients must use `zinder-ingest backfill --wallet-serving`, not a recent checkpoint. That mode asks the upstream node for activation heights through `getblockchaininfo`, derives the serving floor, resolves the parent checkpoint, and ingests from the floor. Tip-style bootstrapping via `--checkpoint-height = tip - 50` is appropriate for storage validation and observability smokes, not for serving. The same depth requirement applies to tree-state coverage: every anchor height a supported wallet flow can request must be at or above the store's first ingested tree-state height.
2. **Transparent UTXO stream.** Zashi-compatible serving requires the compat `GetAddressUtxosStream` path to stay backed by stored transparent UTXO artifacts. Synthetic empty responses, upstream node fallbacks, or compact-block scans are regressions.
3. **TLS termination.** The Android SDK supports plaintext at the model layer (`co.electriccoin.lightwallet.client.model.LightWalletEndpoint(... isSecure = false)`) but Zashi's `ValidateEndpointUseCase` hardcodes `isSecure = true` for any user-supplied endpoint. Production Zinder serving Zashi requires a TLS-terminating front (Caddy, nginx, traefik) speaking HTTPS to the wallet and h2c to the local compat process. The path verified here is `caddy { tls internal; reverse_proxy h2c://127.0.0.1:9077 }`; production needs a real cert.
4. **Wallet anchor vs. store floor.** Even with TLS in place and ingest running, any wallet flow that requests `GetTreeState` below the Zinder store's first ingested tree-state height receives `NOT_FOUND`. Restored-wallet, historical-birthday, and Resync flows are the same class of risk; Zashi's "Create new wallet" flow happens to anchor at the current tip and so avoids it. Query stays strict by design: synthesizing upstream-node fallback tree states would bypass the canonical store contract.

A downstream-Zashi note: the `zcashtestnetInternalDebug` flavor in `zodl-android` ships without a `bools.xml` `zcash_is_testnet=true` override, so it inherits the mainnet default and the wallet rejects any `chainName: "test"` server with "this client expects a server using mainnet but it was test." Adding `app/src/zcashtestnetInternalDebug/res/values/bools.xml` with `<bool name="zcash_is_testnet">true</bool>` fixes the flavor's network detection.

A second operational class to watch: when `tip-follow` lags far behind the upstream node, every other surface looks healthy but Send fails. The wallet computes `expiry_height = anchor + delta` from `GetLightdInfo.blockHeight`; the upstream validator checks the tx against its real tip. If the gap exceeds the SDK's expiry-height window, Zebra rejects with `transaction must not be mined at a block Height(...) greater than its expiry Height(...)`. Reads keep working, sync looks healthy, but Send is broken until the gap closes. Zinder's `/readyz` already exposes this signal as `{"status":"not_ready","cause":{"syncing":{"lag_blocks":N}},...}`; deployments serving sends should alert on `lag_blocks` exceeding the SDK's expiry buffer.

## Reproduction

Bring up a wallet-serving Zinder against an existing testnet Zebra:

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

TIP=$(curl -sS -u "${COOKIE%%:*}:${COOKIE#*:}" -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":"x","method":"getblockcount","params":[]}' \
  http://127.0.0.1:18232 | jq -r '.result')

REORG_WINDOW_BLOCKS=100
SAFE_TO_HEIGHT=$((TIP - REORG_WINDOW_BLOCKS))

./target/debug/zinder-ingest \
  --config .tmp/testnet/config/zinder-ingest.toml \
  backfill \
    --wallet-serving \
    --to-height "$SAFE_TO_HEIGHT" \
    --commit-batch-blocks 25

nohup ./target/debug/zinder-ingest --config .tmp/testnet/config/zinder-ingest.toml --ops-listen-addr 0.0.0.0:9290 tip-follow >> .tmp/testnet/logs/ingest.log 2>&1 &
nohup ./target/debug/zinder-compat-lightwalletd --config .tmp/testnet/config/zinder-compat-lightwalletd.toml --ops-listen-addr 0.0.0.0:9292 >> .tmp/testnet/logs/compat.log 2>&1 &
```

Build and install the SDK demo-app pointed at the LAN IP:

```bash
cd zcash-android-wallet-sdk
# Patch demo-app/src/main/java/cash/z/ecc/android/sdk/demoapp/ext/LightWalletEndpointExt.kt:
#   LightWalletEndpoint.Testnet -> LightWalletEndpoint("<LAN_IP>", 9077, isSecure = false)
# Add demo-app/src/main/res/xml/network_security_config.xml allowing cleartext to <LAN_IP>.
# Reference it from demo-app/src/main/AndroidManifest.xml's <application> tag.
./gradlew :demo-app:installZcashtestnetDebug
adb shell am start -n cash.z.ecc.android.sdk.demoapp.testnet/cash.z.ecc.android.sdk.demoapp.ComposeActivity
```

For Zashi, front the compat with Caddy and bundle the local CA:

```bash
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
#   <bool name="zcash_is_testnet">true</bool>.

echo "SDK_INCLUDED_BUILD_PATH=$ZCASH_ANDROID_SDK" >> ~/.gradle/gradle.properties
( cd "$ZODL" && ./gradlew :app:installZcashtestnetInternalDebug )

adb shell am start -n co.electriccoin.zcash.testnet.internal.debug/co.electricoin.zcash.LauncherActivity
# In-app: Settings → Choose server → Custom → 192.168.1.117:8443 → Save.
adb logcat --pid $(adb shell pidof co.electriccoin.zcash.testnet.internal.debug) | grep -E "Twig|LightWalletClient"
```

## Open questions

- Should `services/zinder-query/src/lib.rs::subtree_roots_at_epoch` return a sparse vector of `Option<SubtreeRoot>` instead of failing the whole batch, so the SDK can fail gracefully on its own gaps? Trade-off: protocol fidelity (lightwalletd does not return sparse) vs. operational tolerance.
- Should `GetTreeState` below the store's first ingested height stay strict forever, or should a repair tool materialize missing canonical tree-state artifacts from upstream node observations before query sees them? Query-time fallback is intentionally not part of the current query contract.

## Why mempool compatibility belongs in the wallet contract

The Android SDK and Zashi/Zodl exercise mempool calls on every startup, not only during sends. The product contract is in [Wallet data plane §Mempool Snapshot and Subscription](../architecture/wallet-data-plane.md#mempool-snapshot-and-subscription); the source-code evidence:

- `lightwallet-client-lib/.../LightWalletClientImpl.kt:307` calls `GetMempoolStream` through `WalletClient.observeMempool`.
- `sdk-lib/.../SdkSynchronizer.kt:746` launches `CompactBlockProcessor.startObservingMempool()` as part of normal startup.
- `sdk-lib/.../CompactBlockProcessor.kt:426-440` decrypts observed mempool raw transactions locally and triggers transaction checks when an observed tx matches wallet state.
- `sdk-lib/.../OutboundTransactionManagerImpl.kt:72-105` submits through the lightwalletd client and maps gRPC responses into `TransactionSubmitResult`.
- Zodl consumes this at the SDK layer (`zodl-android/ui-lib/.../ProposalDataSource.kt:240`, `TransactionRepository.kt:199`).

`GetMempoolStream` and `GetMempoolTx` are compatibility-adapter methods over the native `WalletQuery.MempoolSnapshot` and `WalletQuery.MempoolEvents`. The compat adapter requires an `IngestControlMempoolSurface`; when none is configured, calls return `Status::unavailable("mempool surface is not configured")` so operators can differentiate "feature off" from "feature missing".
