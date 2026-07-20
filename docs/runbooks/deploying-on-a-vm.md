# Deploying the wallet service on one VM

This runbook operates the supported `rocksdb-single-host` wallet-serving
topology. Three independent processes share one host filesystem:
`zinder-ingest` owns canonical storage, `zinder-projector` owns the wallet
projection, and `zinder-compat-lightwalletd` serves immutable canonical and
wallet secondary pairs. Query, explorer, and mixed single-container images are
not release paths.

A successful deployment is a canary until the release's mainnet construction,
wallet-build, coherent-restore, capacity, replacement, and independent-client
gates have current evidence. See
[ADR-0035](../adrs/0035-canonical-storage-topologies.md) and the
[testing runbook](testing.md).

## Prerequisites

- A Linux host with Docker Engine 24 or newer and Docker Compose v2.
- A pinned Zinder release checkout at `/opt/zinder`.
- A running [Z3](https://github.com/ZcashFoundation/z3) stack on the same
  Docker host. Zinder attaches to its network and cookie volume.
- Storage sized for canonical data, the wallet projection, a coherent
  checkpoint, compaction/restore workspace, and chain-growth reserve.
- An HTTP/2-capable reverse proxy for TLS, authentication, rate limits, and
  quotas.

Do not place the three storage owners on hosts with independent filesystems.
RocksDB secondary replication in this lifecycle is a same-filesystem boundary,
not a network replication protocol.

## Install

Clone and pin the intended release:

```bash
sudo git clone https://github.com/gustavovalverde/zinder.git /opt/zinder
sudo git -C /opt/zinder checkout vX.Y.Z
sudo install -d -o root -g root -m 0755 /etc/zinder
sudo cp /opt/zinder/deploy/.env.mainnet /etc/zinder/env
```

Use `.env.testnet` or `.env.regtest` for another network. Each file contains
a stable, non-secret `ZINDER_PROJECTOR_BUILD_OWNER_HEX`; assign a different
32-character hexadecimal value to every side-by-side projector lane.

Create the two root-owned file-backed control-secret directories before
validating Compose. The ordinary ingest token is mounted into all three
runtimes; the checkpoint token is mounted only into ingest and projector.
The group must be the container service group (the release images use GID
1000), while the files remain owned by root and are not stored in the shared
RocksDB volume.

```bash
sudo install -d -o root -g 1000 -m 0750 /etc/zinder/control-secrets
sudo install -d -o root -g 1000 -m 0750 /etc/zinder/ingest-control-secrets
openssl rand -hex 32 | sudo tee /etc/zinder/control-secrets/ingest.token >/dev/null
openssl rand -hex 32 | sudo tee /etc/zinder/control-secrets/checkpoint.token >/dev/null
sudo cp /etc/zinder/control-secrets/ingest.token /etc/zinder/ingest-control-secrets/ingest.token
sudo chown root:1000 /etc/zinder/control-secrets/*.token /etc/zinder/ingest-control-secrets/ingest.token
sudo chmod 0440 /etc/zinder/control-secrets/*.token /etc/zinder/ingest-control-secrets/ingest.token
```

The checked-in network env templates already set
`ZINDER_CONTROL_SECRETS_DIR=/etc/zinder/control-secrets` and
`ZINDER_INGEST_CONTROL_SECRET_DIR=/etc/zinder/ingest-control-secrets`. If the
host uses different paths, update the copied `/etc/zinder/env` values before
resolving Compose. Do not place `checkpoint.token` in the
compatibility-readable directory.

Validate the resolved topology before starting it:

```bash
cd /opt/zinder
docker compose --env-file /etc/zinder/env \
  -f deploy/docker-compose.yml config -q
```

Build and start the three release runtimes:

```bash
docker compose --env-file /etc/zinder/env \
  -f deploy/docker-compose.yml up -d --build
```

The Compose topology gives projector and compatibility containers the ingest
container's network namespace. This keeps `CanonicalControl` and
`IngestControl` on `127.0.0.1:9100` without weakening the non-loopback
bearer-token rule. The compatibility gRPC port is published on host loopback
only.

Create two file-backed control secrets outside the shared data volume: the
ordinary `ingest.token`, mounted where compatibility can read it, and the
separate `checkpoint.token`, mounted only into ingest and projector. Set
`ZINDER_INGEST_CONTROL_SECRET_DIR` to a directory containing only
`ingest.token`, and `ZINDER_CONTROL_SECRETS_DIR` to the ingest/projector
directory containing both files. `checkpoint.token` authorizes both the
loopback `ProjectorControl` capture request and the method-level canonical
checkpoint capability; compatibility must never mount or read it.

The root-owned `state-init` service initializes the shared data volume and the
separate checkpoint-staging volume for UID/GID 1000 before ingest starts.
Checkpoint staging is mounted only into ingest and projector, never into
compatibility. Canonical checkpoint requests accept only an opaque candidate
identifier beneath that root; the projector prepares the candidate directory,
ingest creates the fixed `canonical.rocksdb` child through its owner queue, and
cold admission runs off the writer queue. The current capture foundation can add
the exact-fence wallet checkpoint and publish a format-1 `state-bundle.json`
manifest last. It can also create and byte-admit a fixed-layout sealed recovery
directory whose outer manifest is published last and binds every payload file,
both checkpoint identities, and the canonical construction-manifest sidecar.
The configured local archive root is not physical WORM storage. Instance-state
reset, immutable external publication, inactive-lane restore, and the required
10000-block tail proof remain absent, so neither captured nor locally sealed
manifests satisfy the restore production gate.

## Verify the ownership chain

Use the host ports from the selected env file. Mainnet uses 9105, 9110, 9107,
and 9067; testnet adds 10000; regtest adds 20000.

```bash
curl -fsS http://127.0.0.1:9105/healthz
curl -fsS http://127.0.0.1:9105/readyz
curl -fsS http://127.0.0.1:9110/readyz
curl -fsS http://127.0.0.1:9107/readyz

grpcurl -plaintext -d '{}' 127.0.0.1:9067 \
  cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo
```

Interpret the probes in order:

1. Ingest is ready only when canonical following is within the configured lag
   boundary and the current mempool generation has completed hydration.
2. Projector is ready only when its admitted wallet store has reached an
   authenticated canonical event fence.
3. Compatibility is ready only when its inactive canonical and wallet
   secondaries converge to one exact network, epoch, event sequence, visible
   tip, settled tip, and digest before publication.

A healthy process with a non-ready dependency must continue returning 503. Do
not route wallet traffic based on `/healthz`.

## Put TLS in front of compatibility

Route the public hostname to the loopback compatibility port with h2c upstream
transport. The following Caddy fragment demonstrates TLS termination only:

```caddy
zinder.example.org {
    reverse_proxy {
        to 127.0.0.1:9067
        transport http {
            versions h2c
        }
    }
}
```

This fragment does not provide client authorization, rate limits, quotas, or
readiness-aware routing. Supply those controls in the operator-owned proxy or
load balancer without changing the lightwalletd wire contract. New wallet
traffic must stop routing whenever compatibility `/readyz` returns 503; an
already accepted request may drain against its immutable canonical/wallet
generation.

Keep the ops endpoints and control plane private. The release does not ship
server-side TLS or public authorization. Before public cutover, retain all of
the following operator evidence:

- a successful `GetLightdInfo` call through public DNS with a publicly trusted
  certificate, correct SNI, and HTTP/2 negotiation;
- the exact proxy access, connection, request, stream, and rate-limit policy,
  plus a test showing excess traffic is rejected without breaking a normal
  wallet sync stream;
- an external firewall scan proving that plaintext compatibility, ingest
  control, ops, Prometheus, Grafana, and storage ports are unreachable;
- a readiness-routing test showing new public requests are rejected or drained
  while `http://127.0.0.1:9107/readyz` is not ready;
- certificate issuance and renewal ownership, expiry monitoring, and the
  rollback procedure for a failed proxy or certificate rotation.

## Install systemd supervision

The repository unit expects the checkout at `/opt/zinder` and the selected
env file at `/etc/zinder/env`:

```bash
sudo cp /opt/zinder/deploy/systemd/zinder.service \
  /etc/systemd/system/zinder.service
sudo systemctl daemon-reload
sudo systemctl enable --now zinder
sudo journalctl -u zinder.service -f
```

## Side-by-side wallet rebuild and cutover

Never rebuild a wallet store in place while readers serve it. Provision lane B
with all of the following distinct from lane A:

- `wallet.path`;
- projector `storage.canonical_secondary_path`;
- compatibility canonical and wallet secondary roots;
- projector build-owner identity;
- compatibility gRPC and ops ports.

Both projector lanes may read the same canonical primary and hold independent
retention leases. Start lane B, wait for its projector and compatibility
`/readyz` endpoints, run parity and client probes against lane B, then move
the reverse-proxy upstream atomically. Keep lane A running until in-flight
requests drain. Only then stop lane A and reclaim its wallet and secondary
paths.

If lane B reports an expired event cursor or a source-fence mismatch, leave
lane A serving and rebuild lane B from a newly captured canonical fence. Do not
copy READY markers, rename a live RocksDB directory, or reuse a drained
secondary path before every request holding its generation has released it.

## Restart and rollback

Restart one owner at a time in dependency order:

```bash
docker compose --env-file /etc/zinder/env \
  -f /opt/zinder/deploy/docker-compose.yml restart zinder-ingest
docker compose --env-file /etc/zinder/env \
  -f /opt/zinder/deploy/docker-compose.yml restart zinder-projector
docker compose --env-file /etc/zinder/env \
  -f /opt/zinder/deploy/docker-compose.yml restart zinder-compat-lightwalletd
```

After each restart, require the complete readiness chain again. For a release
rollback, route traffic to a previously certified side-by-side lane. Do not
start a binary on existing canonical or wallet paths unless that release
explicitly declares their physical layouts compatible.

## Production admission checklist

Before routing mainnet traffic, attach evidence for every item:

- fresh canonical construction within the 3-hour hard gate;
- wallet construction within the 2-hour hard gate and complete wallet-ready
  lifecycle within 4 hours;
- verified coherent canonical-plus-wallet restore and 10000-block tail within
  15 minutes;
- at most 2 blocks of canonical lag and at most 2 canonical epochs of wallet
  lag under sustained following;
- maximum-depth replacement, restart, and secondary-generation race tests;
- storage capacity including checkpoint, compaction, restore, and growth
  reserve;
- fresh create, known-seed restore, non-empty transparent funds, send,
  mempool, confirmation, restart, projection lag, and reorg on the pinned
  independent client.

A local deployment is suitable for validation and controlled canaries. It is
not a production certification claim without release-specific evidence for
every gate above.

## References

- [Public environment-variable contract](../architecture/public-interfaces.md#environment-variable-mapping)
- [Initial sync](initial-sync.md)
- [Testing](testing.md)
- [ADR-0035](../adrs/0035-canonical-storage-topologies.md)
