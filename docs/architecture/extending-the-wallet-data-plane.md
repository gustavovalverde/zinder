# Extending the Wallet Data Plane

The wallet data plane exposes typed reads over canonical artifacts and live
writer-owned state. New methods belong here only when they answer a wallet
correctness question rather than a rebuildable product aggregate.

## Boundary rules

- The native contract is typed Rust domain values at `ChainIndex` and
  `WalletQueryApi`; protobuf is translated only at the gRPC boundary.
- Chain-state reads are bound to a `ChainEpoch`. A request without a pin first
  resolves one visible epoch; a response never mixes that epoch with a later
  tip or an upstream read.
- Mempool state is live and writer-owned. It is not epoch-pinnable and readers
  reach it through the ingest-control boundary instead of opening another
  upstream connection.
- Public methods use one explicit capability. Capability discovery determines
  whether an optional method is callable; missing facts remain unknown or
  unavailable, never fabricated as zero values.
- A response may enrich existing facts only from its pinned epoch or source
  event. It must not create a new storage family or silently widen the
  canonical contract.
- The lightwalletd adapter exposes only shapes present in its vendored
  protocol and translates through `LightwalletdQueryApi`.

## Choosing another boundary

Add a [canonical artifact](extending-artifacts.md) when the fact needs its own
identity, retention, or reorg semantics. Add a
[materialized view](materialized-view-plane.md) when it is a rebuildable
explorer or analytics aggregate. Follow [Public interfaces](public-interfaces.md)
for method names, cursor shapes, error vocabulary, and capability naming.
