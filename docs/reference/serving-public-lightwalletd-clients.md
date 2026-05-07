# Serving public lightwalletd clients

| Field    | Value                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Status   | Background research                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| Audience | Zinder maintainers, operators, downstream wallet developers                                                                                                                                                                                                                                                                                                                                                                                                            |
| Date     | 2026-05-07                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| Related  | [Wallet data plane](../architecture/wallet-data-plane.md), [Service operations](../architecture/service-operations.md), [Protocol boundary](../architecture/protocol-boundary.md), [PRD-0001](../prd-0001-zinder-indexer.md), [ADR-0007](../adrs/0007-multi-process-storage-access.md), [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md), [ADR-0009](../adrs/0009-ingest-control-transport-security.md), [Findings from Android wallet integration](android-wallet-integration-findings.md) |

## Purpose

The Zodl Android wallet (formerly Zashi), the `zcash-android-wallet-sdk` `demo-app`, and other lightwalletd-protocol clients reach their backend through `CompactTxStreamer` over HTTPS gRPC on port 443. Community-run servers (`zec.rocks`, `*.zec.rocks`, `*.zec.stardust.rest`) implement that contract and ship in the wallet's default endpoint list. This document records what an operator must do, on top of what Zinder already ships, to serve those same clients from a Zinder deployment, and what gates still apply before a release can claim Zashi/Zodl compatibility.

This is operator and integration territory. The wire surface is owned by [Protocol boundary](../architecture/protocol-boundary.md); the deployment posture is owned by [Service operations](../architecture/service-operations.md); the compatibility-claim contract is owned by [Wallet data plane §External Wallet Compatibility Claims](../architecture/wallet-data-plane.md#external-wallet-compatibility-claims). This document points at those, not around them.

## Wire-protocol equivalence

`zinder-compat-lightwalletd` serves the vendored `CompactTxStreamer` schema pinned in `crates/zinder-proto/proto/compat/lightwalletd/COMMIT`. The Android SDK selects an endpoint by `(host, port, isSecure)`; it does not distinguish lightwalletd, Zaino, `zec.rocks`, or Zinder behind that tuple. As long as the endpoint answers the read-sync surface enumerated in [Protocol boundary §Lightwalletd Compatibility](../architecture/protocol-boundary.md#lightwalletd-compatibility), the SDK treats it as one of its servers.

The Zodl Android client list lives in `ui-lib/src/main/java/co/electriccoin/zcash/ui/common/provider/LightWalletEndpointProvider.kt:14-30` (zodl-android repo). Servers in that list are ranked by `getFastestServers` from the SDK; `getDefaultEndpoint()` returns the first list entry. Both selection paths are network-aware: mainnet and testnet have separate endpoint lists in the same file.

## What Zinder already ships

Per [Service operations](../architecture/service-operations.md):

- Three deployable binaries: `zinder-ingest` (sole writer), `zinder-query` (native gRPC API), `zinder-compat-lightwalletd` (lightwalletd adapter, default listen `127.0.0.1:9067`).
- Operational HTTP surface (`/healthz`, `/readyz`, `/metrics`) on a separate `--ops-listen-addr` socket, with typed readiness causes per [Service operations §Health and Readiness](../architecture/service-operations.md#health-and-readiness).
- Production config validation that fails closed on placeholder credentials, missing storage, mismatched network, and unsafe binds.
- `zinder-ingest backfill --wallet-serving` to seed the historical floor required by lightwalletd-compatible bootstrap, validated end-to-end on testnet against Zashi on 2026-04-29 (see [Findings from Android wallet integration](android-wallet-integration-findings.md)).
- Plaintext h2c on every gRPC port, including the public client port; the private `IngestControl` plane is governed by [ADR-0009](../adrs/0009-ingest-control-transport-security.md).

What Zinder does not ship, by deliberate v1 scope cut (`README.md` and [PRD-0001](../prd-0001-zinder-indexer.md)): public multi-tenant hosting, TLS termination, authentication, rate limiting, and quota accounting.

## Capability comparison

| Capability                                                       | `zec.rocks` and `*.zec.stardust.rest` | Zinder out of the box                                | Operator gap                                                          |
| ---------------------------------------------------------------- | ------------------------------------- | ---------------------------------------------------- | --------------------------------------------------------------------- |
| `CompactTxStreamer` wire protocol                                | yes                                   | yes (`zinder-compat-lightwalletd`)                   | none                                                                  |
| Public DNS                                                       | yes                                   | none                                                 | register a hostname                                                   |
| TLS on port 443 with publicly-trusted cert                       | yes                                   | no (plaintext h2c)                                   | terminate TLS in front; Caddy, nginx, or traefik                      |
| Public reachability                                              | yes                                   | localhost-only by default                            | host on a public VM or container                                      |
| Rate limiting and abuse control                                  | yes (proxy-level)                     | no                                                   | configure in the proxy layer                                          |
| Wallet-bootstrap dataset depth                                   | yes                                   | only after `zinder-ingest backfill --wallet-serving` | run ingest in wallet-serving mode                                     |
| `GetAddressUtxos`, `GetAddressUtxosStream`, `taddrSupport: true` | yes                                   | wired (per the 2026-04-29 re-test)                   | none in code; the store must satisfy the wallet-serving floor         |
| Send path with low expiry-window risk                            | yes                                   | depends on `tip-follow` lag                          | monitor writer-tip vs. node-tip and surface a typed not-ready cause   |

Six of the eight gaps are operator-side glue. The wallet-bootstrap depth and the send-path tip-lag concerns are operational, not code.

## Public deployment shape

```text
[Internet] --HTTPS:443-- [Caddy/nginx/traefik] --h2c:127.0.0.1:9067-- [zinder-compat-lightwalletd]
                                                                              |
                                                                              v h2c (private)
                                                                       [zinder-query]
                                                                              |
                                                                              v secondary RocksDB
                                                                       [zinder-ingest]
                                                                              |
                                                                              v JSON-RPC + cookie/basic
                                                                       [Zebra node]
```

This is the same topology the testnet pilot uses; see [Findings from Android wallet integration](android-wallet-integration-findings.md) for the existing Caddy + h2c forward against `zinder-compat-lightwalletd`. The only difference for a public mainnet endpoint is the cert source: a publicly-trusted CA (Let's Encrypt) rather than a private CA, so wallets do not need to ship a custom trust anchor.

The private `IngestControl` plane between writer and readers stays inside the trust boundary. Pattern selection (localhost, VPN with shared-secret token, or HTTPS reverse proxy with token) is owned by [ADR-0009](../adrs/0009-ingest-control-transport-security.md). Do not expose `IngestControl` to the internet.

## Operator recipe

Single-host mainnet pilot. Multi-host shapes follow [ADR-0007](../adrs/0007-multi-process-storage-access.md) for the writer/reader topology and [ADR-0009](../adrs/0009-ingest-control-transport-security.md) for the control-plane trust boundary.

1. Run a Zebra node on the host or LAN. Enable cookie or basic auth and record the JSON-RPC address.
2. Configure `zinder-ingest` per [Public interfaces §Configuration Conventions](../architecture/public-interfaces.md#configuration-conventions). Run `zinder-ingest backfill --wallet-serving` first, then `tip-follow` for steady state.
3. Run `zinder-query` and `zinder-compat-lightwalletd` against the same canonical store. Both default to `127.0.0.1` ports.
4. Front the compat port with a reverse proxy that terminates HTTPS and forwards h2c. Minimal Caddyfile:

   ```caddyfile
   mainnet.your-domain.tld {
       reverse_proxy h2c://127.0.0.1:9067 {
           transport http {
               versions h2c
           }
       }
   }
   ```

   Caddy provisions Let's Encrypt automatically; nginx and traefik need explicit ACME or cert-file configuration plus `grpc_pass` (nginx) or `h2c` scheme (traefik) for the upstream.
5. Add proxy-level rate limiting and abuse rules. Zinder does not implement these.
6. Verify the endpoint with the lightwalletd `GetLightdInfo` method through `grpcurl`. The response should report a non-zero `block_height` and `taddrSupport: true` once `--wallet-serving` ingest catches up.
7. Wire alerting on `zinder_readiness_state`, `zinder_ingest_writer_tip_height`, and the writer-vs-node tip gap so the send-path failure mode in [Findings from Android wallet integration](android-wallet-integration-findings.md) (2026-05-07) surfaces as a typed not-ready cause rather than as silent send rejections.

## Wallet-side integration

A wallet user can reach a freshly-published Zinder through any of three escalating mechanisms in zodl-android. None require Zinder code changes.

1. **User-typed endpoint.** The in-app *Choose Server* screen calls `ValidateEndpointUseCase` on the entered string and `PersistEndpointUseCase` on accept. Useful for early pilots and internal testers.
2. **Default endpoint list.** Editing `LightWalletEndpointProvider.kt:14-30` (zodl-android repo) to include the new hostname adds it to `getFastestServers` ranking; the wallet may select it automatically when it has the lowest latency.
3. **Default-of-the-defaults.** `getDefaultEndpoint()` returns `getEndpoints().first()`. Placing the new hostname at index 0 of the appropriate network list makes Zinder the fresh-install default.

For internal pilots fronted by a private CA (the `zcashtestnetInternalDebug` flavor pattern), the wallet bundles the CA cert under `app/src/zcashtestnetInternalDebug/res/raw/zinder_caddy_ca.crt` and pins it through `network_security_config.xml`. Public deployments using Let's Encrypt do not need this; the system trust store already covers them.

## Compatibility-claim gating

A deployment may claim Zashi or Zodl compatibility only when the conditions in [Wallet data plane §External Wallet Compatibility Claims](../architecture/wallet-data-plane.md#external-wallet-compatibility-claims) are met:

- The serving store covers the wallet-serving history floor (`zinder-ingest backfill --wallet-serving`).
- The transparent UTXO surface in [Wallet data plane §Transparent Address UTXOs](../architecture/wallet-data-plane.md#transparent-address-utxos) is implemented end-to-end. The 2026-04-29 re-test in [Findings from Android wallet integration](android-wallet-integration-findings.md) confirms the code path is wired.
- TLS in front of the compat process is verified against a real Zashi or `zcash-android-wallet-sdk` endpoint, not just the Go `lightwalletd/testclient`.

After M3, mempool compatibility adds the `GetMempoolStream` and `GetMempoolTx` mapping requirement per the same section. Until M3, a deployment can serve sync and broadcast but not pending-transaction UX. The 2026-05-07 send-path finding remains an operational concern at any milestone: tip-follow lag exceeding the SDK's expiry-height window breaks send even when every other surface looks healthy.
