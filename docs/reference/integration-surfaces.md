# Integration Surfaces

Zinder exposes three integration paths. Pick the path by the contract the client needs, not by implementation convenience.

| Client shape | Integration surface | Use when |
| --- | --- | --- |
| Lightwalletd-compatible mobile or SDK client | `zinder-compat-lightwalletd` | The client already speaks `CompactTxStreamer` and expects lightwalletd wire shapes. |
| Rust wallet or application | `zinder-client::RemoteChainIndex` or `LocalChainIndex` | The client can use the typed `ChainIndex` trait and wants native Zinder errors, cursors, and `ChainEpoch` values. |
| Explorer, analytics, or derived view | `WalletQuery` plus `ExplorerQuery` through the derive plane | The view is rebuilt from canonical artifacts and should not affect wallet sync correctness. |

## Lightwalletd Compatibility

`zinder-compat-lightwalletd` serves the vendored lightwalletd `CompactTxStreamer` protocol by translating requests onto `WalletQueryApi`. It does not call Zebra, write canonical storage, or build artifacts independently. Operators expose it when they need a lightwalletd-compatible endpoint for wallets such as Zodl or SDKs generated from the lightwalletd protos.

Public deployments terminate TLS, authentication, rate limiting, and quota controls before traffic reaches Zinder. The compatibility process speaks plaintext h2c by default and should be bound behind the operator's proxy boundary.

## Native Rust Clients

`zinder-client` is the canonical Rust integration crate:

- `RemoteChainIndex` connects to a `WalletQuery` gRPC endpoint.
- `LocalChainIndex` opens a local RocksDB secondary when colocated with the writer.

The contract is split across two async traits so the compiler expresses which calls a handle can serve:

- `ChainIndex` carries the canonical and derive-store reads. Both adapters implement it identically: compact blocks, tree state, subtree roots, transparent-address unspent outputs and tx-history, canonical prevout resolution, and the confirmed transparent-address balance.
- `EndpointBackedIndex` carries the reads that need a live ingest-control/broadcast endpoint: transaction broadcast, the chain-event stream, live-mempool snapshot/events/overlays, chain value-pools, and the wallet-plane server descriptor. Only `RemoteChainIndex` implements it.

A consumer that broadcasts or subscribes bounds its handle `T: ChainIndex + EndpointBackedIndex`; passing a `LocalChainIndex` there is a compile error rather than a runtime "endpoint not configured" failure. Typed capability discovery (`CapabilityDescriptor::supports(Capability::…)`) probes the advertised set without matching raw strings.

Native clients get typed errors, capability discovery, epoch-pinned reads, chain-event cursors, transaction broadcast results, mempool reads, and transparent-address artifacts without depending on the lightwalletd compatibility layer.

## Vendoring the Protocol

Consumers that cannot depend on `zinder-client` (non-Rust stacks, toolchain or dependency clashes with the workspace) vendor the `.proto` files and generate their own stubs. The protocol packages are self-contained; no `googleapis` or third-party proto is imported.

### File set

The import closure under `crates/zinder-proto/proto/` is:

| Vendor when the consumer speaks | Files |
| --- | --- |
| `WalletQuery` | `zinder/v1/wallet/wallet.proto`, `zinder/v1/ops/server_info.proto` |
| `IngestControl` (adds to the above) | `zinder/v1/ingest/ingest.proto`, `zinder/v1/ops/readiness.proto` |
| `ExplorerQuery` (adds to the wallet set) | `zinder/v1/explorer/explorer.proto` |

`zinder/v1/ops/error.proto` sits outside every import closure but defines the `ErrorReason` enum; vendor it alongside so error handling compiles against the same vocabulary the wire carries.

### Pin and drift guard

Mirror the pattern Zinder itself uses for the lightwalletd protos (`crates/zinder-proto/proto/compat/lightwalletd/COMMIT` plus the `vendored-proto` job in `.github/workflows/ci-pr.yml`):

1. Copy the files preserving the `zinder/v1/...` directory layout and record the pinned Zinder commit hash in a `COMMIT` file next to them.
2. Generate stubs with the consumer's own toolchain (`tonic-prost-build` for Rust; any `protoc` plugin elsewhere). Do not edit the vendored files.
3. Add a CI job that fetches each file at the pinned commit and diffs it against the vendored copy, so local edits and upstream drift both fail loudly.

### Contract surfaces outside the protos

Two vocabularies bind the integration but do not appear in any `.proto` message definition:

- Capability strings. The authoritative table lives in `crates/zinder-proto/src/capabilities.rs`; servers advertise them through `ServerInfo.capabilities` and clients gate features on exact string match.
- Error reasons. `google.rpc.ErrorInfo` with `domain = "zinder.dev"` carries the string-form `ErrorReason` name on every failure; the [Error vocabulary](error-vocabulary.md) page owns the reason-to-code-to-retry semantics.

These two vocabularies plus `ServerInfo.contract_revision` are the machine-checkable integration keys. A vendored consumer asserts a minimum `contract_revision` at connect time and treats a lower value as an incompatible server.

### Feature detection

At connect time, call `ServerInfo` and check:

1. `network` matches the expected wire string exactly (`zcash-mainnet`, `zcash-testnet`, or `zcash-regtest`).
2. `contract_revision` meets the consumer's minimum.
3. Every capability the consumer requires is present in `capabilities`.

One caveat: the wallet-plane mempool capabilities (`wallet.snapshot.mempool_v1`, `wallet.events.mempool_v1`, the `wallet.mempool.*` reads) are always-on and advertised whether or not the deployment wires the ingest-control proxy that feeds them. `wallet.events.chain_v1` is gated on the deployment actually serving the chain-event stream, so a consumer that needs live-plane data probes that capability or issues a live call (for example `MempoolSnapshot`) and handles the failure.

## Server-Side Wallets

Server-side wallets pair `zinder-client` with librustzcash crates. Zinder owns chain reads and broadcast; the wallet process owns keys, trial decryption, note state, account state, transaction building, and proving. See [Server-side wallet pattern](server-side-wallet-pattern.md).

## Explorer And Analytics Views

Explorer-shaped reads use `WalletQuery` for canonical wallet-plane data and `ExplorerQuery` for derived views. The derive plane consumes canonical artifacts and event streams, owns its own storage, and can be rebuilt without touching canonical chain state.

## References

- [Indexer/wallet boundary](../architecture/indexer-wallet-boundary.md)
- [Wallet data plane](../architecture/wallet-data-plane.md)
- [Derive plane](../architecture/derive-plane.md)
- [Protocol boundary](../architecture/protocol-boundary.md)
- [Server-side wallet pattern](server-side-wallet-pattern.md)
