# `postgres-protocol` compatibility patch

This directory contains the published `postgres-protocol` 0.6.12 source from
the `postgres-protocol-v0.6.12` tag in `rust-postgres/rust-postgres`, commit
`76062c9b242da6aada065c021aa3083d0922f7d2`. The local package version is
`0.6.12+zinder.1`; the crates.io archive SHA-256 is
`08808e3c483c46e999108051c78334f473d5adb59d78bb80a1268c7e6aa6c514`.
Publishing is disabled. The vendored package also uses
its local README path and inherits the root workspace resolver. One crate-level
lint attribute allows the two repository-specific Clippy policies that the
unchanged upstream source predates (`disallowed_names` and
`too_many_arguments`); upstream identifiers and public APIs are not rewritten
to satisfy local style preferences. Its local MSRV is raised from upstream's
1.85 to the workspace MSRV, 1.95, so the workspace and Clippy contracts remain
aligned. Cargo Machete metadata records its known false positives for the
optional `getrandom` feature and the `md-5` package's `md5` crate name.

This third-party package intentionally does not declare
`[lints] workspace = true`: upstream contains justified internal invariant
`expect`/`unwrap`/`panic` calls and upstream tests use the same idioms. Rewriting
those unrelated lines would make the security patch harder to audit. The full
workspace Clippy command still compiles this crate with warnings denied; the
crate-level attribute names the only two local style-lint exceptions needed for
unchanged upstream identifiers and API shape.

## Local delta

The protocol implementation is unchanged. The compatibility patch changes
only these RustCrypto dependencies and the now-unnecessary `KeyInit` imports:

- `hmac` 0.13 to 0.12
- `md-5` 0.11 to 0.10
- `sha2` 0.11 to 0.10

Zinder currently resolves `bip32` 0.6.0-pre.1 through its Zcash dependencies.
That release pins prerelease RustCrypto 0.13 packages, and Cargo cannot resolve
them beside the semver-compatible stable packages requested by the unmodified
`postgres-protocol` 0.6.12 manifest. The compatibility patch leaves
`tokio-postgres` 0.7.18 unchanged and retains all protocol and security fixes
from `postgres-protocol` 0.6.12.

## Removal and update

Remove this patch and the root `[patch.crates-io]` entry as soon as the Zcash
dependency graph moves off the conflicting RustCrypto prereleases. When
updating `tokio-postgres`, first test the unmodified matching
`postgres-protocol` release. If a patch is still required, replace this whole
directory from the new upstream tag, reapply the smallest reviewed dependency
delta, and update the commit and local version recorded here.
`scripts/check-postgres-protocol-patch.sh` fails once the currently conflicting
`bip32` prerelease leaves the workspace graph so this bridge cannot silently
become permanent. It also verifies `ZINDER-SHA256SUMS`, a reviewed manifest of
every vendored source, manifest, license, and metadata file. Any vendor edit
therefore fails CI until its exact diff and replacement digest are reviewed.

Run at least:

```console
cargo test -p postgres-protocol --all-features
cargo check -p zinder-bench --all-targets
cargo test -p zinder-bench
ZINDER_TEST_POSTGRES_DATABASE_URL='postgresql://...' \
  cargo nextest run -p zinder-bench --profile=ci-postgres --run-ignored=all
cargo deny check
scripts/check-postgres-protocol-patch.sh
```

The first command runs the upstream SCRAM, password, hstore, codec, and type
tests against the patched dependency graph. The `zinder-bench` integration
test additionally exercises authenticated connection setup, binary `COPY`,
transaction commit, reconnect, and complete persisted read-back when
`ZINDER_TEST_POSTGRES_DATABASE_URL` points to a fresh disposable database. The
test creates its own one-block fixture. The Compose benchmark command uses
`ZINDER_BENCH_POSTGRES_DATABASE_URL`.
