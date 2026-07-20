#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
validator="$repository_root/scripts/validate-deployment-admission.sh"
temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/zinder-deployment-admission.XXXXXX")"
trap 'rm -rf -- "$temporary_directory"' EXIT

fail() {
  echo >&2 "deployment admission test failed: $*"
  exit 1
}

expect_rejected() {
  local description="$1"
  shift

  if bash "$validator" "$@" >/dev/null 2>&1; then
    fail "$description was admitted"
  fi
}

bash "$validator" \
  --deployment-class canary \
  --target zinder-canonical-runtime
bash "$validator" \
  --deployment-class diagnostic \
  --target zinder-canonical-runtime

expect_rejected \
  "the default Railway target" \
  --deployment-class production \
  --railway-default
expect_rejected \
  "an implicit Railway canary target" \
  --deployment-class canary \
  --railway-default
expect_rejected \
  "the mixed single-container target" \
  --deployment-class production \
  --target zinder-single-container
expect_rejected \
  "the ingest-only canonical-runtime target as production" \
  --deployment-class production \
  --target zinder-canonical-runtime

bash "$validator" \
  --release-images-workflow "$repository_root/.github/workflows/release-images.yml"

resolved_compose="$temporary_directory/resolved-compose.json"
control_secrets="$temporary_directory/control-secrets"
ingest_control_secrets="$temporary_directory/ingest-control-secrets"
mkdir -p "$control_secrets" "$ingest_control_secrets"
printf 'test-ingest-token\n' > "$control_secrets/ingest.token"
printf 'test-checkpoint-token\n' > "$control_secrets/checkpoint.token"
printf 'test-ingest-token\n' > "$ingest_control_secrets/ingest.token"
ZINDER_CONTROL_SECRETS_DIR="$control_secrets" \
ZINDER_INGEST_CONTROL_SECRET_DIR="$ingest_control_secrets" \
docker compose \
  --env-file "$repository_root/deploy/.env.testnet" \
  -f "$repository_root/deploy/docker-compose.yml" \
  --profile observability \
  config --format json > "$resolved_compose"
bash "$validator" --compose-contract "$resolved_compose"

missing_state_init_compose="$temporary_directory/missing-state-init-compose.json"
jq 'del(.services["state-init"]) | del(.services["zinder-ingest"].depends_on["state-init"])' \
  "$resolved_compose" > "$missing_state_init_compose"
expect_rejected \
  "a Compose contract without root-owned volume initialization" \
  --compose-contract "$missing_state_init_compose"

missing_checkpoint_root_compose="$temporary_directory/missing-checkpoint-root-compose.json"
jq '.services["state-init"].command = ["chown", "-R", "1000:1000", "/var/lib/zinder"]' \
  "$resolved_compose" > "$missing_checkpoint_root_compose"
expect_rejected \
  "a Compose contract that does not initialize the isolated checkpoint volume" \
  --compose-contract "$missing_checkpoint_root_compose"

shared_checkpoint_volume_compose="$temporary_directory/shared-checkpoint-volume-compose.json"
jq '
  (.services["state-init"].volumes[] | select(.target == "/var/lib/zinder") | .source) as $data_source
  |
  .services["zinder-ingest"].volumes |= map(
    if .target == "/var/lib/zinder/checkpoints" then
      .source = $data_source
    else . end
  )
' "$resolved_compose" > "$shared_checkpoint_volume_compose"
expect_rejected \
  "a Compose contract that stages checkpoints on the shared data volume" \
  --compose-contract "$shared_checkpoint_volume_compose"

aliased_checkpoint_volume_compose="$temporary_directory/aliased-checkpoint-volume-compose.json"
jq '.volumes["checkpoint-staging"].name = .volumes.data.name' \
  "$resolved_compose" > "$aliased_checkpoint_volume_compose"
expect_rejected \
  "a Compose contract whose checkpoint-staging volume aliases the data volume" \
  --compose-contract "$aliased_checkpoint_volume_compose"

missing_owner_checkpoint_mount_compose="$temporary_directory/missing-owner-checkpoint-mount-compose.json"
jq 'del(.services["zinder-projector"].volumes[] | select(.target == "/var/lib/zinder/checkpoints"))' \
  "$resolved_compose" > "$missing_owner_checkpoint_mount_compose"
expect_rejected \
  "a Compose contract missing a projector checkpoint-staging mount" \
  --compose-contract "$missing_owner_checkpoint_mount_compose"

compat_checkpoint_mount_compose="$temporary_directory/compat-checkpoint-mount-compose.json"
jq '
  (.services["zinder-ingest"].volumes[] | select(.target == "/var/lib/zinder/checkpoints")) as $checkpoint_mount
  |
  .services["zinder-compat-lightwalletd"].volumes += [
    $checkpoint_mount
  ]
' "$resolved_compose" > "$compat_checkpoint_mount_compose"
expect_rejected \
  "a Compose contract mounting checkpoint staging into compatibility" \
  --compose-contract "$compat_checkpoint_mount_compose"

public_host_compose="$temporary_directory/public-host-compose.json"
jq '.services["zinder-ingest"].ports[0].host_ip = "0.0.0.0"' \
  "$resolved_compose" > "$public_host_compose"
expect_rejected \
  "a Compose contract with a public host publication" \
  --compose-contract "$public_host_compose"

published_control_compose="$temporary_directory/published-control-compose.json"
jq '.services["zinder-ingest"].ports += [{"host_ip":"127.0.0.1","target":9100,"published":"9100","protocol":"tcp"}]' \
  "$resolved_compose" > "$published_control_compose"
expect_rejected \
  "a Compose contract publishing the private control port" \
  --compose-contract "$published_control_compose"

healthz_compose="$temporary_directory/healthz-compose.json"
jq '.services["zinder-ingest"].healthcheck.test[-1] = "http://localhost:9105/healthz"' \
  "$resolved_compose" > "$healthz_compose"
expect_rejected \
  "a Compose dependency gate using liveness instead of readiness" \
  --compose-contract "$healthz_compose"

split_namespace_compose="$temporary_directory/split-namespace-compose.json"
jq '.services["zinder-projector"].network_mode = null' \
  "$resolved_compose" > "$split_namespace_compose"
expect_rejected \
  "a Compose contract splitting the projector control namespace" \
  --compose-contract "$split_namespace_compose"

implicit_security_compose="$temporary_directory/implicit-security-compose.json"
jq 'del(.services["zinder-compat-lightwalletd"].environment.ZINDER_SECURITY__ALLOW_PUBLIC_BIND)' \
  "$resolved_compose" > "$implicit_security_compose"
expect_rejected \
  "a Compose contract relying on an image-baked public-bind opt-in" \
  --compose-contract "$implicit_security_compose"

mixed_release_workflow="$temporary_directory/release-images.yml"
cp "$repository_root/.github/workflows/release-images.yml" "$mixed_release_workflow"
printf '\n# forbidden release target: zinder-single-container\n' >> "$mixed_release_workflow"
expect_rejected \
  "a release workflow containing the mixed single-container target" \
  --release-images-workflow "$mixed_release_workflow"

projectorless_release_workflow="$temporary_directory/projectorless-release-images.yml"
sed '/"zinder-projector:zinder-projector"/d' \
  "$repository_root/.github/workflows/release-images.yml" \
  > "$projectorless_release_workflow"
expect_rejected \
  "a release workflow omitting zinder-projector" \
  --release-images-workflow "$projectorless_release_workflow"

query_reader_release_workflow="$temporary_directory/query-reader-release-images.yml"
cp "$repository_root/.github/workflows/release-images.yml" "$query_reader_release_workflow"
printf '\n# forbidden release image\n# "zinder-query:zinder-query"\n' \
  >> "$query_reader_release_workflow"
expect_rejected \
  "a release workflow publishing zinder-query" \
  --release-images-workflow "$query_reader_release_workflow"

bash "$validator" --verify-railway-default

guardless_dockerfile="$temporary_directory/Dockerfile.railway-nocache"
cp "$repository_root/deploy/Dockerfile.railway-nocache" "$guardless_dockerfile"
# The literal Docker ARG expression is the unsafe fixture replacement target.
# shellcheck disable=SC2016
sed -i.bak \
  's/FROM zinder-canonical-runtime AS zinder-railway-runtime/FROM ${RAILWAY_DOCKER_TARGET_STAGE} AS zinder-railway-runtime/' \
  "$guardless_dockerfile"
rm "$guardless_dockerfile.bak"
guardless_root="$temporary_directory/guardless-root"
mkdir -p "$guardless_root/deploy" "$guardless_root/scripts"
cp "$repository_root/railway.toml" "$guardless_root/railway.toml"
cp "$guardless_dockerfile" "$guardless_root/deploy/Dockerfile.railway-nocache"
cp "$validator" "$guardless_root/scripts/validate-deployment-admission.sh"
if (
  cd "$guardless_root"
  bash ./scripts/validate-deployment-admission.sh --verify-railway-default >/dev/null 2>&1
); then
  fail "a Railway Dockerfile with a dynamic final stage was admitted"
fi

echo "deployment admission tests passed"
