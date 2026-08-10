# Changelog

All notable changes to Zinder are documented in this file. Zinder releases one
lockstep product version across its first-party crates, services, runtime
images, and API artifacts.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [0.6.0] - 2026-08-10

### Changed

- Lightwalletd-compatible deployments now advertise transparent-address support only when their serving data can answer those requests. Startup rejects configurations that lack a transparent wallet projection or retained transaction bytes. ([#71](https://github.com/gustavovalverde/zinder/pull/71))
- Document the production Testnet Zebra readiness calibration that keeps ordinary block gaps available while peer, sync, lag, and stale-tip failures remain fail-closed. ([#73](https://github.com/gustavovalverde/zinder/pull/73))

### Fixed

- Stale fixed-view mempool epochs now reach the typed view-expiration path so wallet consumers can reacquire the view without treating expected chain rotation as service unavailability. ([#69](https://github.com/gustavovalverde/zinder/pull/69))
- Secondary query and projection processes no longer create thousands of short-lived storage threads per second while following the chain. ([#72](https://github.com/gustavovalverde/zinder/pull/72))


## [0.5.0] - 2026-07-29

### Added

- Expose network upgrade activations through the native query API. ([#16](https://github.com/gustavovalverde/zinder/pull/16))
- Add the optional Cipherscan-compatible explorer API adapter. ([#15](https://github.com/gustavovalverde/zinder/pull/15))
- Ship the native WalletQuery API alongside the lightwalletd compatibility service. ([#22](https://github.com/gustavovalverde/zinder/pull/22))
- Publish guarded amd64 and arm64 runtime images, API artifacts, and lockstep GitHub Releases from validated tags. ([#24](https://github.com/gustavovalverde/zinder/pull/24))
- Add borrowed and Arc-owned chain snapshots that pin canonical wallet reads to one chain epoch and return typed refresh guidance when that epoch is unavailable. ([#28](https://github.com/gustavovalverde/zinder/pull/28))
- Prepare `zinder-core`, `zinder-proto`, and `zinder-client` as a registry-ready Rust SDK catalog with hermetic protocol artifacts and standalone package verification. ([#30](https://github.com/gustavovalverde/zinder/pull/30))
- Publish `zinder-core`, `zinder-proto`, and `zinder-client` from tagged releases through guarded crates.io trusted publishing with resumable source-provenance verification. ([#31](https://github.com/gustavovalverde/zinder/pull/31))
- Publish deterministic x86-64-v3 and AArch64 GNU/Linux bundles containing the four supported Zinder runtime executables with checksums and build metadata. ([#32](https://github.com/gustavovalverde/zinder/pull/32))
- Publish keyless signatures, SLSA provenance, and SPDX 2.3 SBOMs for Zinder GNU/Linux bundles and release images. ([#33](https://github.com/gustavovalverde/zinder/pull/33))
- Run the canonical writer, the wallet projector, and the native WalletQuery reader as one supervised container against a single volume, for hosts that attach one volume per service. ([#60](https://github.com/gustavovalverde/zinder/pull/60))

### Changed

- Establish the fact-first RocksDB lifecycle with bounded catchup, projection workloads, and coherent checkpoint recovery. ([#17](https://github.com/gustavovalverde/zinder/pull/17))
- Cut wallet serving over to canonical facts, projection-aware readiness, and native operational controls. ([#18](https://github.com/gustavovalverde/zinder/pull/18))
- Replace transitional service and configuration names with the final ingest, projector, query, and compatibility vocabulary. ([#19](https://github.com/gustavovalverde/zinder/pull/19))
- Require mempool snapshots to match the visible chain tip, bound ingestion and retention work, and treat ordinary tip movement as synchronization instead of a source failure. ([#27](https://github.com/gustavovalverde/zinder/pull/27))
- Make `zinder-client` remote-first without RocksDB or storage dependencies, remove the unused storage-backed local adapter, and add typed network-upgrade discovery for chain-index consumers. ([#29](https://github.com/gustavovalverde/zinder/pull/29))
- Update the Ironwood dependency stack and report the NU6.3 mainnet activation height in the Cipherscan migration overview. ([#51](https://github.com/gustavovalverde/zinder/pull/51))
- Build the explorer materialized views inside the ingest runtime from canonical storage. The view store is built from genesis on first start, the `[storage.materialized_views]` RocksDB budget now also applies to `zinder-ingest`, and stores built with a checkpoint height do not host materialized views. ([#52](https://github.com/gustavovalverde/zinder/pull/52))
- Certify live regtest and lightwalletd parity behavior against Zebra 6.2.2, including its current RPC consistency and security fixes. ([#57](https://github.com/gustavovalverde/zinder/pull/57))
- Derive native WalletQuery capabilities from admitted storage and providers, fail invalid compositions before serving, and support the Regtest-certified source-built Zallet backend. Regtest JSON-RPC health treats a successful node tip observation as ready because the isolated chain has no public-network tip. Native contract revision 5 removes unauthenticated retention-duration fields from WalletServerInfo and replaces COMPACT_BLOCK_RANGE_TOO_LARGE with BLOCK_RANGE_TOO_LARGE; generated native clients must be regenerated. ([#67](https://github.com/gustavovalverde/zinder/pull/67))

### Fixed

- Preserve configured artifact-store contracts when canonical ingest opens or rebuilds storage. ([#20](https://github.com/gustavovalverde/zinder/pull/20))
- Keep the ingest operations endpoint available after a reorg exceeds the configured window so operators can inspect the drained state and rebuild or restore without a crash loop. ([#25](https://github.com/gustavovalverde/zinder/pull/25))
- Allow tagged release retries to find and resume an existing draft GitHub Release while still rejecting published releases and API failures. ([#35](https://github.com/gustavovalverde/zinder/pull/35))
- Restore post-publication SDK verification against a fresh crates.io-only consumer. ([#37](https://github.com/gustavovalverde/zinder/pull/37))
- Make release image publication wait for GHCR tag convergence and verify attestations with compatible strict identity constraints. ([#39](https://github.com/gustavovalverde/zinder/pull/39))
- Keep deployment admission aligned with the strict release image attestation verifier. ([#40](https://github.com/gustavovalverde/zinder/pull/40))
- Accept the compatibility image's declared native query dependency in release SBOM evidence. ([#41](https://github.com/gustavovalverde/zinder/pull/41))
- Verify release archives and stable image promotion with compatible attestation identity selectors. ([#43](https://github.com/gustavovalverde/zinder/pull/43))
- Check out the validated release commit before preparing draft GitHub Release assets. ([#45](https://github.com/gustavovalverde/zinder/pull/45))
- Bind GitHub Release publication to the validated repository when the publication job runs without a checkout. ([#47](https://github.com/gustavovalverde/zinder/pull/47))
- Correct six documented identifiers that no longer matched the shipped API, including the mempool cursor error, the subtree roots RPC, and the capability table name. ([#50](https://github.com/gustavovalverde/zinder/pull/50))
- Expose typed wallet-sync capabilities through zinder-client, preserve cold admission when a block hash is displaced repeatedly, and provide a current-contract ExplorerQuery overlay that serves transaction detail and transparent activity without opening the canonical store. ([#53](https://github.com/gustavovalverde/zinder/pull/53))
- Prevent the single-host ingest runtime from exhausting file descriptors while opening the canonical and Explorer materialized-view stores. ([#54](https://github.com/gustavovalverde/zinder/pull/54))
- Keep the native WalletQuery service responsive under concurrent range reads; syncing wallets no longer lose their streams, and readiness stays answerable while a wallet catches up. ([#60](https://github.com/gustavovalverde/zinder/pull/60))


## [0.4.0] - 2026-07-11

Zinder's release history before the changeset-style changelog is available in
the repository at the [v0.4.0 tag](https://github.com/gustavovalverde/zinder/tree/v0.4.0).
