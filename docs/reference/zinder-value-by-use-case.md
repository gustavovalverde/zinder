# What Zinder Adds By Use Case

Zinder is a service-oriented indexer for Zcash applications. It reads validated chain data from a node, stores the public facts that products share, and exposes stable APIs for explorers, wallets, and payment services.

Zinder does not validate consensus and does not hold wallet keys. Use Zebra for full-node validation and node-owned indexed state. Use Zaino when an integration needs LightWallet or zcashd-compatible indexer behavior. Use Zinder when several services need the same public chain data at the same committed chain epoch, with explicit freshness, reorg, and feature-negotiation semantics.

## Layer Choice

| Surface | Primary role | Choose it when |
| --- | --- | --- |
| Zebra node and state indexes | Full node, consensus validation, peer networking, mempool, persistent chain state, UTXO data, and zcashd-compatible RPC reads. | The consumer needs validator-coupled data, raw node behavior, or indexed RPCs owned by the node. |
| Zebra indexer gRPC | Node-adjacent stream for chain-tip changes, non-finalized-state changes, mempool changes, and block fetches. | A trusted service needs to synchronize from Zebra's own state and non-finalized chain view. |
| Zaino | Compatibility indexer for LightWallet gRPC, selected zcashd-compatible RPCs, and node-backed chain fetch. | Existing clients expect LightWallet or zcashd-shaped APIs. |
| Zinder | Consumer-neutral chain data plane with epoch-pinned reads, chain events, explorer views, wallet sync data, and typed broadcast results. | Multiple products need consistent chain data without each one rebuilding its own indexer. |

## Zebra Indexed State

Zebra already indexes chain data as part of running a node. Its state stores the best chain, blocks, the UTXO set, and other indexes, and Zebra exposes useful indexed reads through RPC. That includes transparent address balances, address transaction IDs, address UTXOs, raw transaction lookup, tree state, subtree roots, mempool data, and block lookup by hash or height.

Those indexes are node-owned. They serve validation, node operation, compatibility, and node-adjacent synchronization. Zinder builds on that kind of verified source data, but it answers a different question: how should products consume the same chain facts safely and consistently?

## Feature And Index Matrix

This table compares public information surfaces, not private implementation details. It is grounded in Zebra's node RPCs and indexer gRPC service, Zaino's LightWallet and zcashd-compatible APIs, and Zinder's wallet, explorer, and capability services.

`Yes` means the project exposes the information as a current public surface. `Partial` means the data exists only through a raw, compatibility-shaped, deployment-gated, or lower-level surface. `No` means there is no current public surface for that information in that project.

| Information or index | Zebra node/indexer | Zaino | Zinder |
| --- | --- | --- | --- |
| Consensus validation and peer network | Yes: full node | No | No |
| Persistent best-chain state and UTXO set | Yes: node state | Partial: indexer cache and chain state | Partial: product facts, not validator state |
| Full block by hash or height | Yes: `getblock` | Yes: zcashd-compatible block fetch | Partial: `WalletQuery.FullBlock`, when blobs are retained |
| Compact blocks for light clients | No | Yes: `GetBlock`, `GetBlockRange` | Yes: `WalletQuery.CompactBlock`, lightwalletd compat |
| Block headers and block identity | Yes: `getblockheader`, `getblockhash` | Yes: zcashd-compatible block headers | Yes: `BlockHeaderBySelector`, `BlockIdBySelector` |
| Tree state and subtree roots | Yes: `z_gettreestate`, `z_getsubtreesbyindex` | Yes: `GetTreeState`, `GetSubtreeRoots` | Yes: `TreeStateAtHeight`, `SubtreeRoots` |
| Raw transaction by transaction id | Yes: `getrawtransaction` | Yes: `GetTransaction`, `getrawtransaction` | Partial: `WalletQuery.Transaction`, with raw bytes when retained |
| Transaction status by transaction id | Partial: raw lookup or mempool lookup | Partial: raw lookup or LightWallet result | Yes: mined, mempool, conflicting, or missing |
| Parsed public transaction facts | Partial: raw or verbose transaction data | Partial: raw or compact transaction data | Yes: `ExplorerQuery.TransactionDetail` |
| Paid fee and prevout resolution | Partial: infer from raw inputs and UTXOs | Partial: infer from raw inputs and UTXOs | Yes: transaction fee fields, when prevouts resolve |
| Transparent address balance | Yes: `getaddressbalance` | Yes: `GetTaddressBalance`, `getaddressbalance` | Yes: `TransparentAddressBalance` |
| Transparent address UTXOs | Yes: `getaddressutxos` | Yes: `GetAddressUtxos`, `getaddressutxos` | Yes: `TransparentAddressUnspentOutputs` |
| Transparent address transaction history | Yes: `getaddresstxids` | Yes: `getaddresstxids`, `GetTaddressTransactions` | Yes: `TransparentAddressTxIdsInRange`, address activity |
| Transparent address value deltas | No | Yes: `getaddressdeltas` | Yes: `TransparentAddressDeltas` |
| Transparent outpoint lookup | Partial: `gettxout` for unspent outputs | Partial: `gettxout` for unspent outputs | Yes: output, spend, and unspent-by-outpoint reads |
| Transparent UTXO-set summary | No | Yes: `gettxoutsetinfo` | Partial: count, total value, optional commitment |
| Value-pool summary | Yes: `getblockchaininfo` | Yes: `getblockchaininfo` | Yes: `ValuePoolSummary`, `ChainValuePoolsAtTip` |
| Mempool list and summary | Yes: `getrawmempool`, `getmempoolinfo` | Yes: `GetMempoolTx`, `GetMempoolStream`, mempool RPCs | Yes: `MempoolSnapshot`, `MempoolSummary` |
| Mempool event stream | Partial: indexer gRPC `MempoolChange` | Partial: mempool streams close on tip change | Yes: cursor-resumable `MempoolEvents` |
| Chain tip and non-finalized changes | Yes: indexer gRPC streams | Partial: state library and service internals | Partial: committed `ChainEvents`, not non-finalized reads |
| Reorg history for products | Partial: derive from node state changes | Partial: handled by indexer state | Yes: `ChainEvents`, `ChainReorgHistory` |
| Explorer block summaries and recent transactions | No | No | Yes: `BlockSummariesInRange`, `RecentTransactions` |
| Explorer search and privacy refusal | No | No | Yes: typed `Search` responses |
| Fee, mempool, and overview dashboards | No | No | Yes: `FeeSummary`, `MempoolSummary`, `OverviewSnapshot` |
| Payment disclosure verification | No | No | Yes: optional `VerifyPaymentDisclosure` |
| Feature discovery and freshness envelope | Partial: node RPC status | Partial: service and sync status | Yes: `ServerInfo`, capabilities, freshness |
| Transaction broadcast | Yes: `sendrawtransaction` | Yes: `SendTransaction`, `sendrawtransaction` | Yes: typed broadcast outcomes |

## Zaino And Zinder

Zaino and Zinder both sit above a node, but they optimize for different integration contracts. Zaino is compatibility-first. It provides a LightWallet `CompactTxStreamer` service, selected zcashd-compatible RPCs, compact-block data, transaction submission, and a Rust chain fetch library for services that run close to a node.

Zinder is designed around product APIs. It keeps the compatibility path available, but its main contract is a typed data plane shared by explorers, wallets, and payment services. That contract makes consistency visible through `ChainEpoch`, makes reorgs visible through `ChainEvents`, and lets clients negotiate features through capability discovery.

## Shared Guarantees

| Zinder guarantee | What clients get |
| --- | --- |
| Epoch-pinned reads | A request reads from one committed chain view instead of mixing data across competing tips. |
| Reorg-aware streams | Clients can resume from cursors and react to committed or reverted ranges. |
| Freshness envelopes | UIs and services can tell whether the node, ingest loop, projection, or query layer is stale. |
| Capability discovery | Clients can check which optional views are available before calling them. |
| Typed broadcast outcomes | Services can distinguish accepted, duplicate, queued, rejected, and unknown transaction states. |
| Shared projections | Explorers, wallets, and payment services can reuse the same derived chain facts. |

## Block Explorers

Zebra supplies validated blocks, transactions, mempool state, node status, and indexed address and tree-state RPCs. An explorer still needs page-ready answers: recent activity, parsed transaction facts, address views, search behavior, reorg context, and freshness. Zinder provides those answers as stable product APIs.

| Explorer need | Zinder provides |
| --- | --- |
| Recent activity | Block summaries, block detail, and recent transaction lists. |
| Transaction pages | Parsed public facts, component counts, privacy shape, fees when resolvable, V6 fields, and Ironwood action counts. |
| Fee and mempool pages | Fee summaries, mempool summaries, activity windows, and event counts. |
| Transparent address pages | Address activity, deltas, balances, UTXOs, and pagination-ready history. |
| Search | Typed results for blocks, transactions, transparent addresses, TEX addresses, and unified addresses, plus explicit privacy refusals. |
| Reorg context | Reorg history and chain-event streams that explain when a page or payment state moved backward. |
| Dashboard coherence | Overview snapshots and freshness data so one screen does not silently mix different tips. |

Zebra and Zaino overlap with some explorer needs through node-owned indexes, LightWallet methods, and zcashd-compatible methods. Zinder's added value is the explorer-shaped contract: the response already carries the projection, consistency, and freshness semantics an explorer needs to render safely.

## Wallets

Wallets still own keys, trial decryption, note state, witnesses, account balances, transaction construction, proving, signing, labels, and user policy. Zinder only owns the chain-data side of wallet sync.

| Wallet need | Zinder provides | Wallet still owns |
| --- | --- | --- |
| Shielded sync input | Compact blocks, tree state, and subtree roots. | Trial decryption, notes, witnesses, and balances. |
| Transparent wallet support | UTXOs, transaction history, transparent balances, and prevout resolution. | Address ownership, labels, and user notifications. |
| Broadcast | Typed transaction outcomes. | Construction, signing, proofs, and fee policy. |
| Reorg recovery | Chain events with committed and reverted ranges. | Wallet-specific rollbacks and rescans. |
| Compatibility | A lightwalletd-compatible service over the same store. | Wallet UX, seed handling, and local storage. |

Zaino is a good fit when the wallet expects LightWallet or zcashd-compatible APIs. Zinder is a good fit when the wallet wants typed errors, explicit feature negotiation, epoch-aware reads, and the same chain data contract used by other services.

## Payment Facilitators

Payment facilitators do not need to become wallets or full nodes. They need to prepare a payment, broadcast a wallet-signed transaction, confirm it, handle reorgs, and expose a clean lifecycle to their own callers.

| Facilitator need | Zinder provides |
| --- | --- |
| Expiry and confirmation math | Current tip, visible tip, settled tip, and chain-view data. |
| Broadcast handling | Typed outcomes that make duplicate broadcasts idempotent and rejected transactions actionable. |
| Confirmation tracking | Transaction lookup plus chain events. |
| Reorg handling | A way to move a payment back from confirmed to pending when the chain reverts. |
| Receipt verification | Transaction facts and optional payment-disclosure verification surfaces. |
| Service boundary | A private gRPC dependency that lets the facilitator expose its own public HTTP API. |

The difference from calling Zebra directly is lifecycle semantics. Zinder gives the facilitator chain data already shaped for expiry, broadcast, confirmation, settlement, and reorg correction.

## When Not To Use Zinder

Use Zebra directly for node operation, mining, tools that work directly with validator behavior, indexed node RPCs, and low-level RPC workflows.

Use Zaino when compatibility with LightWallet or zcashd-shaped APIs is the primary requirement.

Use a wallet library or wallet process for keys, viewing keys, account state, note detection, signing, labels, and user-specific policy.

Use Zinder when the answer is public chain data, the answer must be consistent for every caller at the same committed chain epoch, and more than one product can reuse it.

## References

- [What Zinder is and is not](../architecture/indexer-wallet-boundary.md)
- [Integration surfaces](integration-surfaces.md)
- [Server-side wallet pattern](server-side-wallet-pattern.md)
- [Explorer plane](../architecture/explorer-plane.md)
- [Wallet data plane](../architecture/wallet-data-plane.md)
