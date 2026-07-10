# What Zinder Adds By Use Case

Zinder is a service-oriented indexer for Zcash applications. It reads validated chain data from a node, stores the public facts that products share, and exposes stable APIs for explorers, wallets, and payment services.

Zinder does not validate consensus and does not hold wallet keys. Use Zebra for full-node validation and node-owned indexed state. Use lightwalletd when an integration needs the reference LightWallet gRPC server exactly as existing clients expect it. Use Zaino when an integration needs a faster reimplementation of that same protocol plus selected zcashd-compatible RPCs. Use Zinder when several services need the same public chain data at the same committed chain epoch, with explicit freshness, reorg, and feature-negotiation semantics.

## Layer Choice

| Surface | Primary role | Choose it when |
| --- | --- | --- |
| Zebra node and state indexes | Full node, consensus validation, peer networking, mempool, persistent chain state, UTXO data, and zcashd-compatible RPC reads. | The consumer needs validator-coupled data, raw node behavior, or indexed RPCs owned by the node. |
| Zebra indexer gRPC | Node-adjacent stream for chain-tip changes, non-finalized-state changes, mempool changes, and block fetches. | A trusted service needs to synchronize from Zebra's own state and non-finalized chain view. |
| lightwalletd | Reference Go implementation of the LightWallet gRPC service (`CompactTxStreamer`), adapting a single zcashd or Zebra JSON-RPC backend for mobile wallet clients. | An integration needs the unmodified reference server that other LightWallet-compatible services are measured against. |
| Zaino | Compatibility indexer for LightWallet gRPC, selected zcashd-compatible RPCs, and node-backed chain fetch. | Existing clients expect LightWallet or zcashd-shaped APIs, and the deployment wants a Rust reimplementation of that surface. |
| Zinder | Consumer-neutral chain data plane with epoch-pinned reads, chain events, explorer views, wallet sync data, and typed broadcast results. | Multiple products need consistent chain data without each one rebuilding its own indexer. |

## Zebra Indexed State

Zebra already indexes chain data as part of running a node. Its state stores the best chain, blocks, the UTXO set, and other indexes, and Zebra exposes useful indexed reads through RPC. That includes transparent address balances, address transaction IDs, address UTXOs, raw transaction lookup, tree state, subtree roots, mempool data, and block lookup by hash or height.

Those indexes are node-owned. They serve validation, node operation, compatibility, and node-adjacent synchronization. Zinder builds on that kind of verified source data, but it answers a different question: how should products consume the same chain facts safely and consistently?

## Feature And Index Matrix

This table compares public information surfaces, not private implementation details. It is grounded in Zebra's node RPCs and indexer gRPC service, reference lightwalletd's `CompactTxStreamer` service, Zaino's LightWallet and zcashd-compatible APIs, and Zinder's wallet, explorer, and capability services.

`Yes` means the project exposes the information as a current public surface. `Partial` means the data exists only through a raw, compatibility-shaped, deployment-gated, or lower-level surface. `No` means there is no current public surface for that information in that project.

| Information or index | Zebra node/indexer | lightwalletd | Zaino | Zinder |
| --- | --- | --- | --- | --- |
| Consensus validation and peer network | Yes: full node | No | No | No |
| Persistent best-chain state and UTXO set | Yes: node state | Partial: local compact-block cache only, no UTXO set | Partial: indexer cache and chain state | Partial: product facts, not validator state |
| Full block by hash or height | Yes: `getblock` | No: full raw transaction lookup only, no full raw block RPC | Yes: zcashd-compatible block fetch | Partial: `WalletQuery.FullBlock`, when blobs are retained |
| Compact blocks for light clients | No | Yes: `GetBlock`, `GetBlockRange` | Yes: `GetBlock`, `GetBlockRange` | Yes: `WalletQuery.CompactBlock`, lightwalletd compat |
| Block headers and block identity | Yes: `getblockheader`, `getblockhash` | Partial: height and hash only, via `BlockID`, no dedicated header RPC | Yes: zcashd-compatible block headers | Yes: `BlockHeaderBySelector`, `BlockIdBySelector` |
| Tree state and subtree roots | Yes: `z_gettreestate`, `z_getsubtreesbyindex` | Yes: `GetTreeState`, `GetSubtreeRoots` (Sapling and Orchard only) | Yes: `GetTreeState`, `GetSubtreeRoots` | Yes: `TreeStateAtHeight`, `SubtreeRoots` |
| Raw transaction by transaction id | Yes: `getrawtransaction` | Yes: `GetTransaction` | Yes: `GetTransaction`, `getrawtransaction` | Partial: `WalletQuery.Transaction`, with raw bytes when retained |
| Transaction status by transaction id | Partial: raw lookup or mempool lookup | Partial: height-sentinel values on `RawTransaction`, no explicit status field | Partial: raw lookup or LightWallet result | Yes: mined, mempool, conflicting, or missing |
| Parsed public transaction facts | Partial: raw or verbose transaction data | No: raw or compact transaction bytes only, client parses | Partial: raw or compact transaction data | Yes: `ExplorerQuery.TransactionDetail` |
| Paid fee and prevout resolution | Partial: infer from raw inputs and UTXOs | Partial: infer from raw inputs and UTXOs | Partial: infer from raw inputs and UTXOs | Yes: transaction fee fields, when prevouts resolve |
| Transparent address balance | Yes: `getaddressbalance` | Yes: `GetTaddressBalance`, `GetTaddressBalanceStream` | Yes: `GetTaddressBalance`, `getaddressbalance` | Yes: `TransparentAddressBalance` |
| Transparent address UTXOs | Yes: `getaddressutxos` | Yes: `GetAddressUtxos`, `GetAddressUtxosStream` | Yes: `GetAddressUtxos`, `getaddressutxos` | Yes: `TransparentAddressUnspentOutputs` |
| Transparent address transaction history | Yes: `getaddresstxids` | Yes: `GetTaddressTxids` (deprecated), `GetTaddressTransactions` | Yes: `getaddresstxids`, `GetTaddressTransactions` | Yes: `TransparentAddressTxIdsInRange`, address activity |
| Transparent address value deltas | No | No | Yes: `getaddressdeltas` | Yes: `TransparentAddressDeltas` |
| Transparent outpoint lookup | Partial: `gettxout` for unspent outputs | No | Partial: `gettxout` for unspent outputs | Yes: output, spend, and unspent-by-outpoint reads |
| Transparent UTXO-set summary | No | No | Yes: `gettxoutsetinfo` | Partial: count, total value, optional commitment |
| Value-pool summary | Yes: `getblockchaininfo` | No | Yes: `getblockchaininfo` | Yes: source-backed `ValuePoolSummary`, `ChainValuePoolsAtTip` |
| Mempool list and summary | Yes: `getrawmempool`, `getmempoolinfo` | Partial: `GetMempoolTx` snapshot stream, no count or size summary RPC | Yes: `GetMempoolTx`, `GetMempoolStream`, mempool RPCs | Yes: `MempoolSnapshot`, `MempoolSummary` |
| Mempool event stream | Partial: indexer gRPC `MempoolChange` | Partial: `GetMempoolStream` closes on tip change | Partial: mempool streams close on tip change | Yes: cursor-resumable `MempoolEvents` |
| Chain tip and non-finalized changes | Yes: indexer gRPC streams | Partial: unary `GetLatestBlock` poll only, no push stream | Partial: state library and service internals | Partial: committed `ChainEvents`, not non-finalized reads |
| Reorg history for products | Partial: derive from node state changes | No | Partial: handled by indexer state | Yes: `ChainEvents`, `ChainReorgHistory` |
| Explorer block summaries and recent transactions | No | No | No | Yes: `BlockSummariesInRange`, `RecentTransactions` |
| Explorer search and privacy refusal | No | No | No | Yes: typed `Search` responses |
| Fee, mempool, and overview dashboards | No | No | No | Yes: `FeeSummary`, `MempoolSummary`, `OverviewSnapshot` |
| Payment disclosure verification | No | No | No | No: `VerifyPaymentDisclosure` is reserved but has no bundled verifier |
| Feature discovery and freshness envelope | Partial: node RPC status | Partial: `GetLightdInfo` version and height fields, no capability negotiation | Partial: service and sync status | Yes: `ServerInfo`, capabilities, freshness |
| Transaction broadcast | Yes: `sendrawtransaction` | Yes: `SendTransaction` | Yes: `SendTransaction`, `sendrawtransaction` | Yes: typed broadcast outcomes |

## Lightwalletd, Zaino, And Zinder

lightwalletd, Zaino, and Zinder all sit above a node, but they optimize for different integration contracts.

lightwalletd is the reference implementation of the LightWallet gRPC protocol. It streams compact blocks, tree state, and transparent-address data from a single zcashd or Zebra JSON-RPC backend, and existing mobile wallet clients were built against its exact behavior.

Zaino is compatibility-first. It reimplements that same `CompactTxStreamer` surface in Rust, and adds selected zcashd-compatible RPCs, compact-block data, transaction submission, and a Rust chain fetch library for services that run close to a node.

Zinder is designed around product APIs. It keeps the compatibility path available through a `CompactTxStreamer` adapter, but its main contract is a typed data plane shared by explorers, wallets, and payment services. That contract makes consistency visible through `ChainEpoch`, makes reorgs visible through `ChainEvents`, and lets clients negotiate features through capability discovery.

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

Zebra, lightwalletd, and Zaino overlap with some explorer needs through node-owned indexes, LightWallet methods, and zcashd-compatible methods. Zinder's added value is the explorer-shaped contract: the response already carries the projection, consistency, and freshness semantics an explorer needs to render safely.

## Wallets

Wallets still own keys, trial decryption, note state, witnesses, account balances, transaction construction, proving, signing, labels, and user policy. Zinder only owns the chain-data side of wallet sync.

| Wallet need | Zinder provides | Wallet still owns |
| --- | --- | --- |
| Shielded sync input | Compact blocks, tree state, and subtree roots. | Trial decryption, notes, witnesses, and balances. |
| Transparent wallet support | UTXOs, transaction history, transparent balances, and prevout resolution. | Address ownership, labels, and user notifications. |
| Broadcast | Typed transaction outcomes. | Construction, signing, proofs, and fee policy. |
| Reorg recovery | Chain events with committed and reverted ranges. | Wallet-specific rollbacks and rescans. |
| Compatibility | A lightwalletd protocol adapter over the same store. | Wallet UX, seed handling, and local storage. |

A wallet that expects the LightWallet protocol is not limited to lightwalletd or Zaino: Zinder's `zinder-compat-lightwalletd` adapter serves the same `CompactTxStreamer` surface over its own store. Reach for lightwalletd specifically when the wallet needs the unmodified reference server as its compatibility baseline, or for Zaino when an exact raw zcashd JSON-RPC method is a hard dependency. Zinder provides native semantic equivalents for `getaddressdeltas` (`ExplorerQuery.TransparentAddressDeltas`) and `gettxoutsetinfo` (`WalletQuery.TransparentUtxoSetSummary`); neither preserves the zcashd method name or raw response shape. The UTXO summary deliberately omits zcashd's format-dependent serialized-set hash and byte size. Zinder is the better fit when the wallet wants that same protocol plus typed errors, explicit feature negotiation, epoch-aware reads, and the same chain data contract used by other services.

## Payment Facilitators

Payment facilitators do not need to become wallets or full nodes. They need to prepare a payment, broadcast a wallet-signed transaction, confirm it, handle reorgs, and expose a clean lifecycle to their own callers.

| Facilitator need | Zinder provides |
| --- | --- |
| Expiry and confirmation math | Current tip, visible tip, settled tip, and chain-view data. |
| Broadcast handling | Typed outcomes that make duplicate broadcasts idempotent and rejected transactions actionable. |
| Confirmation tracking | Transaction lookup plus chain events. |
| Reorg handling | A way to move a payment back from confirmed to pending when the chain reverts. |
| Receipt verification | Transaction facts; payment-disclosure verification is not currently provided. |
| Service boundary | A private gRPC dependency that lets the facilitator expose its own public HTTP API. |

The difference from calling Zebra directly is lifecycle semantics. Zinder gives the facilitator chain data already shaped for expiry, broadcast, confirmation, settlement, and reorg correction.

## When Not To Use Zinder

Use Zebra directly for node operation, mining, tools that work directly with validator behavior, indexed node RPCs, and low-level RPC workflows.

Use lightwalletd directly when an integration wants the unmodified reference LightWallet gRPC server as its compatibility baseline. Use Zaino when a Rust reimplementation of that same protocol is preferred, or when a raw zcashd JSON-RPC method is needed directly. Zinder's typed address-delta and UTXO-summary reads are semantic equivalents, not zcashd RPC compatibility shims.

Use a wallet library or wallet process for keys, viewing keys, account state, note detection, signing, labels, and user-specific policy.

Use Zinder when the answer is public chain data, the answer must be consistent for every caller at the same committed chain epoch, and more than one product can reuse it.

## References

- [What Zinder is and is not](../architecture/indexer-wallet-boundary.md)
- [Integration surfaces](integration-surfaces.md)
- [Server-side wallet pattern](server-side-wallet-pattern.md)
- [Explorer plane](../architecture/explorer-plane.md)
- [Wallet data plane](../architecture/wallet-data-plane.md)
