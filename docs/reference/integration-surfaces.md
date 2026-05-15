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
- `ChainIndex` is the async trait shared by both implementations.

Native clients get typed errors, capability discovery, epoch-pinned reads, chain-event cursors, transaction broadcast results, mempool reads, and transparent-address artifacts without depending on the lightwalletd compatibility layer.

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
