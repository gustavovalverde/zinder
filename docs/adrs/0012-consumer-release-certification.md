# ADR-0012: Consumer-Release Certification Tier

| Field | Value |
| ----- | ----- |
| Status | Accepted (2026-05-10) |
| Product | Zinder |
| Domain | Test infrastructure, release engineering, consumer-side parity |
| Related | [ADR-0006: Test tiers and unified live-test config](0006-test-tiers-and-live-config.md), [ADR-0008: Consumer-neutral wallet data plane](0008-consumer-neutral-wallet-data-plane.md), [Public interfaces](../architecture/public-interfaces.md) |

## Context

[ADR-0006](0006-test-tiers-and-live-config.md) defines four nextest profiles: `default` (T0/T1 unit + integration), `ci` (canonical CI gate), `ci-perf` (smoke perf budgets), and `ci-live` (T3 network-touching tests). Each tier answers a different question:

- `default` and `ci`: does the code compile and behave correctly in isolation?
- `ci-perf`: does the read path stay inside loose CI budgets?
- `ci-live`: does the indexer agree with a real upstream node on regtest, testnet, and mainnet?

None of those tiers answers the consumer-facing question: **does Zinder serve consumer X without a parity regression versus the prior-art Zaino surface?**

The four named consumers ([ADR-0008](0008-consumer-neutral-wallet-data-plane.md)) each exercise a different public contract:

- **Zashi / Zodl** (mobile, via `zcash-android-wallet-sdk`): exercises `zinder-compat-lightwalletd::CompactTxStreamer`.
- **Zallet** (desktop, future Rust consumer of `zinder-client::ChainIndex`).
- **Public lightwalletd operators** (`zec.rocks`-style): exercise the lightwalletd compat surface from `lightwalletd-go testclient` and Go-SDK callers.
- **Block explorers**: exercise the typed `WalletQuery` surface plus the federated `derive.explorer.*` methods.

[Android wallet integration findings](../reference/android-wallet-integration-findings.md) is the existing observational evidence for one consumer; it is refresh-on-test cadence, not an automated CI gate. The local/remote `*_parity.rs` integration tests at `crates/zinder-client/tests/integration/transparent_address_*_parity.rs` assert `LocalChainIndex` against `RemoteChainIndex`, not Zinder against Zaino.

The release-engineering gap: a release candidate cannot be certified as "no parity regression for consumer X" without a harness that exercises X-shaped requests against a Zinder build and asserts the response shape. This ADR adds that harness as a new test tier.

## Decision

`zinder` adds a fifth nextest profile, `ci-parity`, defined in `.config/nextest.toml`. The profile runs T2 integration tests scoped to consumer-facing surfaces and asserts the typed shape each closed gap row promises.

### Profile shape

`ci-parity` mirrors `ci-live`'s structural conventions (longer per-test timeouts, `--run-ignored=all` semantics where needed) but does not require a live upstream node. The minimum-viable parity tier exercises closed gap surfaces against `StoreFixture` synthetic data, which is reproducible across CI workers without external infrastructure.

### Certification scope per consumer

Each consumer gets a per-consumer test module under `crates/zinder-client/tests/parity/`:

- `parity/zashi.rs` — exercises lightwalletd-compat surfaces Zashi hits today (`GetBlockRange`, `GetCompactBlock`, `GetSubtreeRoots`, `GetTreeState`, `GetMempoolTx`, `GetLightdInfo`). Asserts the typed shape and the proto field-for-field contract.
- `parity/zallet.rs` — exercises `ChainIndex` methods Zallet's planned migration depends on (`block_id_by_selector`, `transaction_by_id`, `tree_state_at`, `chain_events`, `subtree_roots_in_range`). Asserts the typed `IndexerError` variants downstream consumers can match against.
- `parity/lightwalletd_operators.rs` — exercises lightwalletd compat surfaces public operators expose (`GetLightdInfo` non-empty fields, `GetAddressUtxos`, `GetTaddressTxids`, `GetTaddressBalance`).
- `parity/explorers.rs` — exercises native `WalletQuery` and federated `derive.explorer.*` surfaces typed for explorer use cases (`TransparentAddressBalance`, `BlockHeaderBySelector`).

Each module asserts shape only, not behavioral equivalence with Zaino. "Parity" here means "the typed Zinder method returns the consumer-expected shape", not "Zinder bytewise matches Zaino at the wire." The latter standard is unattainable when Zinder deliberately refuses Zaino's anti-patterns (see [Extending the wallet data plane §Anti-patterns to refuse](../architecture/extending-the-wallet-data-plane.md#anti-patterns-to-refuse)).

### Failure semantics

`ci-parity` is a release gate, not a per-PR gate.

- The CI matrix runs `ci-parity` on PRs targeting `main` (advisory) and on the release-tag pipeline (blocking).
- Per-PR failure does not block merge by default; the release-tag pipeline does. This avoids paying the parity-tier latency on every PR while still gating ship-readiness.
- Operators inspecting a release notice can read the parity report to confirm "Zinder serves consumer X."

### Boundary against ADR-0006 tiers

| Tier | Scope | Network needed | Per-PR? |
|---|---|---|---|
| `default` | T0/T1 unit + integration | No | Yes |
| `ci` | T0/T1/T2 against fixtures | No | Yes |
| `ci-perf` | Range-read latency budgets | No | Yes |
| `ci-live` | T3 against regtest / testnet / mainnet | Yes | Manual / scheduled |
| `ci-parity` (new) | Consumer-shaped requests against fixtures | No | Release-tag pipeline |

`ci-parity` uses the same `StoreFixture` infrastructure as `ci`; the new dimension is the test-module organization (per-consumer) and the assertion target (consumer-shaped requests).

## Consequences

### Operational

- Release notice generators read the latest `ci-parity` report and quote "all four consumers passing." Failures block release-tag publication.
- Per-consumer test ownership is explicit: each `parity/<consumer>.rs` module names the consumer in the file path. Failures route to the consumer's owning team in the issue tracker.
- The parity tier does not add CI cost on per-PR runs; only the release-tag pipeline pays it. CI minutes stay bounded.

### Implementation

- The scaffold lives in `crates/zinder-client/tests/parity/` with one module per named consumer.
- Each new public surface that a consumer depends on appends an assertion to the relevant per-consumer module.
- The `assert_wallet_chain_index_methods_compile` pattern from `crates/zinder-client/tests/integration/capability_coverage.rs` is mirrored per consumer for compile-time enforcement of trait surface presence.

### Testing

- The parity tier itself is exercised by running `cargo nextest run --profile=ci-parity` from the workspace root. CI invokes it on the release-tag pipeline.
- Test functions follow the `snake_case_describing_behavior` convention; the consumer scope is encoded in the file path (`parity/zashi.rs`), never in the function name.
- Parity tests do not depend on `ZINDER_TEST_LIVE`; they run in any environment that can build the workspace.

## Alternatives Considered

### Run real consumer SDKs (Zashi, Zallet, lightwalletd-go testclient) against a Zinder build

A higher-fidelity option: spin up Zinder, point a real Zashi build at it, scan a regtest range, assert success. Rejected for v1 of the parity tier because:

- Each consumer SDK has its own toolchain (Android Gradle for Zashi, Cargo for Zallet, Go for lightwalletd-testclient), inflating CI image complexity.
- SDK versioning drifts independently of Zinder; pinning the consumer SDK rev becomes its own maintenance burden.
- The shape assertion v1 catches the most likely regressions (renamed methods, removed capabilities, changed wire shapes) without that overhead.

A v2 of the parity tier may add `ci-parity-live` for full consumer-SDK runs, sequenced after the tier shape stabilizes.

### Reuse `ci` for parity by adding consumer-shaped assertions

Adding parity assertions inline in `ci` couples release-gate concerns with per-PR runtime. Per-PR runs would slow down on assertions that catch a different class of regression than per-PR tests are meant to catch. Separation by tier is consistent with [ADR-0006](0006-test-tiers-and-live-config.md)'s lifecycle rule.

### Build a Zaino reference recording and assert byte-equivalence

Recording a Zaino response stream and asserting Zinder-vs-Zaino byte-equivalence would be a strong claim. It is unworkable because Zinder deliberately refuses several Zaino shapes (see [Extending the wallet data plane §Anti-patterns to refuse](../architecture/extending-the-wallet-data-plane.md#anti-patterns-to-refuse)). The "no parity regression" claim is shaped against the consumer's expectations, not against Zaino's specific bytes.

## Out of Scope

- **Live consumer SDK runs.** Deferred to a future `ci-parity-live` profile; v1 stays at fixture-based shape assertions.
- **Cross-version parity (Zinder vN vs. vN-1).** A separate concern; the existing `wallet.events.chain_v1` style capability versioning carries the cross-version contract, not this tier.
- **Performance parity.** `ci-perf` already covers per-method latency budgets; `ci-parity` does not duplicate them.
- **Mainnet certification.** Mainnet T3 lives in `ci-live` per ADR-0006; `ci-parity` does not require mainnet access.

## ADR sequencing

Builds on [ADR-0006](0006-test-tiers-and-live-config.md). The fifth tier is additive; ADR-0006's existing four tiers are unchanged. [ADR-0008](0008-consumer-neutral-wallet-data-plane.md) defines the four named consumers this tier certifies against.
