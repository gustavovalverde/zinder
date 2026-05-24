# ADR-0010: Transaction Public Facts As The Shared Parser Output

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Transaction parsing, explorer wire surface, ingest pipeline, mempool pipeline |
| Related | [ADR-0008](0008-network-parameter-discovery.md), [ADR-0009](0009-explorer-plane-as-product-surface.md), [Explorer plane](../architecture/explorer-plane.md), [Wallet data plane](../architecture/wallet-data-plane.md) |

## Context

Zinder needs one transaction parser output that produces the public facts
required by wallet status, explorer transaction detail, fee projection,
privacy-shape classification, and transparent-address activity.

The fact-first store materializes parsed transaction facts during ingest and
stores them in the canonical `transaction_facts` table.

## Current Decision

`TransactionPublicFacts` remains the shared parsed shape. It is constructed from
source transaction bytes by `zinder-source`, then committed as part of
`TransactionFactsArtifact` alongside `TransactionLocation`.

Read paths consume `transaction_facts` and `transaction_location` directly.
Raw transaction bytes live only in the optional `transaction_blob` table when
the deployment opts into raw blobs.

## Consequences

- The parser remains the single owner of transaction version, lock time, expiry
  height, consensus branch ID, auth digest, component counts, privacy shape, and
  unsupported-section classification.
- Explorer and wallet reads do not parse raw transaction bytes on the normal
  path.
- Fee computation still depends on transparent-output resolution for input-side
  values.
- Adding a new public fact is a schema and API change, not a hidden read-time
  parser change.
