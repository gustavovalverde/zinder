# Vendored Zebra indexer protocol

`indexer.proto` is vendored from [zcash/zebra](https://github.com/ZcashFoundation/zebra)
at the commit recorded in `COMMIT`. Zinder's source layer consumes the
`zebra.indexer.rpc.Indexer` service to receive streaming mempool change
notifications when an upstream Zebra is built with `--features indexer`.

## Updating

1. Pick a Zebra commit that ships the `indexer.proto` shape Zinder requires.
2. Copy the file: `cp $ZEBRA/zebra-rpc/proto/indexer.proto crates/zinder-proto/proto/external/zebra/indexer.proto`
3. Record the commit: `echo $ZEBRA_SHA > crates/zinder-proto/proto/external/zebra/COMMIT`
4. Run the validation gate; the `vendored-proto` CI job diffs this directory
   against the recorded upstream commit.

## Boundary

Zinder's source adapter (`crates/zinder-source/src/zebra_indexer_mempool.rs`)
consumes the generated `zebra.indexer.rpc.Indexer` client. The adapter
hydrates `MempoolChange::ADDED` observations with raw transaction bytes
from Zebra's JSON-RPC `getrawtransaction` endpoint before yielding a typed
`MempoolSourceEvent`. The adapter does not re-export Zebra's generated
types across the source boundary; the public `MempoolSourceEvent` carries
only Zinder-owned vocabulary.
