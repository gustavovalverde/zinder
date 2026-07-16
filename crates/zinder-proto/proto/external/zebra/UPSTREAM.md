# Vendored Zebra indexer protocol

`indexer.proto` is vendored from [zcash/zebra](https://github.com/ZcashFoundation/zebra)
at the commit recorded in `COMMIT`. Zinder's source layer consumes the
`zebra.indexer.rpc.Indexer` service to receive streaming notifications and
fetch raw best-chain block bytes by height.

## Updating

1. Pick a Zebra commit that ships the `indexer.proto` shape Zinder requires.
2. Copy the file: `cp $ZEBRA/zebra-rpc/proto/indexer.proto crates/zinder-proto/proto/external/zebra/indexer.proto`
3. Record the commit: `echo $ZEBRA_SHA > crates/zinder-proto/proto/external/zebra/COMMIT`
4. Run the validation gate; the `vendored-proto` CI job diffs this directory
   against the recorded upstream commit.

## Boundary

Zinder's source adapters consume the generated `zebra.indexer.rpc.Indexer`
client. Mempool observations are hydrated through JSON-RPC before becoming
typed `MempoolSourceEvent` values. Historical block responses become typed
`SourceBlock` values only after their raw header and display-order response
hash agree. The adapters do not re-export Zebra's generated types across the
source boundary.
