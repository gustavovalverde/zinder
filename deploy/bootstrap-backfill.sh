#!/bin/sh
# Bootstrap the canonical store with a bulk `backfill` pass before tip-follow
# takes over.
#
# Tip-follow processes one block per poll cycle, which is fine for steady
# state but takes days on a multi-million-block backlog. Backfill batches up
# to `commit_batch_blocks` blocks per commit and drains the upstream in
# hours. Running this script once leaves the store close enough to tip that
# tip-follow can catch the rest in minutes.
#
# Re-runs are cheap: backfill exits `AlreadyComplete` when the requested
# range is covered, so it is safe to sequence in front of every `up`.
#
# Required env (provided by docker-compose.yml):
#   ZINDER_NODE__JSON_RPC_ADDR  Zebra JSON-RPC URL (e.g. http://zebra:18232)
#   ZINDER_NODE__AUTH__PATH     Cookie file path (e.g. /var/run/auth/.cookie)
#
# Optional env:
#   BACKFILL_TIP_OFFSET  Blocks back from Zebra's tip (default 100, matches
#                        reorg_window_blocks). The `ZINDER_` prefix is
#                        reserved for binary config; bootstrap knobs use
#                        their own namespace.

set -eu

cookie_path="${ZINDER_NODE__AUTH__PATH:-/var/run/auth/.cookie}"
rpc_url="${ZINDER_NODE__JSON_RPC_ADDR:-http://zebra:18232}"
offset="${BACKFILL_TIP_OFFSET:-100}"

if [ ! -r "$cookie_path" ]; then
    echo "bootstrap: cookie file $cookie_path not readable; skipping backfill" >&2
    exit 0
fi
cookie=$(cat "$cookie_path")

# Probe Zebra for its current tip height. getblockchaininfo returns
# {"result": {"blocks": N, ...}}; pull N out without taking a jq dependency.
response=$(curl -sS --connect-timeout 5 --max-time 30 \
    -u "$cookie" \
    -H 'Content-Type: application/json' \
    --data '{"jsonrpc":"1.0","id":"bootstrap","method":"getblockchaininfo","params":[]}' \
    "$rpc_url") || {
    echo "bootstrap: getblockchaininfo failed; skipping backfill" >&2
    exit 0
}

tip=$(printf '%s' "$response" | sed -n 's/.*"blocks":[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -1)
if [ -z "$tip" ]; then
    echo "bootstrap: could not parse blocks from getblockchaininfo response" >&2
    echo "$response" >&2
    exit 1
fi

target=$((tip - offset))
if [ "$target" -le 0 ]; then
    echo "bootstrap: zebra tip $tip below offset $offset; tip-follow will start from genesis" >&2
    exit 0
fi

echo "bootstrap: zebra tip=$tip; backfilling to=$target (offset=$offset blocks)"
exec /usr/local/bin/zinder-ingest backfill \
    --wallet-serving \
    --to-height "$target" \
    --config /etc/zinder/config.toml
