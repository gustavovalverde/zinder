# ADR-0017: Compute-At-Read-Time at the Compat Boundary

| Field | Value |
| ----- | ----- |
| Status | Accepted (2026-05-12) |
| Product | Zinder |
| Domain | Compatibility-shim read-path and capability layering |
| Related | [Wallet data plane](../architecture/wallet-data-plane.md), [Derive plane](../architecture/derive-plane.md), [Public interfaces](../architecture/public-interfaces.md), [Protocol boundary](../architecture/protocol-boundary.md), [Lessons from Zaino](../reference/lessons-from-zaino.md), [ADR-0011](0011-derive-plane-federation-pattern.md), [ADR-0013](0013-derive-plane-instantiation-and-transparent-address-balance.md), [ADR-0014](0014-compute-at-read-time-canonical-reads.md), [ADR-0016](0016-wire-conventions-and-zebra-alignment.md) |

## Context

Two surfaces answer the same conceptual query, "the confirmed transparent-address balance summed across an address set":

- Lightwalletd-compat `GetTaddressBalance` (`cash.z.wallet.sdk.rpc.CompactTxStreamer`). Used by every lightwalletd-compatible wallet: Zashi, Zodl, librustzcash-based wallets, and any future client that scans through the vendored upstream proto.
- Native `WalletQuery.TransparentAddressBalance` (`zinder.v1.wallet`). Used by Zinder-native wallets and the in-process local client.

The derive plane's `ExplorerQuery.TransparentAddressBalance` computes a rich response: it sums canonical UTXOs *and* applies a live mempool overlay (pending inflows minus pending outflows). Routing both surfaces through the derive plane would couple every deployment to `zinder-derive`. That coupling is wrong on the compat surface: legacy lightwalletd clients do not consult capabilities, so `UNAVAILABLE` from `GetTaddressBalance` reads as a broken wallet, not a negotiable signal. A capability-gated `UNAVAILABLE` is correct on the native surface and wrong on the compat surface in the same deployment.

[ADR-0014](0014-compute-at-read-time-canonical-reads.md) named the compute-at-read-time pattern for canonical reads: a deterministic function over a bounded number of canonical artifacts, no new column family, no aggregation of unbounded inputs. Transparent-address confirmed balance fits the pattern exactly: the input is `TransparentAddressUtxo` artifacts that ingest already writes; the per-request address cap (256, matching the federated derive cap and the prevout cap from [`MAX_TRANSPARENT_PREVOUTS_PER_REQUEST`](../../crates/zinder-core/src/transparent_prevout.rs)) keeps the fan-out bounded; the response binds to one `ChainEpoch`.

The question this ADR resolves is which plane owns which version of the balance, and how the capability surface communicates the difference to clients that *do* gate.

## Decision

Confirmed transparent-address balance is computed at read time from canonical UTXOs on both surfaces. The optional derive plane adds the mempool overlay on the native surface and is the sole source of `unconfirmed_delta_zat`; the compat surface always reports confirmed-only because the legacy proto carries no overlay field. Capability layering communicates the split to clients.

### Method ownership

- **`WalletQuery.TransparentAddressBalance`** (native): always answers. With the derive plane configured and ready, the call is federated and the response carries the mempool overlay in `unconfirmed_delta_zat`. Without the derive plane (or with the derive proxy not ready), the native adapter computes `confirmed_zat` directly from canonical UTXOs and reports `unconfirmed_delta_zat = 0`. The helper lives at `zinder_query::transparent_address_confirmed_balance_response`; it accepts the same `TransparentAddressBalanceRequest` shape, enforces the per-request cap, and pins the read to a single chain epoch.
- **Compat `GetTaddressBalance` and `GetTaddressBalanceStream`** (lightwalletd-shaped): always answers from canonical UTXOs. The legacy `Balance { value_zat: int64 }` shape carries no overlay slot. Clients that want the overlay must speak the native `WalletQuery.TransparentAddressBalance`. The compat shim calls `transparent_address_confirmed_balance_response` directly through its `WalletQueryApi` handle; there is no proxy hop and no derive dependency.
- **`ExplorerQuery.TransparentAddressBalance`** (derive plane): unchanged. Continues to own the rich confirmed+mempool response used by federation and consumed by the native adapter when ready.

### Capability layering

Two coexisting capability strings advertise the same native RPC under different semantics:

- `wallet.address.transparent_balance_v1`: always present. Signals that `WalletQuery.TransparentAddressBalance` is answerable from canonical UTXOs alone (confirmed totals, no overlay).
- `derive.explorer.transparent_balance_v1`: present when the derive plane is configured and ready. Signals that the same response additionally carries the live mempool overlay in `unconfirmed_delta_zat`.

Clients gate features accordingly:

- A wallet that only needs confirmed totals checks for `wallet.address.transparent_balance_v1` (or simply calls the RPC, which always answers).
- A wallet that depends on the mempool overlay checks for `derive.explorer.transparent_balance_v1` and degrades gracefully if absent: it treats the `unconfirmed_delta_zat` field on the response as authoritative only when the derive capability is advertised.

The compat shim has no capability surface to layer; `LightdInfo` predates capability discovery. The shim's behavior is single-shape and always answers.

### Per-request bounds

The native helper enforces `MAX_TRANSPARENT_ADDRESSES_PER_BALANCE_REQUEST = 256`, identical to the federated derive cap. Each address scans canonical UTXOs through `WalletQueryApi::transparent_address_utxos` with cursor pagination; the response is pinned to one `ChainEpoch` so paginated drains are reproducible. The `confirmed_zat` accumulator saturates at `u64::MAX` in keeping with the rest of the wallet read surface; Zcash's hardcoded `MAX_MONEY` fits comfortably inside `u64::MAX` so saturation only triggers on upstream data corruption.

### Promotion path

If telemetry ever justifies it, `wallet.address.transparent_balance_v1` may promote from Shape C (compute-at-read-time) to Shape A (a dedicated balance column family) without changing the public contract. The capability string and the response shape are independent of the storage shape, per [ADR-0014 §Storage-shape ladder](0014-compute-at-read-time-canonical-reads.md#storage-shape-ladder). Promotion is deferred until a workload proves the parse-and-sum path cannot meet the latency budget.

## Alternatives considered

### Keep `UNAVAILABLE` and document the requirement

Rejected. Legacy lightwalletd clients do not honor capability gating; `UNAVAILABLE` from `GetTaddressBalance` is observed as a broken wallet, not a negotiable signal. Every Zashi or Zodl deployment that pointed at a Zinder shim without `zinder-derive` would silently fail one of the wallet's most visible reads.

### Compute confirmed balance on the derive plane only and have native fall back to UTXO retrieval at the wallet

Rejected. Pushing the aggregation to the wallet doubles round trips and forces every wallet to implement the per-request cap and the pinning logic. The server is the right place; the operational cost of the canonical-confirmed path is single-digit milliseconds at the cap.

### Add an `unconfirmed_delta_zat` field to the lightwalletd proto

Rejected. The vendored upstream proto is pinned to the `lightwallet-protocol` commit (surfaced as `LIGHTWALLETD_PROTOCOL_COMMIT`). Forking the schema is a non-starter for compatibility. Wallets that need the overlay should use the native API.

### Make derive mandatory when serving `GetTaddressBalance`

Rejected. The point of the derive plane is to stay optional ([ADR-0013](0013-derive-plane-instantiation-and-transparent-address-balance.md)). Making one compat method derive-mandatory would re-coupling the planes for a feature the legacy surface cannot even express.

## Consequences

- A Zinder deployment without `zinder-derive` answers `GetTaddressBalance` correctly. The parity harness exercises this path against `electriccoinco/lightwalletd:latest`.
- `WalletQuery.TransparentAddressBalance` is not derive-gated. `ServerInfo` advertises `wallet.address.transparent_balance_v1` unconditionally and `derive.explorer.transparent_balance_v1` only when the proxy is configured and ready. Clients that need the overlay gate on the derive capability; clients that need confirmed totals rely on the wallet capability.
- The native helper `transparent_address_confirmed_balance_response` is a public part of `zinder-query` and reusable by future compat surfaces or alternate adapters.
- The compat shim does not take a `DeriveProxy` argument. The derive plane is not a compat-shim dependency on any path.
- The structural test `crates/zinder-proto/tests/integration/capability_string_uniqueness.rs` ([ADR-0016](0016-wire-conventions-and-zebra-alignment.md)) enforces that the new capability string is imported, not duplicated.

## Forward compatibility

- Adding mempool overlay support to a future native `WalletQuery.*Balance` family of methods reuses the capability split: `wallet.address.*_v1` for the canonical path, `derive.explorer.*_v1` for the federated rich path.
- Future compat-side reads whose lightwalletd-go equivalent computes from upstream node calls (and where Zinder has the corresponding canonical artifacts) may follow the same pattern: compute at read time on the shim, advertise no capability change because lightwalletd has no capability discovery.
- A future schema bump for `TransparentAddressBalanceRequest` (richer filters, range bounds) does not need to disturb the compat surface; the shim translates the bounded legacy input into the richer native request, and the native helper consumes both shapes.
