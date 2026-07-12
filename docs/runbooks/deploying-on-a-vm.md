# Deploying Zinder on a VM

This runbook gets a fresh Zinder deployment onto a single Linux VM in roughly 30 minutes. Target shape: Docker Compose orchestrating `zinder-ingest` + `zinder-query`, behind systemd, attached to a Z3 stack on the same VM via the [Z3 platform contract](https://github.com/ZcashFoundation/z3/blob/main/docs/contract.md).

Steps below assume a Debian-family VM with Docker Engine 24+ installed. Adapt package commands for your distribution.

## Prerequisites

- Linux VM (4 vCPU, 8 GB RAM, 500 GB disk minimum for testnet; mainnet sizing larger).
- Docker Engine 24+ and Docker Compose v2 (`docker compose version`).
- A running [Z3 stack](https://github.com/ZcashFoundation/z3) on the same Docker host. Zinder reads Zebra's JSON-RPC over Z3's external network and the cookie file via Z3's shared cookie volume; no credentials need to be copied or rotated by hand. See Z3's [Quick start](https://github.com/ZcashFoundation/z3#quick-start).
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
│                                │   zinder-<network>-data/   │
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
    https://raw.githubusercontent.com/gustavovalverde/zinder/main/deploy/docker-compose.yml
sudo curl -fsSLo /etc/zinder/config/ingest.toml \
    https://raw.githubusercontent.com/gustavovalverde/zinder/main/deploy/single-container/config.example.ingest.toml
sudo curl -fsSLo /etc/zinder/config/query.toml \
    https://raw.githubusercontent.com/gustavovalverde/zinder/main/deploy/single-container/config.example.query.toml
```

Each Zinder binary strict-parses its own TOML schema (writer and reader fields do not share a section set), so the single-container image mounts two configs. Adjust the `[node]`, `[storage]`, and per-service blocks (`[ingest]` for `ingest.toml`, `[query]` for `query.toml`) to match your VM. Each example config documents every field.

### 2. Pick a network env file

The repo ships one env file per network: `deploy/.env.mainnet`, `deploy/.env.testnet`, `deploy/.env.regtest`. Each sets `Z3_NETWORK_LOWER` (picks the matching `z3-<network>` external network and `z3-<network>-cookie` external volume), the Zebra JSON-RPC + indexer URLs, and the per-network host-port matrix. Inside the Z3 network Zebra resolves at the bare DNS name `zebra` on its per-network RPC port.

```bash
sudo curl -fsSLo /etc/zinder/env \
    https://raw.githubusercontent.com/gustavovalverde/zinder/main/deploy/.env.mainnet
```

(Substitute `.env.testnet` or `.env.regtest` for the other networks; the systemd unit always reads `/etc/zinder/env`.) Two flavours can coexist on one host: mainnet uses the canonical host ports (`9100`/`9101`/`9068`/`9069`/`9105`/`9106`/`9095`); testnet adds `+10000` (`19100`/`19101`/...); regtest adds `+20000`. Host-port assignments live in the env file; the compose file itself is shape-only.

For attaching to a Zebra outside Z3 (advanced), see the [legacy override section](#appendix-attaching-to-a-non-z3-zebra) below.

`--print-config` will redact every secret regardless of how it was injected.

### 3. Install the systemd unit

```bash
sudo curl -fsSLo /etc/systemd/system/zinder.service \
    https://raw.githubusercontent.com/gustavovalverde/zinder/main/deploy/systemd/zinder.service
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

The expected readiness sequence is `phase=awaiting_upstream cause=starting` → `phase=bulk_catchup cause=syncing` → `phase=following_tip cause=syncing` → `phase=following_tip cause=ready`. While catching up, `/readyz` returns 503 with `{"status":"not_ready","phase":"bulk_catchup","cause":"syncing","lag_blocks":N,...}`. Once caught up, it returns 200 with `{"status":"ready","phase":"following_tip","cause":"ready",...}`. The startup tracing events (`phase=load_config`, `phase=open_storage`, etc.) shown above describe the process bring-up sequence and are distinct from the unified ingest loop's `IngestPhase` exposed on `/readyz`; the two share the field name `phase` but report different lifecycles. See [ADR-0015](../adrs/0015-unified-phase-driven-ingest.md) for the runtime-phase taxonomy.

### Artifact schema 13 to 14

Schema 14 is an additive, in-place migration for final note-commitment roots;
it does not require deleting or recreating the data volume. It does require a
coordinated restart because the writer creates a new canonical column family
and derive-store families, and a schema-13 reader cannot interpret a newly
committed schema-14 epoch.

1. Build or pull all schema-14 service images before downtime.
2. Stop `zinder-explorer`, `zinder-query`, and `zinder-ingest`.
3. Using the stopped schema-13 ingest image, run
   `zinder-ingest backup --config /etc/zinder/config.toml --to <checkpoint>`
   against the existing volume. Keep the canonical and bundled `derive`
   checkpoint together.
4. Start schema-14 `zinder-ingest` first. Its primary opens the additive
   column families and reconciles the new derive consumer manifest.
5. After ingest is healthy and has committed a schema-14 epoch, start the
   schema-14 query and explorer readers.
6. Verify the volume creation timestamp is unchanged, all three health probes
   pass, and commitment-root backfill progress is advancing.

Do not perform a reader-first rolling restart for this migration. Do not roll
back only one service: restoring a schema-13 binary after a schema-14 commit
requires stopping the stack and serving the pre-migration checkpoint with the
matching schema-13 service set.

### Artifact schema 14 to 15

Schema 15 adds canonical signed Sprout, Sapling, Orchard, and Ironwood
transaction-intrinsic value balances. The new artifact family and exact
paid-fee derive consumer are additive: existing schema-13 rows remain readable,
and the current volume is upgraded in place rather than rebuilt or recreated.

Use the same coordinated checkpoint procedure as schema 13 to 14. Every binary
that opens the canonical or derive store must come from the schema-15 release.
Start the new ingest writer before its readers so it can create the new
canonical and derive column families and reconcile the bundled-consumer
manifest. After the first schema-15 commit, an older binary fails closed by
design. Rollback therefore restores the canonical-plus-derive checkpoint with
the complete prior service set; it never deletes the new families from a live
RocksDB instance.

The paid-fee backfill grows newest-first from the seeded live tail. Monitor its
explicit coverage and unavailable-transaction counts rather than canonical
readiness. Seven-day queries can become complete while older history continues
to prepend, and increasing the configured history window widens the durable
floor without clearing existing paid-fee rows.

Transaction-component history is a separate derive-consumer migration and does
not increment the canonical artifact schema. Version 2 replaces its
fixed-width version-1 rows, so this is a coordinated outage: stop ingest and
every derive-store secondary, take the canonical-plus-derive checkpoint, deploy
one version-2 service set, start ingest first, and wait for
`transaction_component_backfill_completed` before starting readers. A
reader-first rolling restart is invalid because either binary rejects the
other version's manifest. Startup seeds the visible unsettled tail before the
event dispatcher starts, so the worker joins height-1 history to current chain
coverage without waiting for another block. The migration clears only the
transaction-component consumer and preserves the canonical volume plus every
unrelated derive consumer; do not wipe or replay the canonical store.

### Artifact schema 15 to 16

Schema 16 adds canonical per-block value-pool balances. Existing schemas 12 through 15 remain readable while the resumable newest-first enrichment establishes historical coverage. Use the same coordinated checkpoint, writer-first start, and whole-stack rollback procedure. The value-pool balance capability is not a readiness proxy: verify its projection coverage reaches the response fence before claiming complete history.

### Artifact schema 16 to 17

Schema 17 adds final note-commitment roots to newly captured displaced-block rows and introduces the writer-owned displaced-root index and archive coverage record. Capture starts with the first replacement accepted by a schema-17 writer; it does not reconstruct older displaced branches. Archive retention is currently permanent, so operators must include its monotonic growth in capacity planning and checkpoint sizing. Rollback requires the pre-upgrade canonical-plus-derive checkpoint and the matching schema-16 service set.

### 5. Hit the probes

The commands below assume the canonical mainnet host ports. On testnet, add `+10000`; on regtest, `+20000`.

```bash
# Liveness (mainnet):
curl -fsS http://localhost:9106/readyz | jq .

# Metrics (Prometheus scrape):
curl -fsS http://localhost:9106/metrics | head

# gRPC server reflection (when enabled in config):
grpcurl -plaintext localhost:9101 list
```

### 6. Front Zinder with a reverse proxy

Zinder serves plaintext gRPC. Terminate TLS and apply auth/rate-limit at the proxy. Because the listeners bind public or unspecified addresses, each binary requires `security.allow_public_bind = true` (`ZINDER_SECURITY__ALLOW_PUBLIC_BIND=true`) to start; the example configs fetched in step 1 already set it. Without a proxy in front, the plaintext ports are exposed directly. Example Caddy config:

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

The canonical store at `/var/lib/docker/volumes/zinder-<network>-data` survives rollback. If a schema migration was introduced, consult that release's notes before downgrading.

## Troubleshooting

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| `network z3-<net> declared as external, but could not be found` | Z3 stack is not running | Bring up Z3 first: `docker compose --env-file .env.<network> up -d` in the Z3 repo |
| `volume z3-<net>-cookie declared as external, but could not be found` | Z3 stack ran but cookie volume name does not match `Z3_NETWORK_LOWER` | Confirm `Z3_NETWORK_LOWER` matches the running Z3 network |
| `/readyz` stays `not_ready` with cause `node_unavailable` | Zebra not reachable through the Z3 network | Confirm `docker network inspect z3-<network>` lists both `zinder-ingest` and `zebra`; restart the zinder containers if they attached before Z3 was up |
| `/readyz` cause is `node_capability_missing` | Z3's pinned Zebra is too old to serve a required RPC | Upgrade Z3 to a release with the required Zebra version |
| `/readyz` cause is `schema_mismatch` | Existing store was created by an incompatible Zinder version | Migrate or recreate; consult the release notes |
| `/readyz` cause is `reorg_window_exceeded` | The reorg crossed `reorg_window_blocks` | Operator action: re-sync from the divergence point after preserving incident evidence |
| Startup logs show `phase=connect_node outcome=failed reason=permission denied` | Cookie file mode regression in Z3 (sidecar not chmod'ing) | Verify Z3's `cookie-permissions` container is up; the cookie should be mode 0644 |

## Appendix: attaching to a non-Z3 Zebra

If you must point Zinder at a Zebra that lives outside the Z3 stack (legacy deployment, bespoke testbed), remove the `networks:` and `z3-cookie:` blocks from the compose and inject auth manually:

```bash
ZINDER_NODE__JSON_RPC_ADDR=http://your-zebra-host:18232
ZINDER_NODE__AUTH__METHOD=basic
ZINDER_NODE__AUTH__USERNAME=zebra
ZINDER_NODE__AUTH__PASSWORD=...
```

or supply the cookie inline:

```bash
ZINDER_NODE__AUTH__METHOD=cookie
ZINDER_NODE__AUTH__COOKIE=user:cookie-secret-here
```

This path is out of scope for the supported topology; new deployments should use the Z3 attachment above.

## References

- [Z3 platform contract](https://github.com/ZcashFoundation/z3/blob/main/docs/contract.md)
- [Z3 compose-peer integration](https://github.com/ZcashFoundation/z3/blob/main/docs/integrations/compose-peer.md)
- [ADR-0003: Epoch-bound storage access with RocksDB secondaries](../adrs/0003-canonical-storage-access-boundary.md)
- [Public interfaces §Environment variable mapping](../architecture/public-interfaces.md#environment-variable-mapping)
- [`deploy/docker-compose.yml`](../../deploy/docker-compose.yml)
- [`deploy/systemd/zinder.service`](../../deploy/systemd/zinder.service)
- [Service operations](../architecture/service-operations.md)
