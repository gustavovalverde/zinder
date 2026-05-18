# Explorer-only deployment

Status: Stable
Audience: operators standing up `zinder-explorer` against an existing
`zinder-query` deployment.

This runbook covers the topology a block-explorer consumer expects in
production: one `zinder-explorer` instance per region, federated to a
shared `zinder-query` endpoint. The ingest and canonical-storage layers
stay on a separate operational footprint (see
[zinder-ingest](./zinder-ingest.md) and
[zinder-query](./zinder-query.md)).

## Prerequisites

- A reachable `zinder-query` endpoint (gRPC) advertising:
  - `wallet.read.latest_block_v1`
  - `wallet.read.full_block_at_v1`
  - `wallet.read.transparent_prevouts_v1` (optional; enables paid-fee
    fields on explorer reads when present)
  - `wallet.snapshot.mempool_v1`
  - `wallet.address.transparent_history_v1`
  - `wallet.address.transparent_balance_v1`
  - `wallet.read.chain_value_pools_at_tip_v1`
- A storage path the explorer process can write to. The explorer
  derive store is independent from any canonical store; it can live on
  a separate filesystem.

## Required environment variables

| Variable | Required | Description |
| -------- | -------- | ----------- |
| `ZINDER_NETWORK` | Yes | `zcash-mainnet`, `zcash-testnet`, or `zcash-regtest`. Must match the `zinder-query` deployment. |
| `ZINDER_EXPLORER__STORAGE_PATH` | Yes | Filesystem path where the explorer derive store opens. |
| `ZINDER_EXPLORER__LISTEN_ADDR` | Yes | gRPC listen address (e.g. `0.0.0.0:9087`). |
| `ZINDER_EXPLORER__OPS_LISTEN_ADDR` | Yes | HTTP listen address for `/healthz`, `/readyz`, `/metrics`. |
| `ZINDER_EXPLORER__WALLET_QUERY_ENDPOINT` | Yes | gRPC URL of the upstream `zinder-query`. |
| `ZINDER_EXPLORER__BEARER_TOKEN_PATH` | Optional | Path to a file containing the shared-secret bearer token. When set, every inbound gRPC request must carry `authorization: Bearer <token>`. |

## Capability check

After the process starts, the easiest probe is `curl` against the ops
endpoint:

```bash
curl http://127.0.0.1:9088/healthz
```

Sample response:

```json
{
  "status": "alive",
  "service": "zinder-explorer",
  "version": "0.1.0",
  "network": "zcash-mainnet",
  "capabilities": [
    "explorer.server_info_v1",
    "explorer.transparent_address.balance_v1",
    "explorer.transaction.detail_v1",
    "explorer.search_v1",
    "explorer.mempool.summary_v1",
    "explorer.mempool.activity_v1",
    "explorer.transparent_address.activity_v1",
    "explorer.fee.summary_v1",
    "explorer.value_pool.summary_v1",
    "explorer.block.summary_v1",
    "explorer.block.detail_v1"
  ]
}
```

The `capabilities` array mirrors what `ExplorerQuery.ServerInfo`
advertises; dashboards and `curl` probes can branch on the array
without a gRPC round trip.

## Sample systemd unit

```ini
[Unit]
Description=zinder-explorer
After=network.target

[Service]
Type=simple
User=zinder
WorkingDirectory=/var/lib/zinder-explorer
ExecStart=/usr/local/bin/zinder-explorer
Environment=ZINDER_NETWORK=zcash-mainnet
Environment=ZINDER_EXPLORER__STORAGE_PATH=/var/lib/zinder-explorer/store
Environment=ZINDER_EXPLORER__LISTEN_ADDR=0.0.0.0:9087
Environment=ZINDER_EXPLORER__OPS_LISTEN_ADDR=0.0.0.0:9088
Environment=ZINDER_EXPLORER__WALLET_QUERY_ENDPOINT=http://10.1.0.10:9085
Environment=ZINDER_EXPLORER__BEARER_TOKEN_PATH=/etc/zinder/bearer-token
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

## Sample Docker Compose service

```yaml
services:
  zinder-explorer:
    image: zinder/zinder-explorer:latest
    restart: unless-stopped
    environment:
      ZINDER_NETWORK: zcash-mainnet
      ZINDER_EXPLORER__STORAGE_PATH: /var/lib/zinder-explorer/store
      ZINDER_EXPLORER__LISTEN_ADDR: 0.0.0.0:9087
      ZINDER_EXPLORER__OPS_LISTEN_ADDR: 0.0.0.0:9088
      ZINDER_EXPLORER__WALLET_QUERY_ENDPOINT: http://zinder-query:9085
      ZINDER_EXPLORER__BEARER_TOKEN_PATH: /run/secrets/bearer_token
    ports:
      - "9087:9087"
      - "9088:9088"
    volumes:
      - explorer_store:/var/lib/zinder-explorer/store
    secrets:
      - bearer_token

volumes:
  explorer_store: {}

secrets:
  bearer_token:
    file: ./bearer-token
```

## Operational signals

- `readyz` returns `200` only once the derive store is open and the
  explorer has connected to the upstream wallet plane. A `503` indicates
  startup is still in progress or an upstream connectivity failure.
- The Prometheus series `zinder_explorer_request_duration_seconds` plus
  `zinder_explorer_request_total` carry per-RPC duration and outcome
  labels (`operation`, `status`, `error_class`). Alert on p95 above 1 s
  for 10 minutes per operation.

## Catching up after a wallet-plane re-key

If the upstream `zinder-query` rotates its bearer token, restart the
explorer process so the cached channel re-handshakes. The derive store
survives restart; cursors are persisted alongside the materialized
records.
