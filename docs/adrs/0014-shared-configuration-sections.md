# ADR-0014: Shared Configuration Sections

## Status

Accepted.

## Context

Zinder ships four service binaries (`zinder-ingest`, `zinder-query`,
`zinder-compat-lightwalletd`, `zinder-explorer`). Each consumes a layered
TOML + env + CLI configuration through the
[`zinder-runtime`](../../crates/zinder-runtime/src/config.rs)
`ConfigLoader`. Before this ADR, the binaries each owned a private copy
of the schema for the sections they shared: `[network]`, `[node]`,
`[storage]`, retention windows, and the IngestControl writer/reader
plumbing. The same field appeared with the same semantics in two to four
places with drift-prone duplication of struct shapes, default constants,
and validation logic.

Two production incidents traced back to this duplication:

- The 2026-05-15 operational endpoint footgun, where the four service
  Dockerfiles each baked an `ENV ZINDER_OPS_LISTEN_ADDR=…` (single
  underscore) that only one service consumed. The other three accepted
  the value into the container environment but never wired it through
  config-rs because they had no schema for it.
- Operator confusion across runbooks about whether retention windows
  belonged under `[ingest.retention]` (writer enforcement) or under
  `[storage]` (reader advertisement). The two paths were textual copies
  of the same field set, validated independently, with operators
  expected to keep them in sync.

The asymmetry meant that adding a fifth binary, or adding a new shared
field to an existing concern, required N copy-paste edits, each one a
chance for drift. The vocabulary spine at
[`docs/architecture/public-interfaces.md`](../architecture/public-interfaces.md)
had no canonical schema to point at because there was no single source
of truth.

## Decision

Every TOML section that more than one binary consumes lives in a single
shared module under
[`crates/zinder-runtime/src/sections/`](../../crates/zinder-runtime/src/sections),
with a public struct (`<Name>Section`), a resolver
(`resolve_<name>(...)`), an optional resolved-config projection
(`Resolved<Name>`), and a TOML mirror for `--print-config` rendering.
The corresponding [`ConfigLoader`](../../crates/zinder-runtime/src/config.rs)
helper (`with_<name>_section`) wires per-service defaults so a new
service cannot drift from the established schema.

The eight shared sections at the time of writing are:

- `[network]` ([`NetworkSection`](../../crates/zinder-runtime/src/config.rs))
- `[ops]` ([`OpsSection`](../../crates/zinder-runtime/src/sections/ops.rs)),
  uniform across all four binaries, on-by-default with empty-string
  opt-out
- `[storage]` ([`PrimaryStorageSection`](../../crates/zinder-runtime/src/sections/storage.rs)
  for the writer,
  [`SecondaryStorageSection`](../../crates/zinder-runtime/src/sections/storage.rs)
  for readers)
- `[retention]`
  ([`RetentionSection`](../../crates/zinder-runtime/src/sections/retention.rs)),
  enforced by `zinder-ingest` and advertised by `zinder-query`; single
  source of truth replaces the writer/reader duplicate
- `[ingest_control]`
  ([`IngestControlSection`](../../crates/zinder-runtime/src/sections/ingest_control.rs)),
  one section that carries writer-side `listen_addr`, reader-side `addr`,
  and shared `bearer_token_path` (ADR-0006); each binary reads only what
  it needs
- `[node]` and `[node.auth]` ([`NodeSection`](../../crates/zinder-source/src/node_target.rs),
  pre-existing in `zinder-source`)

Per-service sections (`[ingest]`, `[backup]`, `[query]`, `[compat]`,
`[explorer]`) stay private to their owning binary.
[ADR-0015](0015-unified-phase-driven-ingest.md) collapses the earlier
`[backfill]` and `[tip_follow]` writer-side splits into the
sub-sectioned `[ingest.phases]`, `[ingest.bulk_catchup]`,
`[ingest.tip_follow]`, and `[ingest.modifiers]` schema, and adds a
new shared `[node.health]` sub-section on the existing `[node]`
schema so the upstream-health knobs are operator-readable from every
binary that wants them.

### Defaults

Default ports for the operational endpoint and the gRPC listen addresses
live in
[`crates/zinder-runtime/src/sections/defaults.rs`](../../crates/zinder-runtime/src/sections/defaults.rs)
as `pub const` functions keyed on
[`ServiceIdentifier`](../../crates/zinder-runtime/src/sections/service.rs).
Per-section retention/catchup constants live in the section module that
consumes them; they are intentionally private because external callers
work through the resolver, which already applies the defaults.

### Env-var contract

The shared sections inherit the `ZINDER_<SECTION>__<FIELD>` env-var
convention from
[Public interfaces §Environment variable mapping](../architecture/public-interfaces.md#environment-variable-mapping).
The
[`ENVIRONMENT_VARIABLES`](../../crates/zinder-runtime/src/env_var_docs.rs)
constant is the single registry; the CI doc-mirror test fails when the
constant and the spine table diverge.

The
[`env_diagnostics`](../../crates/zinder-runtime/src/env_diagnostics.rs)
module intercepts serde "unknown field" errors at deserialization time,
maps the rejected key back to the originating `ZINDER_…` env var, and
emits
[`ConfigError::RejectedEnvVar`](../../crates/zinder-runtime/src/config.rs)
with a "did you mean" hint that points at the double-underscore form.
This catches the single-vs-double-underscore footgun that motivated the
refactor at startup with an actionable error, instead of silently
producing a meaningless top-level key that the schema rejects with a
generic message.

## Consequences

**For service authors.** Adding a new field to a shared section means
editing one struct + one resolver. The four binaries pick it up by
calling the existing `with_<name>_section` helper. Adding a new
binary means listing its `ServiceIdentifier` variant and chaining the
existing shared helpers; the schema is uniform by construction.

**For operators.** The canonical TOML at
[Public interfaces §Configuration Conventions](../architecture/public-interfaces.md#section-layout)
is the only place that describes the schema. `--print-config` against any
binary renders the same shared sections in the same shape; operator
scripts that pivot on section names see the same fields regardless of
which binary they target.

**For test authors.** Cross-field invariants on shared sections (e.g.,
"retention warning lead time must be \u{2264} the retention window")
live in the section's resolver and are exercised by the section's own
unit tests; the four service test suites no longer carry parallel
copies.

**Breaking changes.** Adopting this ADR renamed several operator-facing
keys; per the Zinder pre-release breaking-change policy there is no
compatibility shim:

| Old path                                | New path                            |
|------------------------------------------|-------------------------------------|
| `[ingest.control] listen_addr`           | `[ingest_control] listen_addr`      |
| `[ingest.control] token_path`            | `[ingest_control] bearer_token_path`|
| `[storage] ingest_control_addr`          | `[ingest_control] addr`             |
| `[storage] ingest_control_token_path`    | `[ingest_control] bearer_token_path`|
| `[ingest.retention] *`                   | `[retention] *`                     |
| `[storage] chain_event_retention_hours`  | `[retention] chain_event_retention_hours` (single source of truth replaces the reader-side mirror) |
| `[storage] mempool_mined_retention_minutes` | `[retention] mempool_mined_retention_minutes` |
| `[storage] mempool_invalidated_retention_hours` | `[retention] mempool_invalidated_retention_hours` |

The matching `ZINDER_…` env vars follow the same path; the env-var
table in
[Public interfaces](../architecture/public-interfaces.md#operator-facing-variables)
is the authoritative list.

## References

- [ADR-0003: Canonical Storage Access Boundary](0003-canonical-storage-access-boundary.md)
- [ADR-0004: Node Source and Protocol Boundaries](0004-node-source-and-protocol-boundaries.md)
- [ADR-0006: IngestControl Transport Security](0006-ingest-control-transport-security.md)
- [`crates/zinder-runtime/src/sections/`](../../crates/zinder-runtime/src/sections)
- [Public interfaces §Configuration Conventions](../architecture/public-interfaces.md#configuration-conventions)
