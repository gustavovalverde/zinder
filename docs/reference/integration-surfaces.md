# Integration Surfaces

This page starts after a client has chosen Zinder. It maps each client shape to
the smallest suitable contract, then uses existing wallet codebases to show
which methods an adapter would require. For the architectural choice between
Zebra, Zaino, Zinder, and lightwalletd, see
[Indexer/wallet boundary](../architecture/indexer-wallet-boundary.md).

## Pick a surface

| Client shape | Integration surface | Constraint |
| --- | --- | --- |
| Existing lightwalletd client | `zinder-compat-lightwalletd` | The client already speaks `CompactTxStreamer`; changing the endpoint should not require a protocol rewrite. |
| Rust wallet or service that needs live events or broadcast | `zinder-client::RemoteChainIndex` | The consumer can link the Zinder Rust crates and reach `zinder-query` over gRPC. |
| Rust read-only process colocated with canonical storage | `zinder-client::LocalChainIndex` | The process only needs stored reads and can manage a RocksDB secondary path. It cannot subscribe, broadcast, or read live mempool state. |
| Client that cannot link `zinder-client` | Vendored `WalletQuery` protos | The consumer generates a native gRPC client with its own language and toolchain. |
| Explorer or analytics application | `ExplorerQuery`, with `WalletQuery` where required | The consumer needs derived views rather than only wallet-sync artifacts. |

## Method mapping from existing wallets

The wallet codebases below demonstrate integration seams and method demand; they are not a Zinder support matrix. The mapping describes what an adapter could call on Zinder `main`, without assuming that a wallet-specific adapter has landed or passed end-to-end certification.

| Wallet codebase | Existing seam | Zinder methods or method families | Integration shape |
| --- | --- | --- | --- |
| [ZODL](https://github.com/zodl-inc/zodl-android) | Zcash Android SDK `LightWalletEndpoint` | `GetLightdInfo`, `GetLatestBlock`, `GetBlockRange`, `GetTreeState`, `GetSubtreeRoots`, `GetTransaction`, `GetAddressUtxosStream`, `GetTaddressTxids`, `SendTransaction`, `GetMempoolTx`, and `GetMempoolStream` | Point the existing `CompactTxStreamer` client at `zinder-compat-lightwalletd`; no native Zinder adapter is required. |
| [Vizor](https://github.com/chainapsis/vizor-wallet) | librustzcash `lightwalletd-tonic` client with a configurable endpoint | `GetLightdInfo`, latest block and block ranges, tree state, subtree roots, transaction lookup, transparent receiver discovery, `SendTransaction`, and mempool methods through librustzcash's sync engine | Point the existing `CompactTxStreamer` client at `zinder-compat-lightwalletd`; public deployments also need trusted TLS in front of Zinder. |
| [Zallet](https://github.com/zcash/zallet) | Backend-neutral `Chain` and snapshot-scoped `ChainView` traits | `ServerInfo`, `LatestBlock`, `FullBlocksInRange`, block headers, `SubtreeRootsInRange`, `Transaction`, transparent output and spend lookups, `ChainEvents`, `MempoolSnapshot`, `MempoolEvents`, and `BroadcastTransaction` | Implement a new native `WalletQuery` adapter behind the existing traits. This mapping does not claim that Zallet integration exists on Zinder `main`. |
| [Zally](https://github.com/gustavovalverde/zally) | `ChainSource` for reads and events, plus `Submitter` for broadcast | `LatestSafeBlock`, `LatestBlock`, `CompactBlocksInRange`, `TreeState`, `SubtreeRootsInRange`, `Transaction`, `TransparentAddressUnspentOutputs`, `ChainEvents`, and `BroadcastTransaction` | Map the traits to `RemoteChainIndex` and `EndpointBackedIndex`; `LocalChainIndex` cannot satisfy events or broadcast. |

Method coverage proves that Zinder has an appropriate public primitive. A support claim additionally requires the wallet's create or import, sync, recovery, send, mempool, and reorg flows against the selected wallet release and network.

## Lightwalletd compatibility

`zinder-compat-lightwalletd` serves the vendored lightwalletd
`CompactTxStreamer` protocol by translating requests onto `WalletQueryApi`. It
opens canonical and derive storage only through secondary readers. It does not
call Zebra, write canonical storage, or build artifacts independently.

For a compatible wallet, Zinder is an endpoint-level replacement for
lightwalletd: the wallet keeps the `CompactTxStreamer` contract and changes the
server address. It is not an operator-level binary or configuration replacement.
Zinder has its own ingest, storage, query, readiness, and deployment model.

Public deployments terminate TLS, authentication, rate limiting, and quota controls before traffic reaches Zinder. The compatibility process speaks plaintext h2c by default and should be bound behind the operator's proxy boundary.

## Native Rust clients

`zinder-client` is the canonical Rust integration crate:

- `RemoteChainIndex` connects to a `WalletQuery` gRPC endpoint.
- `LocalChainIndex` opens a local RocksDB secondary when colocated with the writer.

The contract is split across two async traits so the compiler expresses which calls a handle can serve:

- `ChainIndex` carries the canonical and derive-store reads. Both adapters implement it identically: compact blocks, tree state, subtree roots, transparent-address unspent outputs and tx-history, canonical prevout resolution, and the confirmed transparent-address balance.
- `EndpointBackedIndex` carries the reads that need a live ingest-control/broadcast endpoint: transaction broadcast, the chain-event stream, live-mempool snapshot/events/overlays, chain value-pools, and the wallet-plane server descriptor. Only `RemoteChainIndex` implements it.

A consumer that broadcasts or subscribes bounds its handle `T: ChainIndex + EndpointBackedIndex`; passing a `LocalChainIndex` there is a compile error rather than a runtime "endpoint not configured" failure. Typed capability discovery (`CapabilityDescriptor::supports(Capability::…)`) probes the advertised set without matching raw strings.

Native clients get typed errors, capability discovery, epoch-pinned reads,
chain-event cursors, transaction broadcast results, mempool reads, and
transparent-address artifacts without depending on the lightwalletd
compatibility layer.

## Vendoring the protocol

Consumers that cannot depend on `zinder-client` vendor the `.proto` files and
generate their own stubs. This includes non-Rust stacks and Rust consumers with
toolchain or native-dependency clashes. The protocol packages are
self-contained; no `googleapis` or third-party proto is imported.

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

## Server-side wallets

Server-side wallets can pair `zinder-client` with a higher-level wallet library
or directly with librustzcash crates. Zinder owns chain reads and broadcast; the
wallet process owns keys, trial decryption, note state, account state,
transaction building, and proving. See
[Server-side wallet pattern](server-side-wallet-pattern.md).

## Explorer and analytics views

Explorer-shaped reads use `WalletQuery` for canonical wallet-plane data and
`ExplorerQuery` for derived views. A consumer should call `WalletQuery` directly
for canonical blocks, transactions, tree state, broadcast, and wallet events.
It should call `ExplorerQuery` for summaries, search, history, distributions,
rankings, and other derived projections. The derive plane consumes canonical
artifacts and event streams, owns its own storage, and can be rebuilt without
touching canonical chain state.

## References

- [Indexer/wallet boundary](../architecture/indexer-wallet-boundary.md)
- [Wallet data plane](../architecture/wallet-data-plane.md)
- [Derive plane](../architecture/derive-plane.md)
- [Protocol boundary](../architecture/protocol-boundary.md)
- [Server-side wallet pattern](server-side-wallet-pattern.md)
