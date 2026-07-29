# State portability

Use `zinderctl snapshot` for a fast, exact-schema deployment seed. Use
`zinderctl migrate` when the destination release does not admit the snapshot's
physical schemas. Every filesystem argument below is absolute, and every
output candidate or deployment path must be absent.

## Create and verify a physical snapshot

The running ingest and projector processes must share the checkpoint staging
root, and projector owner control must be enabled.

```bash
candidate="testnet-$(date -u +%Y%m%d-%H%M%S)"
mkdir -p /var/backups/zinder /var/lib/zinder/recovery-validation

zinderctl snapshot create \
  --network zcash-testnet \
  --projector-control-addr http://127.0.0.1:9111 \
  --projector-control-token-path /etc/zinder/projector-control.token \
  --staging-root /var/lib/zinder/checkpoint-staging \
  --archive-root /var/backups/zinder \
  --candidate "$candidate"

zinderctl snapshot verify \
  --network zcash-testnet \
  --archive-root /var/backups/zinder \
  --candidate "$candidate" \
  --validation-staging /var/lib/zinder/recovery-validation
```

The portable directory contains `canonical.rocksdb/`, `wallet.rocksdb/`,
`state-bundle.json`, and `recovery-archive.json`. Verification scans the
complete wallet projection and can take materially longer than byte admission.
For mainnet, use `zcash-mainnet` and a separate `mainnet/` object prefix; an
archive is always bound to exactly one network.

## Upload to Cloudflare R2

Preserve the candidate directory as one object-key prefix:

```bash
rclone copy \
  "/var/backups/zinder/$candidate" \
  "r2:zinder-state/testnet/$candidate" \
  --checksum \
  --exclude recovery-archive.json

rclone copyto \
  "/var/backups/zinder/$candidate/recovery-archive.json" \
  "r2:zinder-state/testnet/$candidate/recovery-archive.json" \
  --checksum

sha256sum "/var/backups/zinder/$candidate/recovery-archive.json"
```

Enable object versioning or retention when R2 is a recovery authority. Record
the manifest SHA-256 in a trusted deployment manifest; payload hashes do not
authenticate a replaced outer manifest.

The S3-compatible form is:

```bash
aws s3 sync \
  "/var/backups/zinder/$candidate" \
  "s3://zinder-state/testnet/$candidate" \
  --endpoint-url "https://${CLOUDFLARE_ACCOUNT_ID}.r2.cloudflarestorage.com" \
  --exclude recovery-archive.json

aws s3 cp \
  "/var/backups/zinder/$candidate/recovery-archive.json" \
  "s3://zinder-state/testnet/$candidate/recovery-archive.json" \
  --endpoint-url "https://${CLOUDFLARE_ACCOUNT_ID}.r2.cloudflarestorage.com"
```

Credentials remain in the external client's configuration. Zinder never reads
them.

## Download and restore

The URL names a public HTTPS candidate prefix. For production R2 distribution,
use a [custom domain](https://developers.cloudflare.com/r2/buckets/public-buckets/);
Cloudflare's managed `r2.dev` URL is rate-limited and intended for development.
A single S3-compatible presigned object URL is not a prefix and cannot authorize
the other objects in the archive.

```bash
mkdir -p /var/backups/zinder-downloads

zinderctl snapshot pull \
  --url "https://state.example.net/testnet/$candidate" \
  --archive-root /var/backups/zinder-downloads \
  --expected-manifest-sha256 "$trusted_manifest_sha256"
```

Stop ingest, projector, query, and compatibility processes, then restore to
fresh paths:

```bash
zinderctl snapshot restore \
  --network zcash-testnet \
  --archive-root /var/backups/zinder-downloads \
  --candidate "$candidate" \
  --canonical-target /var/lib/zinder/canonical-restored \
  --wallet-target /var/lib/zinder/wallet-restored \
  --validation-staging /var/lib/zinder/recovery-validation
```

Point ingest and projector at the returned paths, then start writer owners
before serving processes. Normal startup admission remains authoritative.
Materialized views rebuild from canonical history, and mempool state rehydrates
from Zebra.

If a copy fails, remove only the explicitly named
`.<target-name>.zinder-restore-<candidate>.incomplete` sibling after
investigating the filesystem error. Never replace an existing configured store
in place.

## Cross-schema migration

Logical export requires a complete-history physical snapshot and a Zebra node
on the same chain. Run export with the source-release `zinderctl` that admits
the physical snapshot:

```bash
zinderctl migrate export \
  --snapshot-root /var/backups/zinder \
  --snapshot-candidate "$candidate" \
  --network zcash-testnet \
  --node-json-rpc-addr http://127.0.0.1:8232 \
  --node-cookie-path /run/zebra/rpc.cookie \
  --output "/var/backups/zinder/${candidate}-logical"

zinderctl migrate plan \
  --archive "/var/backups/zinder/${candidate}-logical"
```

Export recaptures height 0 through the snapshot tip and refuses publication
unless its canonical digest matches the snapshot fence. Upload the logical
directory as one object prefix; `migration-state.json` is written after all
immutable segments.

```bash
logical_manifest_sha256="$(
  sha256sum "/var/backups/zinder/${candidate}-logical/migration-state.json" |
    awk '{print $1}'
)"

rclone copy \
  "/var/backups/zinder/${candidate}-logical" \
  "r2:zinder-state/testnet/${candidate}-logical" \
  --checksum \
  --exclude migration-state.json

rclone copyto \
  "/var/backups/zinder/${candidate}-logical/migration-state.json" \
  "r2:zinder-state/testnet/${candidate}-logical/migration-state.json" \
  --checksum

mkdir -p /var/backups/zinder-logical-downloads
rclone copy \
  "r2:zinder-state/testnet/${candidate}-logical" \
  "/var/backups/zinder-logical-downloads/${candidate}-logical" \
  --checksum
```

On the destination release, use its `zinderctl` so the normal constructor
writes the destination schema:

```bash
zinderctl migrate import \
  --archive "/var/backups/zinder-logical-downloads/${candidate}-logical" \
  --expected-manifest-sha256 "$logical_manifest_sha256" \
  --canonical-target /var/lib/zinder/canonical-migrated \
  --reorg-window-blocks 100
```

Import uses the destination binary's canonical constructor and compares the
published fence with the archive. Start `zinder-projector` against the migrated
canonical path and an absent wallet path to build the current wallet schema.
After projector convergence, start query and compatibility services.

A failed logical export or import leaves a
`.<target-name>.zinder-migration-export.incomplete` or
`.<target-name>.zinder-migration-import.incomplete` sibling. The requested
final path is published only after the archive fence or rebuilt canonical fence
verifies.
