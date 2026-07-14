# Storage benchmark environment

`docker-compose.storage-benchmark.yml` is the disposable, resource-bounded
environment for the fact-first storage bake-off. It keeps benchmark state and
resource limits separate from the production topology in `docker-compose.yml`.
The exact image ID, source revision, fixture digest, resource limits, storage
class, writer settings, and durability posture make runs comparable. The image
build itself is not claimed to be bit-reproducible.

The current `zinder-bench` binary replays only the RocksDB implementation. The
PostgreSQL service is a digest-pinned PostgreSQL 18.4 candidate endpoint for
schema, bulk-load, and lifecycle work as that implementation lands. Starting
both services does not yet produce an apples-to-apples backend comparison, and
results must not be reported as one until the same fixture and report contract
drive PostgreSQL.

## Storage and resource model

The immutable block fixture and starting RocksDB checkpoint are read-only bind
mounts. JSON reports use a writable bind mount so they survive stack cleanup.
The replay's mutable RocksDB clone uses a named volume because writable RocksDB
files on Docker Desktop's macOS virtiofs path can fail manifest validation.

PostgreSQL `PGDATA` also uses a named volume. This preserves durability and
exercises representative Linux filesystem behavior; putting the database on
tmpfs would turn the storage comparison into a memory benchmark. Only `/tmp` is
tmpfs. `shm_size` is explicit and separate because PostgreSQL parallel workers
and dynamic shared memory use `/dev/shm`.

Both engines default to eight CPUs and 16 GiB. PostgreSQL keeps `fsync`, full
page writes, and synchronous commit enabled. Change resource limits and database
memory settings together, record every override with the result, and do not
compare runs that used different limits.

The `rocksdb` and `postgres` profiles run either side in isolation. The `full`
profile enables both sides for the eventual end-to-end comparison without
adding dashboards, administration UIs, or unrelated services.

Use isolated profile runs for backend throughput measurements so the engines do
not compete for host resources. Use `full` for integration tests that
intentionally exercise both services together.

## Configure and validate

Run commands from the repository root. Copy the example environment file to a
local file and point the three paths at an immutable fixture, its matching
starting checkpoint, and a writable result directory:

```bash
cp deploy/.env.storage-benchmark.example /tmp/zinder-storage-benchmark.env
mkdir -p benchmark-results
```

Formal replays require structured resource provenance and an immutable image
identity. Start from a committed, clean worktree. Build a revision-scoped local
tag, capture Docker's content-addressed image ID, and give Compose that ID
rather than the mutable tag. `ZINDER_BENCH_RUNNER_ID` identifies the complete
hardware and runtime profile, while `ZINDER_BENCH_STORAGE_CLASS` names the
measured volume and host storage class. Change either identity when the
underlying profile changes.

```bash
test -z "$(git status --porcelain)"
: "${ZINDER_BENCH_RUNNER_ID:?export the immutable benchmark runner profile identity}"
: "${ZINDER_BENCH_STORAGE_CLASS:?export the measured storage class identity}"
software_revision="$(git rev-parse HEAD)"
image_tag="zinder-bench:${software_revision}"
image_id_file="/tmp/zinder-bench-${software_revision}.iid"

rm -f "$image_id_file"
docker build \
  --file deploy/Dockerfile \
  --target zinder-bench \
  --tag "$image_tag" \
  --iidfile "$image_id_file" \
  .
image_reference="$(sed -n '1p' "$image_id_file")"
test "${#image_reference}" -eq 71

printf 'ZINDER_BENCH_IMAGE=%s\n' "$image_reference" \
  >> /tmp/zinder-storage-benchmark.env
printf 'ZINDER_BENCH_SOFTWARE_REVISION=%s\n' "$software_revision" \
  >> /tmp/zinder-storage-benchmark.env
printf 'ZINDER_BENCH_RUNNER_ID=%s\n' "$ZINDER_BENCH_RUNNER_ID" \
  >> /tmp/zinder-storage-benchmark.env
printf 'ZINDER_BENCH_STORAGE_CLASS=%s\n' "$ZINDER_BENCH_STORAGE_CLASS" \
  >> /tmp/zinder-storage-benchmark.env
```

Regenerate the file when the tested revision or image content changes. A shared
image may instead use a nonempty registry reference pinned with
`@sha256:<64-hex>`. Do not use a branch name, `latest`, `local`, a developer
name, or an empty value as provenance. The CLI rejects mutable image tags.

The starting checkpoint tip must be exactly one block below the fixture's first
height. The result directory must be writable by UID 1000, which is the
non-root user in the benchmark image.

Render and validate the complete model before starting containers:

```bash
ZINDER_BENCH_IMAGE=sha256:0000000000000000000000000000000000000000000000000000000000000000 \
ZINDER_BENCH_SOFTWARE_REVISION=config-validation \
ZINDER_BENCH_RUNNER_ID=config-validation \
ZINDER_BENCH_STORAGE_CLASS=config-validation \
docker compose \
  --env-file /tmp/zinder-storage-benchmark.env \
  -f deploy/docker-compose.storage-benchmark.yml \
  --profile full \
  config --quiet

ZINDER_BENCH_IMAGE=sha256:0000000000000000000000000000000000000000000000000000000000000000 \
ZINDER_BENCH_SOFTWARE_REVISION=config-validation \
ZINDER_BENCH_RUNNER_ID=config-validation \
ZINDER_BENCH_STORAGE_CLASS=config-validation \
docker compose \
  --env-file /tmp/zinder-storage-benchmark.env \
  -f deploy/docker-compose.storage-benchmark.yml \
  config --profiles
```

The all-zero image ID and `config-validation` provenance values are only for
static model validation; they must never be used for a benchmark report.

## Start and verify PostgreSQL

Start only the database candidate and wait for its healthcheck:

```bash
docker compose \
  --env-file /tmp/zinder-storage-benchmark.env \
  -f deploy/docker-compose.storage-benchmark.yml \
  --profile postgres up -d --wait postgres-candidate
```

Prove both readiness and the selected server version from inside the container:

```bash
docker compose \
  --env-file /tmp/zinder-storage-benchmark.env \
  -f deploy/docker-compose.storage-benchmark.yml \
  exec -T postgres-candidate sh -ceu '
    pg_isready --username "$POSTGRES_USER" --dbname "$POSTGRES_DB"
    psql --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
      --set ON_ERROR_STOP=1 \
      --command "SELECT current_setting('\''server_version'\''), current_database();"
  '
```

The host endpoint defaults to `postgresql://zinder_bench:zinder_bench_local_only@127.0.0.1:55432/zinder_bench`.
Those credentials are intentionally public and suitable only for this
loopback-bound local environment.

## Run the current-schema RocksDB oracle

The image was built and reduced to an immutable ID during configuration.
Compose never rebuilds it implicitly, so the executed bytes and report identity
cannot drift between arms.

Every replay mutates its starting store. Begin with clean benchmark volumes,
then copy the read-only checkpoint into the named RocksDB volume. The copy runs
inside the existing benchmark image and fixes ownership for its UID 1000 user:

```bash
docker compose \
  --env-file /tmp/zinder-storage-benchmark.env \
  -f deploy/docker-compose.storage-benchmark.yml \
  --profile full down --volumes --remove-orphans

docker compose \
  --env-file /tmp/zinder-storage-benchmark.env \
  -f deploy/docker-compose.storage-benchmark.yml \
  --profile rocksdb run --rm --no-deps \
  --user 0:0 --entrypoint /bin/sh rocksdb-replay -ceu '
    test -n "$(ls -A /benchmark/start-store)"
    test -z "$(ls -A /var/lib/zinder/benchmark-store)"
    cp -a /benchmark/start-store/. /var/lib/zinder/benchmark-store/
    rm -rf /var/lib/zinder/benchmark-store/derive
    chown -R 1000:1000 /var/lib/zinder/benchmark-store
  '
```

The deletion affects only the disposable named-volume clone. It prevents a
projection arm from reusing projection state embedded in the checkpoint. Run
the canonical-only current-schema oracle; its structured report is written
under `ZINDER_BENCH_RESULTS_PATH`:

```bash
docker compose \
  --env-file /tmp/zinder-storage-benchmark.env \
  -f deploy/docker-compose.storage-benchmark.yml \
  --profile rocksdb run --rm --no-deps rocksdb-replay
```

To exercise the formal fixture-replay acceptance boundary, export the target
and hard limit chosen for this exact corpus, reseed the RocksDB volume, and run
the complete canonical-only override. Compose still applies the same CPU and
memory values recorded by the report:

```bash
set -a
. /tmp/zinder-storage-benchmark.env
set +a
: "${ZINDER_BENCH_FIXTURE_REPLAY_TARGET_SECS:?set the fixture-specific target}"
: "${ZINDER_BENCH_FIXTURE_REPLAY_HARD_LIMIT_SECS:?set the fixture-specific hard limit}"

docker compose \
  --env-file /tmp/zinder-storage-benchmark.env \
  -f deploy/docker-compose.storage-benchmark.yml \
  --profile rocksdb run --rm --no-deps rocksdb-replay \
  replay \
  --fixture /benchmark/fixture \
  --store /var/lib/zinder/benchmark-store \
  --block-prepare-concurrency "$ZINDER_BENCH_BLOCK_PREPARE_CONCURRENCY" \
  --software-revision "$ZINDER_BENCH_SOFTWARE_REVISION" \
  --runner-id "$ZINDER_BENCH_RUNNER_ID" \
  --cpu-limit-cores "$ZINDER_BENCH_ROCKSDB_CPUS" \
  --memory-limit-bytes "$ZINDER_BENCH_ROCKSDB_MEMORY_LIMIT_BYTES" \
  --storage-class "$ZINDER_BENCH_STORAGE_CLASS" \
  --image-reference "$ZINDER_BENCH_IMAGE" \
  --canonical-fixture-replay-target-secs "$ZINDER_BENCH_FIXTURE_REPLAY_TARGET_SECS" \
  --canonical-fixture-replay-hard-limit-secs "$ZINDER_BENCH_FIXTURE_REPLAY_HARD_LIMIT_SECS" \
  --report "/benchmark/results/rocksdb-current-schema-oracle-${ZINDER_BENCH_SOFTWARE_REVISION}-acceptance.json"
```

For a projection arm or cache sweep, override the service command explicitly so
the changed flags remain visible in shell history and the run record:

```bash
set -a
. /tmp/zinder-storage-benchmark.env
set +a

docker compose \
  --env-file /tmp/zinder-storage-benchmark.env \
  -f deploy/docker-compose.storage-benchmark.yml \
  --profile rocksdb run --rm --no-deps rocksdb-replay \
  replay \
  --fixture /benchmark/fixture \
  --store /var/lib/zinder/benchmark-store \
  --block-prepare-concurrency 16 \
  --block-cache-bytes 536870912 \
  --projection-preset wallet \
  --software-revision "$ZINDER_BENCH_SOFTWARE_REVISION" \
  --runner-id "$ZINDER_BENCH_RUNNER_ID" \
  --cpu-limit-cores "$ZINDER_BENCH_ROCKSDB_CPUS" \
  --memory-limit-bytes "$ZINDER_BENCH_ROCKSDB_MEMORY_LIMIT_BYTES" \
  --storage-class "$ZINDER_BENCH_STORAGE_CLASS" \
  --image-reference "$ZINDER_BENCH_IMAGE" \
  --report /benchmark/results/rocksdb-wallet-cache-512m.json
```

Reseed the named volume before every replay. Reusing a mutated checkpoint makes
the run invalid. The default report path includes the source revision and is
created exclusively; a second run at that path fails instead of replacing
evidence. Override the complete command with a new report path for every sweep
arm.

Acceptance thresholds are deliberately absent from the default service
command. The current CLI accepts only the paired
`--canonical-fixture-replay-target-secs` and
`--canonical-fixture-replay-hard-limit-secs` flags. They apply to this exact
captured range and starting checkpoint, not ADR-0035's fresh canonical
construction lifecycle. A thresholded run must be canonical-only and must keep
all provenance flags shown above. Production construction, restore, following,
and wallet-readiness thresholds remain unavailable until dedicated drivers own
those complete boundaries.

## Full profile and cleanup

The `full` profile can provision PostgreSQL before running the current-schema
RocksDB oracle. It becomes the shared entrypoint for identical backend arms
once the PostgreSQL replay command exists:

```bash
docker compose \
  --env-file /tmp/zinder-storage-benchmark.env \
  -f deploy/docker-compose.storage-benchmark.yml \
  --profile full up -d --wait postgres-candidate

docker compose \
  --env-file /tmp/zinder-storage-benchmark.env \
  -f deploy/docker-compose.storage-benchmark.yml \
  --profile full run --rm --no-deps rocksdb-replay
```

Remove containers, the mutable RocksDB clone, and the PostgreSQL cluster after
capturing reports. Bind-mounted fixtures and results are not deleted:

```bash
docker compose \
  --env-file /tmp/zinder-storage-benchmark.env \
  -f deploy/docker-compose.storage-benchmark.yml \
  --profile full down --volumes --remove-orphans
```
