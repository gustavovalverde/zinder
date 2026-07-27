# ADR-0038: State portability and schema migration

| Field | Value |
| --- | --- |
| Status | Accepted |
| Product | Zinder |
| Domain | Recovery, deployment bootstrap, storage schema evolution |
| Related | [Canonical storage topologies](0035-canonical-storage-topologies.md), [Storage backend](../architecture/storage-backend.md), [Initial sync](../runbooks/initial-sync.md), [State portability](../runbooks/state-portability.md) |

## Context

Operators need to seed mainnet and testnet deployments without rebuilding the
same state on every host. Developers also need to move state across Zinder
releases whose physical RocksDB schemas intentionally refuse old layouts.
Treating both jobs as one mechanism would either weaken physical admission or
force recovery archives to retain version-specific transforms indefinitely.

## Decision

Zinder has two explicit portability artifacts.

The physical artifact is `zinder-recovery-archive` format 1. It contains one
coherent canonical and wallet checkpoint pair, the inner state-bundle
manifest, and an outer manifest that commits every payload path, length, and
SHA-256. Restore verifies every byte, cold-opens both stores, compares their
complete READY evidence with the manifest, and writes only to absent deployment
paths. A physical snapshot is reusable only by a binary that admits its exact
canonical and wallet schemas.

The logical artifact is `zinder-logical-state` format 1. It contains raw source
blocks from the network's height-zero predecessor through the captured tip,
the node-discovered network-upgrade table, shielded subtree roots, immutable
segment hashes, and the canonical fact-sequence digest. Export recaptures those
source bytes from Zebra and requires their tip and canonical digest to equal an
admitted physical snapshot fence. Import supplies the archive as a
`NodeSource` to the normal fresh canonical constructor. The destination derives
its current physical schema, and the projector rebuilds the wallet from
canonical events.

Format 1 logical export requires complete canonical history beginning at
height 1. Checkpointed-history migration remains unsupported until an artifact
can authenticate the full typed predecessor frontier without an old physical
decoder.

Both artifacts are fixed-layout directories, so each file maps directly to an
object-store key. `zinderctl snapshot pull` supports manifest-first streaming
from a static HTTPS prefix, including an R2 public custom domain. Upload uses
ordinary object-store tools such as `rclone` or an S3-compatible client; Zinder
does not embed a provider SDK or credential model. The outer manifest is
published last locally and should be uploaded last or with immutable object
semantics remotely.

Materialized views and mempool state are not portable correctness state.
Materialized views rebuild from canonical events, and the mempool rehydrates
from the node. There is no global Zinder schema version, in-place restore,
generic migration graph, or fallback decoder for old physical stores.

SHA-256 detects byte replacement only when the manifest itself is trusted.
Physical download and logical import therefore require a trusted exact
manifest digest. Distribution systems can additionally use signed provenance
and immutable object retention.

## Consequences

- Same-release bootstrap restores exact canonical and wallet state without
  relaxing normal admission.
- Cross-schema migration is deterministic because only source-domain bytes
  cross the version boundary.
- R2, S3-compatible storage, static HTTP servers, and removable media share one
  artifact layout.
- Logical export is substantially more expensive than packaging a physical
  checkpoint because it reads the complete retained chain from Zebra.
- A failed restore can leave an explicitly named `.incomplete` sibling, but
  final configured paths are never overwritten.

## Deferred work

- Authenticated checkpointed-history logical migration.
- Signed manifests and a release-owned transparency policy.
- Resumable HTTP ranges after measurements justify their added state machine.
- Provider-specific upload commands beyond documented external clients.
