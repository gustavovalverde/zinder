# Deploying Zinder on a VM

This runbook gets a fresh Zinder deployment onto a single Linux VM in roughly 30 minutes. Target shape: Docker Compose orchestrating `zinder-ingest` + `zinder-query`, behind systemd, against a Zebra deployment running on the same VM or a peer VM.

Steps below assume a Debian-family VM with Docker Engine 24+ installed. Adapt package commands for your distribution.

## Prerequisites

- Linux VM (4 vCPU, 8 GB RAM, 500 GB disk minimum for testnet; mainnet sizing larger).
- Docker Engine 24+ and Docker Compose v2 (`docker compose version`).
- A reachable Zebra deployment exposing JSON-RPC. The Zebra container's RPC port is reachable from the Zinder VM.
- The Zebra cookie file (`~/.cache/zebra/<network>/auth.cookie`), or the cookie content as a string, or a static `username:password`.
- Outbound HTTPS for image pulls from `ghcr.io`.

## Topology

```
┌────────────────────────────────────────────────────────────┐
│ VM                                                         │
│                                                            │
│ ┌────────────────────┐    ┌─────────────────────────────┐  │
│ │   systemd          │    │   Docker Compose            │  │
│ │   zinder.service   │──▶ │   zinder-ingest + zinder-   │  │
│ │                    │    │   query (per-service images) │  │
│ └────────────────────┘    └───┬─────────────────────────┘  │
│                                │ /var/lib/docker/volumes/   │
│                                │   zinder-data/             │
│                                ▼                            │
│                            (canonical store + secondary)    │
│                                                            │
│ ┌────────────────────────────────────────────────────────┐ │
│ │ Operator-supplied reverse proxy (Caddy / Nginx)        │ │
│ │ - TLS termination · rate limit · auth                  │ │
│ │ - public :443 → container :9101 (WalletQuery gRPC)     │ │
│ └────────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────┘
        ▲                                          ▲
        │ public consumers                         │ Zebra RPC
        │                                          │ (loopback or peer VM)
        ▼                                          ▼
   wallets / faucets / SDKs                    Zebra deployment
```

Zinder does not own TLS termination, authentication, or rate limiting; those are out of scope for v1. The reverse-proxy layer is operator-supplied; it sits between the public consumers and the container's plaintext gRPC port.

## Steps

### 1. Pull the deployment assets

```bash
sudo install -d -o root -g root -m 0755 /etc/zinder /etc/zinder/config
sudo curl -fsSLo /etc/zinder/docker-compose.yml \
    https://raw.githubusercontent.com/zcashfoundation/zinder/main/deploy/docker-compose.yml
sudo curl -fsSLo /etc/zinder/config/ingest.toml \
    https://raw.githubusercontent.com/zcashfoundation/zinder/main/deploy/single-container/config.example.ingest.toml
sudo curl -fsSLo /etc/zinder/config/query.toml \
    https://raw.githubusercontent.com/zcashfoundation/zinder/main/deploy/single-container/config.example.query.toml
```

Each Zinder binary strict-parses its own TOML schema (writer and reader fields do not share a section set), so the single-container image mounts two configs. Adjust the `[node]`, `[storage]`, and per-service blocks (`[ingest]` / `[tip_follow]` for `ingest.toml`, `[query]` for `query.toml`) to match your VM. Each example config documents every field.

### 2. Wire credentials via env vars

Edit `/etc/zinder/env` (or set in the systemd unit's `Environment=`):

```bash
sudo install -m 0600 -o root -g root /dev/stdin /etc/zinder/env <<'EOF'
ZINDER_NETWORK__NAME=zcash-testnet
ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:18232
ZINDER_NODE__AUTH__METHOD=cookie
ZINDER_NODE__AUTH__COOKIE=user:cookie-secret-here
EOF
```

The cookie value can be supplied as inline content via `ZINDER_NODE__AUTH__COOKIE` (per [ADR-0018](../adrs/0018-environment-variable-secret-policy.md)) or as a path via `ZINDER_NODE__AUTH__PATH=/path/to/.cookie`. Pick one; never both.

If you prefer basic auth:

```bash
ZINDER_NODE__AUTH__METHOD=basic
ZINDER_NODE__AUTH__USERNAME=zebra
ZINDER_NODE__AUTH__PASSWORD=...
```

`--print-config` will redact every secret regardless of how it was injected.

### 3. Install the systemd unit

```bash
sudo curl -fsSLo /etc/systemd/system/zinder.service \
    https://raw.githubusercontent.com/zcashfoundation/zinder/main/deploy/systemd/zinder.service
sudo systemctl daemon-reload
sudo systemctl enable --now zinder
```

The unit runs `docker compose up` with `Restart=on-failure` and a sane rate-limit envelope (5 restarts per 10 minutes). It tears down the Compose topology on stop so subsequent restarts begin from a clean state.

### 4. Verify startup phases

```bash
sudo journalctl -u zinder.service -f
```

You should see structured tracing events with `phase=` and `phase_state=`:

```
... phase=load_config phase_state=entry "startup phase entered"
... phase=load_config phase_state=exit outcome=ok elapsed_ms=42
... phase=open_storage phase_state=entry "startup phase entered"
... phase=open_storage phase_state=exit outcome=ok elapsed_ms=183
... phase=connect_node phase_state=entry "startup phase entered"
... phase=connect_node phase_state=exit outcome=ok elapsed_ms=512
... phase=check_schema phase_state=entry "startup phase entered"
... phase=check_schema phase_state=exit outcome=ok elapsed_ms=27
... phase=ready phase_state=entry
... phase=ready phase_state=exit outcome=ok elapsed_ms=0
```

The expected readiness cause sequence is `starting` → `syncing` → `ready`. While catching up, `/readyz` returns 503 with `{"status":"not_ready","cause":{"syncing":{"lag_blocks":N}}}`. Once caught up, it returns 200 with `{"status":"ready","cause":"ready"}`.

### 5. Hit the probes

```bash
# Liveness:
curl -fsS http://localhost:9106/readyz | jq .

# Metrics (Prometheus scrape):
curl -fsS http://localhost:9106/metrics | head

# gRPC server reflection (when enabled in config):
grpcurl -plaintext localhost:9101 list
```

### 6. Front Zinder with a reverse proxy

Zinder serves plaintext gRPC. Terminate TLS and apply auth/rate-limit at the proxy. Example Caddy config:

```caddy
zinder.example.org {
    reverse_proxy {
        to localhost:9101
        transport http {
            versions h2c
        }
    }
}
```

Adapt for Nginx, Cloudflare, or whichever proxy you operate. Verify the proxy is HTTP/2-capable; gRPC requires it.

### 7. Rollback

If a new release misbehaves:

```bash
sudo systemctl stop zinder
sudo sed -i 's|zinder-ingest:.*|zinder-ingest:v0.1.0|' /etc/zinder/docker-compose.yml
sudo sed -i 's|zinder-query:.*|zinder-query:v0.1.0|'  /etc/zinder/docker-compose.yml
sudo systemctl start zinder
```

The canonical store at `/var/lib/docker/volumes/zinder-data` survives rollback. If a schema migration was introduced, consult that release's notes before downgrading.

## Troubleshooting

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| `/readyz` stays `not_ready` with cause `node_unavailable` | Zebra RPC not reachable from the container | Check `ZINDER_NODE__JSON_RPC_ADDR`; if Zebra is on the host, use `host.docker.internal` (Linux: `--add-host host.docker.internal:host-gateway`) |
| `/readyz` cause is `node_capability_missing` | Zebra is too old to serve a required RPC | Upgrade Zebra to the version pinned in this Zinder release |
| `/readyz` cause is `schema_mismatch` | Existing store was created by an incompatible Zinder version | Migrate or recreate; consult the release notes |
| `/readyz` cause is `reorg_window_exceeded` | The non-finalized reorg crossed `reorg_window_blocks` | Operator action: re-sync from the divergence point per ADR-0007 |
| Startup logs show `phase=connect_node outcome=failed reason=...` | Credential or network issue contacting Zebra | The `reason` field carries the underlying error |
| `docker compose up` exits immediately | Container failed health check before settling | Increase `start_period` in the Compose healthcheck if your initial sync is slow |

## References

- [ADR-0007: Multi-process storage access](../adrs/0007-multi-process-storage-access.md)
- [ADR-0018: Environment variable secret policy](../adrs/0018-environment-variable-secret-policy.md)
- [`deploy/docker-compose.yml`](../../deploy/docker-compose.yml)
- [`deploy/systemd/zinder.service`](../../deploy/systemd/zinder.service)
- [Service operations](../architecture/service-operations.md)
