# The Zcash Indexer as a Chain Data Plane

*A product-vision note for review. It argues that the Zcash ecosystem's indexing needs have outgrown the product scope Zaino was built for, sets out what a modern indexer should be, and points to a working, backward-compatible demonstration.*

A Zcash indexer should be a consumer-neutral chain data plane: one typed contract that serves wallets, block explorers, payment rails, and custody systems alike, reorg-safe by construction, and still backward-compatible with lightwalletd for the clients already deployed. Zaino was built for a narrower job, and at the time that was the right job. The work the ecosystem now needs done is larger than the scope Zaino was drawn around.

This note is written by the author of Zinder, a clean-slate indexer that demonstrates the data-plane shape. The argument is about the shape, not the repository. Zinder appears only as existence proof: the shape is buildable, and backward compatibility survives it. The disclosure at the end states plainly what that proof does and does not establish.

## The scope that fit the moment

Zaino was scoped to replace two aging services at once: lightwalletd's `CompactTxStreamer` gRPC for light clients, and a subset of zcashd's JSON-RPC for full-node wallets and explorers. Its README states the design center plainly: keep backward compatibility with lightwalletd and zcashd so existing clients switch with minimal changes. It reads chain state from Zebra, keeps indexing separate from validation, and ships as an embeddable Rust library that a full-node wallet can link in-process.

That was a sound response to zcashd's deprecation. The immediate need was continuity: get the mobile light wallets and the new full-node wallet onto a maintained Rust stack without forcing every client to relearn its data source. A compatibility shim with an embeddable library is exactly the instrument for that need, and Zaino executed it. It is mature, it carries a completed Zellic audit, it has a real contributor base, and it is embedded in Zallet today. None of what follows disputes any of that.

The point is narrower and it is about scope, not quality. A product scoped to "be the compatible replacement for the two services we are retiring" inherits the shape of the things it replaces. That shape was the mobile-light-wallet era's shape. The ecosystem has since grown other shapes.

---

The previous section described the job Zaino was built for. The next describes the jobs that did not exist when that scope was drawn.

## What the ecosystem outgrew

The consumer set for chain data has broadened past light wallets. A block explorer needs typed, aggregated views of blocks, transactions, fees, and value pools, plus a search surface that respects Zcash's privacy model. A payment rail settling native ZEC needs broadcast, chain-tip math, and confirmation tracking with typed outcomes. A faucet and a server-side custody system need reorg-safe wallet sync and transaction submission without running a wallet daemon. Agent-driven commerce needs all of these behind stable contracts. These products run on Zebra, alongside Zallet and the Z3 stack, and they share one trait: none of them is a light wallet, and several of them cannot be assembled from a light-wallet streamer.

What unites their requirements is a move up the stack, from "stream me compact blocks so I can scan" to "answer typed questions about chain state and tell me how fresh the answer is." Three needs recur across the set, and the compatibility-shim scope was never meant to carry them.

**Reorg safety as a contract, not a client chore.** Correct sync depends on a reorg-window-aware safe tip and a notification when the chain reorganizes. lightwalletd and Zaino expose the raw tip and leave clients to poll and diff to discover that history changed. Every consumer then reimplements the same fragile reconciliation.

**Local answers about transparent addresses.** Balance, history, and UTXO queries for a transparent address are first-class questions for explorers and payment flows. Zaino answers them by passing through to Zebra; its own local transparent-address index is off by default and finalized-state only. A round trip to the validator per query is a different performance and ownership posture than a local index.

**Analytics as a typed surface.** Block summaries, fee summaries, value-pool summaries, mempool activity, and privacy-aware search are the substance of an explorer. They are not expressible as `CompactTxStreamer` calls. A developer can assemble them client-side from zcashd JSON-RPC, but assembling them is precisely the act of building the missing layer.

---

The needs above describe a gap. The next section names the shape that fills it.

## The shape of a data plane

The recurring axis across every need is ownership: does the indexer own a typed, consistent, consumer-neutral answer, or does it forward a request and leave the consumer to finish the job? A compatibility shim sits at one end of that axis; a data plane sits at the other. The contrast is concrete rather than abstract, so it is worth stating dimension by dimension.

| Dimension | Compatibility-shim end | Data-plane end |
|---|---|---|
| Consumer model | One wallet's backend, plus a wire other clients tolerate | One typed contract many product types consume |
| Transparent-address truth | Forwarded to the validator per query | Owned in a local index, paginated and mempool-aware |
| Reorg handling | Client polls the tip and diffs to detect reorgs | Server pushes a typed, resumable reorg and commit stream |
| Analytics | Assembled client-side from raw RPCs | Served as typed block, fee, value-pool, and search RPCs |
| Negotiation | Implicit, discovered by trial or version | Explicit per-feature capabilities and a freshness stamp |
| Consumer coupling | Indexer embedded in the consumer process | Consumed across a typed gRPC or client-library seam |
| Privacy | The consumer's responsibility | A refusal built into the data plane itself |

Two of these rows carry most of the weight, because they are correctness and capability facts rather than matters of taste.

Reorg safety is the deeper of the two. A data plane that emits a typed reorg stream, with the reverted and committed ranges and a resumable cursor, lets every consumer recover the same correct way. A shim that exposes only the raw tip pushes that logic into each client, where it is written more than once and tested less than once. The difference shows up first in wallet sync and explorer freshness, then compounds as consumers multiply.

The analytics surface is the more visible of the two. An explorer is, in product terms, a set of typed aggregate queries with a privacy filter in front of them. When the indexer owns those queries, the explorer is a thin client. When it does not, the explorer team rebuilds the indexer's missing half on top of raw RPCs, including the privacy refusal that keeps shielded counterparties out of public views. The work does not disappear; it moves to the consumer and is duplicated across consumers.

---

The shape is a claim. The next section is the evidence that the shape is buildable and that one plane serves products of genuinely different kinds.

## One plane, many products

Zinder implements the data-plane end of every dimension above, and four products consume it through three different transports. The spread is the point: a wallet library, an explorer, a payment facilitator, and a faucet are not variations on one client, yet they share one data plane.

| Product | What it is | How it consumes the plane | Could the shim scope serve it as-is |
|---|---|---|---|
| `zally` | Headless server-side wallet library | Typed `ChainIndex` client over gRPC, behind pluggable chain-source and submitter seams | Partially; the read path ports, but the safe-tip and reorg-stream primitives have no shim equivalent |
| `zexplorer` | Zcash block explorer | Native analytics and wallet gRPC | No, without rebuilding the analytics, capability, and freshness layer |
| `zpay` | Native-ZEC payment facilitator (x402) | Typed `ChainIndex` client for broadcast, tip, and confirmation depth | Partially; the runtime calls map across, but it is coded to the typed client, not a config swap |
| `fauzec` | Testnet faucet with custody | Through the `zally` library | Yes, for its requirements; it uses only the light-wallet sync and broadcast subset |

The honest reading of this table is a split, not a sweep. The faucet, the payment facilitator, and most of the wallet library consume the light-wallet-and-broadcast subset the shim scope already covers. For them the compatibility thesis holds, and the data plane improves seams and reorg safety rather than being a precondition. The explorer is the product the shim scope cannot reach: most of what it calls is the analytics plane, and rebuilding that on raw RPCs is building the data plane by another name.

`zally` is the sharpest illustration, because it occupies the same role as Zaino's embeddable library and inverts the dependency. Zaino's library is the indexer, linked into the wallet process. Zally is a wallet that owns wallet logic and consumes chain data through a typed boundary, so the same wallet code runs against a colocated read-replica, a remote gRPC endpoint, or a test mock without noticing the difference. That inversion, the indexer as a plane the wallet reads rather than an engine the wallet hosts, is the product vision compressed into one design decision.

---

A new shape is only adoptable if it does not strand the clients already running. The next section addresses that directly.

## Compatibility without capture

The data-plane shape does not require abandoning lightwalletd. Zinder ships a discrete `CompactTxStreamer` adapter speaking the vendored v0.4.0 protocol, validated by a live parity suite against the reference lightwalletd. The native typed plane is the product; the lightwalletd wire is a translation layer over the same stored data. Backward compatibility is a coverage contract the plane satisfies, not a constraint the plane is shaped by.

That distinction sets the migration cost for the clients that exist today. The installed light clients, Zingolib and Zashi through its SDK, reach their indexer over the `CompactTxStreamer` wire. Moving them is an endpoint change with parity validation, not a rewrite.

The deepest integration is Zallet, which embeds Zaino as an in-process library rather than calling it over a wire. A compatibility shim cannot swap an in-process dependency, so that case deserves an honest accounting. At the call surface the coupling is shallow: Zallet uses roughly nine distinct indexer methods, and a typed client already covers nearly all of them. One real gap remains: a verbose mined-transaction read that returns serialized bytes. The data behind it is already stored, so closing the gap is one additional RPC, not a new index. The most useful fact is that Zallet's own stated direction is to consume an abstract chain-index trait over a chain snapshot, which is structurally what the typed client already is. The migration aligns with where Zallet says it wants to go.

---

The shape is buildable, demonstrated, and compatible. The remaining question is what the ecosystem should do with that.

## What this asks of the ecosystem

Maintaining the compatibility shim keeps the present working, and the wallets wired to it today need that maintenance during the zcashd-deprecation window. That investment funds continuity. It does not, on its own, move the architecture toward the data plane the broader product set now needs, because a maintenance scope reproduces the scope it maintains.

The ask is to fund the forward shape as deliberately as the present one, and to judge any indexer, including Zinder, against the data-plane dimensions rather than lightwalletd parity alone. Three moves carry the most leverage:

- Treat the typed reorg stream and the analytics surface as load-bearing requirements, not nice-to-haves.
- Land the single transaction-read RPC that unblocks a Zallet trial of the typed client.
- Make the case for the typed chain-index seam to the wallet team on the merits of their own roadmap.

## The limits of this argument

Two facts keep this honest. The product suite that demonstrates the shape is the work of a single author. That proves the data-plane contract is general enough to serve a wallet library, an explorer, a payment rail, and a faucet. It does not prove independent adoption. Generality and adoption are different claims, and only generality is evidenced here. The sustainability of a single-maintainer project is the real open question, and it is organizational, not architectural.

The second fact is that Zaino is good at the job it was scoped for. "Compatibility shim" is a description of scope, not a verdict on quality, and the wallet, custody, and payment products confirm that the shim surface genuinely suffices for them. The argument is that the ecosystem's job has grown past that scope, not that the scope was ever done badly.

A modern Zcash indexer is a chain data plane. The shape is buildable, it stays compatible with the clients already deployed, and it serves the products the ecosystem is actually building. That is the case for treating the data plane as the target and the compatibility shim as the floor.

---

*Reference implementation: [Zinder](https://github.com/gustavovalverde/zinder).*
