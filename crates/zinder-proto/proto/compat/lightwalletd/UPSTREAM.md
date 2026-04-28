# lightwalletd Protocol Provenance

The vendored `compact_formats.proto` and `service.proto` files in this
directory are pinned to the upstream content below, with trailing whitespace
normalized for the repository hygiene gate:

- Repository: `https://github.com/zcash/lightwallet-protocol`
- Commit: `dd0ea2c3c5827a433e62c2f936b89efa2dec5a9a`
- Source paths:
  - `walletrpc/compact_formats.proto`
  - `walletrpc/service.proto`
- Verified on: 2026-04-27

Use the adjacent `COMMIT` file as the machine-readable source of this pin when
deciding whether a future upstream protocol change is an intentional Zinder
compatibility update or unreviewed drift.

Verification:

```bash
diff -u \
  <(perl -pe 's/[ \t]+$//' crates/zinder-proto/proto/compat/lightwalletd/compact_formats.proto) \
  <(curl -fsSL https://raw.githubusercontent.com/zcash/lightwallet-protocol/dd0ea2c3c5827a433e62c2f936b89efa2dec5a9a/walletrpc/compact_formats.proto | perl -pe 's/[ \t]+$//')

diff -u \
  <(perl -pe 's/[ \t]+$//' crates/zinder-proto/proto/compat/lightwalletd/service.proto) \
  <(curl -fsSL https://raw.githubusercontent.com/zcash/lightwallet-protocol/dd0ea2c3c5827a433e62c2f936b89efa2dec5a9a/walletrpc/service.proto | perl -pe 's/[ \t]+$//')
```
