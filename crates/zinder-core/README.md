# zinder-core

`zinder-core` contains Zinder's transport-independent chain identity, epoch,
artifact, transaction, and wallet-query domain values. It is the lowest layer
of the public Rust SDK and does not open storage or connect to a Zinder
deployment.

Most applications should depend on `zinder-client`, which re-exports the
consumer-facing domain values used by its typed query API. Depend on
`zinder-core` directly when implementing another transport or adapting the
domain model at a protocol boundary.

The optional `commitment-tree-codec` default feature enables the Sapling,
Orchard, and incremental-Merkle-tree codecs needed to decode tree-state
artifacts. Disable default features when only the base identity and query
types are required.
