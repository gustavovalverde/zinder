# Known Consumers

This page lists the consumers known to integrate with Zinder, the integration shape each one uses, and a brief note on what kind of integration the consumer is. New integrators can read this to find prior art before they choose an integration path.

The list is not exhaustive and intentionally so. If you are integrating against Zinder and want to be listed, open a PR adding a row to the table below with the integration shape your project uses.

## Active integrations

| Consumer | Type | Integration shape | Notes |
| --- | --- | --- | --- |
| **Zashi** (mobile) | Mobile wallet | `zinder-compat-lightwalletd` | Uses the vendored lightwalletd protocol; treats Zinder as a drop-in `lightwalletd` replacement. The Android SDK driving Zashi is the same one driving the gap analysis in [Findings from Android wallet integration](android-wallet-integration-findings.md). |
| **Zodl** (mobile) | Mobile wallet | `zinder-compat-lightwalletd` | Same protocol path as Zashi; treats Zinder as a drop-in `lightwalletd` replacement. |
| **Zallet** | Full-node wallet process | `zinder-client::LocalChainIndex` / `RemoteChainIndex` | The companion full-node wallet; pairs with Zinder for chain reads + broadcast. See [Serving Zebra and Zallet](serving-zebra-and-zallet.md) for the integration audit. |
| **Zcash testnet faucet** | Server-side wallet | `zinder-client::RemoteChainIndex` + `chain_events` | Server-side wallet consumer for the testnet faucet. Uses the transparent-address surface plus the `chain_events` address-invalidation hint ([ADR-0021](../adrs/0021-canonical-confirmed-push-channel-for-transparent-activity.md)) to detect incoming deposits. |

## Reserved sections

The sections below describe integration paths that are supported but where the consumer set is not enumerated here. They are listed so new integrators understand the shape exists.

### Public-`lightwalletd`-client population

Operators running Zinder as a drop-in `lightwalletd` replacement expose `zinder-compat-lightwalletd` on the same port and protocol every existing public `lightwalletd` consumer expects. The Android SDK, ECC Wallet, and any other client that speaks the vendored lightwalletd protocol can integrate without code changes. See [Serving public lightwalletd clients](serving-public-lightwalletd-clients.md) for the operator gap analysis vs. community-run servers like `zec.rocks`.

### Block explorers

Block explorers that need read-only canonical chain state plus transparent-address indexing can integrate against the wallet data plane (`WalletQuery`) and the derive plane (`ExplorerQuery`). The derive plane is the right surface for explorer-shaped reads because it composes the canonical artifacts into explorer-friendly views without polluting the wallet data plane.

### Future Rust consumers

`zinder-client` is the canonical Rust integration crate. It exposes:

- `RemoteChainIndex`: connects to a `WalletQuery` gRPC endpoint.
- `LocalChainIndex`: opens a local RocksDB secondary directly when colocated with the writer.
- The `ChainIndex` trait: the canonical async surface for both shapes.

Any new Rust consumer that needs chain reads + broadcast should integrate through one of these. The trait is the same regardless of which adapter you pick, so an integration can start local and graduate to remote without code changes.

## Integration shape decision guide

If you are starting a new integration, pick the shape based on what you control:

| If you ... | Use |
| --- | --- |
| ... ship a mobile app | the Zashi/Zodl SDK (already integrated) |
| ... want a drop-in `lightwalletd` replacement | `zinder-compat-lightwalletd` |
| ... build a Rust server-side wallet | `zinder-client::RemoteChainIndex` + `zcash_client_backend` (see [Server-side wallet pattern](server-side-wallet-pattern.md)) |
| ... need transparent-only reads + broadcast (no shielded) | `zinder-client::RemoteChainIndex` directly |
| ... need explorer-shaped views | `WalletQuery` + `ExplorerQuery` (via the derive plane) |
| ... need a wallet RPC the operator can drive externally | Zallet alongside Zinder |
| ... need cross-language access | `WalletQuery` gRPC + the published OpenAPI / descriptor set ([ADR-0022](../adrs/0022-release-artifact-set.md)) |

## When NOT to be on this list

This page lists *known* consumers. There is no obligation to be listed; many operators run Zinder behind their own wallet stack without ever sharing the integration shape. The list exists to help new integrators find prior art, not to track adoption.

## References

- [ADR-0008: Consumer-neutral wallet data plane](../adrs/0008-consumer-neutral-wallet-data-plane.md)
- [Indexer/wallet boundary](../architecture/indexer-wallet-boundary.md)
- [Server-side wallet pattern](server-side-wallet-pattern.md)
- [Serving Zebra and Zallet](serving-zebra-and-zallet.md)
- [Serving public lightwalletd clients](serving-public-lightwalletd-clients.md)
- [Findings from Android wallet integration](android-wallet-integration-findings.md)
