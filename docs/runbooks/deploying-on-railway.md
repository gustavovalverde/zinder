# Railway canonical canary

Railway is not an admitted fact-first wallet-serving topology. Its services do
not provide the shared host filesystem required by the current
`rocksdb-single-host` lifecycle, and the deleted mixed single-container
runtime combined canonical-v1 ingest with superseded readers. The checked-in
Railway target therefore supports only an ingest-only diagnostic or
performance canary.

Use the [single-VM runbook](deploying-on-a-vm.md) for the three-runtime
wallet-serving shape.

## Admitted target

The Railway Dockerfile fails closed unless the service explicitly sets:

```text
RAILWAY_DOCKER_TARGET_STAGE=zinder-canonical-runtime
```

That target runs `zinder-ingest` only. It does not publish a wallet API,
projector, compatibility route, coherent checkpoint, or production readiness
claim.

Verify the repository admission boundary before deploying:

```bash
bash scripts/validate-deployment-admission.sh \
  --deployment-class canary \
  --target zinder-canonical-runtime
bash scripts/validate-deployment-admission.sh --verify-railway-default
```

## Canary configuration

Attach one persistent volume at `/var/lib/zinder` and provide the canonical
writer configuration through `ZINDER_*` variables. At minimum, set the
network, Zebra JSON-RPC endpoint, node authentication, wallet projection
preset, wallet-serving coverage, raw transaction retention, and ingest-control
settings required by the release's public environment-variable contract.

The image exposes ingest ops on port 9105. Use `/healthz` for process
liveness and `/readyz` only as canonical/mempool canary evidence. Neither
probe proves wallet serving because Railway does not run the projector or
compatibility reader in this target.

Record the exact image digest, network, starting fence, final fence, elapsed
construction/publication time, peak memory, physical bytes by family, and
restart result. Preserve the volume after a failed performance run until its
evidence has been captured.

## Rejection cases

Deployment admission must continue rejecting:

- the removed `zinder-single-container` target;
- `zinder-canonical-runtime` as a production class;
- release workflows that publish superseded query or explorer images;
- release workflows that omit `zinder-projector`.

A successful Railway canary can close canonical correctness or performance
evidence. It cannot close wallet projection, exact-pair serving, coherent
restore, TLS routing, capacity, or independent-client gates.

## References

- [Fact-first wallet-serving cutover](../plans/fact-first-wallet-serving-cutover.md)
- [ADR-0035](../adrs/0035-fact-first-storage-selection-and-lifecycle.md)
- [Single-VM deployment](deploying-on-a-vm.md)
- [Public environment-variable contract](../architecture/public-interfaces.md#environment-variable-mapping)
