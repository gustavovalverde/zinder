# Railway canonical-writer validation

Railway is not a wallet-serving topology. The supported RocksDB composition
requires canonical and wallet owners plus readers on one shared host
filesystem. The checked Railway target runs only `zinder-ingest` for isolated
canonical correctness, restart, or performance validation.

Use [Deploying on a VM](deploying-on-a-vm.md) for wallet serving.

## Target admission

The Railway build requires:

```text
RAILWAY_DOCKER_TARGET_STAGE=zinder-canonical-runtime
```

Verify the repository guard before deploying:

```bash
bash scripts/validate-deployment-admission.sh \
  --deployment-class canary \
  --target zinder-canonical-runtime
bash scripts/validate-deployment-admission.sh --verify-railway-default
```

The target starts `zinder-ingest` through
`deploy/canonical-runtime-entrypoint`, drops privileges to the `zinder` user,
and exposes the operational endpoint. It does not run projector, compatibility,
native query, explorer, or coherent restore services.

## Configuration

Attach one persistent volume at `/var/lib/zinder`. Configure the network, Zebra
endpoint and authentication, canonical path, wallet workload, wallet-serving
coverage, transaction retention, reorg policy, operations listener, and private
control listener through `ZINDER_*` variables.

`/healthz` proves process liveness. `/readyz` proves only the canonical writer
and mempool contract. It does not prove wallet projection, exact-pair serving,
public TLS, coherent recovery, or client compatibility.

Record the image digest, network, starting and final fences, construction and
publication duration, peak memory, storage bytes, source settings, and restart
result outside the repository. Preserve the volume after a failed run until
the required diagnostic evidence is captured.

## Boundaries

- Do not label this target as a production wallet deployment.
- Do not add query or compatibility processes to the same container.
- Do not use Railway service volumes as cross-service RocksDB replication.
- Do not infer full-topology capacity from the canonical directory alone.

See [ADR-0035](../adrs/0035-canonical-storage-topologies.md),
[Service boundaries](../architecture/service-boundaries.md), and
[Public interface configuration](../architecture/public-interfaces.md#configuration).
