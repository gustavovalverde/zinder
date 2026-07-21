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
  --release-workflow "$repository_root/.github/workflows/release.yml"
bash "$validator" \
  --build-images-workflow "$repository_root/.github/workflows/build-images.yml"
bash "$validator" \
  --prometheus-config "$repository_root/deploy/observability/prometheus.yml"

if grep -Eq 'zinder-[a-z-]+:latest' "$repository_root/deploy/docker-compose.yml"; then
  fail "the deployment Compose file defaults a Zinder runtime to latest"
fi

for z3_env in mainnet testnet; do
  grep -Fqx 'ZINDER_NODE__INDEXER_GRPC_ADDR=http://zebra:8155' \
    "$repository_root/deploy/.env.$z3_env" \
    || fail "the $z3_env lane does not use Zebra's internal indexer endpoint"
done
for z3_env in mainnet testnet regtest; do
  projector_owner="$(sed -n 's/^ZINDER_PROJECTOR_BUILD_OWNER_HEX=//p' \
    "$repository_root/deploy/.env.$z3_env")"
  [[ "$projector_owner" =~ ^[0-9a-f]{32}$ && "$projector_owner" =~ [a-f] ]] \
    || fail "the $z3_env projector owner is not a config-safe 32-character hex string"
done

resolved_compose="$temporary_directory/resolved-compose.json"
control_secrets="$temporary_directory/control-secrets"
mkdir -p "$control_secrets"
printf 'test-ingest-token\n' > "$control_secrets/ingest.token"
printf 'test-checkpoint-token\n' > "$control_secrets/checkpoint.token"
ZINDER_CONTROL_SECRETS_DIR="$control_secrets" \
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

query_checkpoint_mount_compose="$temporary_directory/query-checkpoint-mount-compose.json"
jq '
  (.services["zinder-ingest"].volumes[] | select(.target == "/var/lib/zinder/checkpoints")) as $checkpoint_mount
  |
  .services["zinder-query"].volumes += [
    $checkpoint_mount
  ]
' "$resolved_compose" > "$query_checkpoint_mount_compose"
expect_rejected \
  "a Compose contract mounting checkpoint staging into native query" \
  --compose-contract "$query_checkpoint_mount_compose"

query_checkpoint_secret_compose="$temporary_directory/query-checkpoint-secret-compose.json"
jq '
  (.services["zinder-ingest"].volumes[] | select(.target == "/var/run/zinder-checkpoint/checkpoint.token")) as $checkpoint_secret
  |
  .services["zinder-query"].volumes += [
    ($checkpoint_secret | .target = "/var/run/alternate/checkpoint-capability")
  ]
' "$resolved_compose" > "$query_checkpoint_secret_compose"
expect_rejected \
  "a native query contract mounting the checkpoint secret under an alternate target" \
  --compose-contract "$query_checkpoint_secret_compose"

swapped_query_config_compose="$temporary_directory/swapped-query-config-compose.json"
jq '
  (.services["zinder-compat-lightwalletd"].volumes[] | select(.target == "/etc/zinder/config.toml") | .source) as $compat_config
  |
  (.services["zinder-query"].volumes[] | select(.target == "/etc/zinder/config.toml") | .source) = $compat_config
' "$resolved_compose" > "$swapped_query_config_compose"
expect_rejected \
  "a native query contract mounting the compatibility runtime config" \
  --compose-contract "$swapped_query_config_compose"

overlapping_config_root="$temporary_directory/overlapping/deploy/config"
mkdir -p "$overlapping_config_root"
sed 's#/query/#/compat/#g' "$repository_root/deploy/config/query.toml" \
  > "$overlapping_config_root/query.toml"
overlapping_reader_paths_compose="$temporary_directory/overlapping-reader-paths-compose.json"
jq --arg query_config "$overlapping_config_root/query.toml" '
  (.services["zinder-query"].volumes[] | select(.target == "/etc/zinder/config.toml") | .source) = $query_config
' "$resolved_compose" > "$overlapping_reader_paths_compose"
expect_rejected \
  "native and compatibility readers sharing secondary roots" \
  --compose-contract "$overlapping_reader_paths_compose"

nested_config_root="$temporary_directory/nested/deploy/config"
mkdir -p "$nested_config_root"
sed 's#/var/lib/zinder/query/canonical-secondary#/var/lib/zinder/compat/canonical-secondary/nested#' \
  "$repository_root/deploy/config/query.toml" > "$nested_config_root/query.toml"
nested_reader_paths_compose="$temporary_directory/nested-reader-paths-compose.json"
jq --arg query_config "$nested_config_root/query.toml" '
  (.services["zinder-query"].volumes[] | select(.target == "/etc/zinder/config.toml") | .source) = $query_config
' "$resolved_compose" > "$nested_reader_paths_compose"
expect_rejected \
  "a native reader secondary nested beneath a compatibility reader root" \
  --compose-contract "$nested_reader_paths_compose"

public_host_compose="$temporary_directory/public-host-compose.json"
jq '.services["zinder-ingest"].ports[0].host_ip = "0.0.0.0"' \
  "$resolved_compose" > "$public_host_compose"
expect_rejected \
  "a Compose contract with a public host publication" \
  --compose-contract "$public_host_compose"

duplicate_host_port_compose="$temporary_directory/duplicate-host-port-compose.json"
jq '.services["zinder-ingest"].ports[1].published = .services["zinder-ingest"].ports[0].published' \
  "$resolved_compose" > "$duplicate_host_port_compose"
expect_rejected \
  "a Compose contract publishing two runtimes on the same host port" \
  --compose-contract "$duplicate_host_port_compose"

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

split_query_namespace_compose="$temporary_directory/split-query-namespace-compose.json"
jq '.services["zinder-query"].network_mode = null' \
  "$resolved_compose" > "$split_query_namespace_compose"
expect_rejected \
  "a Compose contract splitting the native query control namespace" \
  --compose-contract "$split_query_namespace_compose"

implicit_security_compose="$temporary_directory/implicit-security-compose.json"
jq 'del(.services["zinder-compat-lightwalletd"].environment.ZINDER_SECURITY__ALLOW_PUBLIC_BIND)' \
  "$resolved_compose" > "$implicit_security_compose"
expect_rejected \
  "a Compose contract relying on an image-baked public-bind opt-in" \
  --compose-contract "$implicit_security_compose"

implicit_query_security_compose="$temporary_directory/implicit-query-security-compose.json"
jq 'del(.services["zinder-query"].environment.ZINDER_SECURITY__ALLOW_PUBLIC_BIND)' \
  "$resolved_compose" > "$implicit_query_security_compose"
expect_rejected \
  "a native query contract relying on an image-baked public-bind opt-in" \
  --compose-contract "$implicit_query_security_compose"

mixed_release_workflow="$temporary_directory/release.yml"
cp "$repository_root/.github/workflows/release.yml" "$mixed_release_workflow"
printf '\n# forbidden release target: zinder-single-container\n' >> "$mixed_release_workflow"
expect_rejected \
  "a release workflow containing the mixed single-container target" \
  --release-workflow "$mixed_release_workflow"

projectorless_release_workflow="$temporary_directory/projectorless-release.yml"
sed '/"zinder-projector:zinder-projector"/d' \
  "$repository_root/.github/workflows/release.yml" \
  > "$projectorless_release_workflow"
expect_rejected \
  "a release workflow omitting zinder-projector" \
  --release-workflow "$projectorless_release_workflow"

queryless_release_workflow="$temporary_directory/queryless-release.yml"
sed '/"zinder-query:zinder-query"/d' \
  "$repository_root/.github/workflows/release.yml" \
  > "$queryless_release_workflow"
expect_rejected \
  "a release workflow omitting zinder-query" \
  --release-workflow "$queryless_release_workflow"

queryless_merge_workflow="$temporary_directory/queryless-merge-release.yml"
sed '/^[[:space:]]*- zinder-query$/d' \
  "$repository_root/.github/workflows/release.yml" \
  > "$queryless_merge_workflow"
expect_rejected \
  "a release workflow building query but omitting its published manifest" \
  --release-workflow "$queryless_merge_workflow"

manual_release_workflow="$temporary_directory/manual-release.yml"
sed '/^on:/a\  workflow_dispatch:' \
  "$repository_root/.github/workflows/release.yml" \
  > "$manual_release_workflow"
expect_rejected \
  "a publishing workflow with a manual-dispatch path" \
  --release-workflow "$manual_release_workflow"

early_latest_workflow="$temporary_directory/early-latest-release.yml"
sed '/-t "${image}:${RELEASE_TAG}"/a\            -t "${image}:latest"' \
  "$repository_root/.github/workflows/release.yml" \
  > "$early_latest_workflow"
expect_rejected \
  "a release workflow that promotes latest before final publication" \
  --release-workflow "$early_latest_workflow"

unprotected_release_workflow="$temporary_directory/unprotected-release.yml"
awk '
  /^  authorize:/ { in_authorize = 1 }
  in_authorize && /^    environment: release$/ { next }
  in_authorize && /^  build:/ { in_authorize = 0 }
  { print }
' "$repository_root/.github/workflows/release.yml" \
  > "$unprotected_release_workflow"
expect_rejected \
  "a release workflow that pushes digests before environment approval" \
  --release-workflow "$unprotected_release_workflow"

bypassed_authorization_workflow="$temporary_directory/bypassed-authorization-release.yml"
sed '/^[[:space:]]*- authorize$/d' \
  "$repository_root/.github/workflows/release.yml" \
  > "$bypassed_authorization_workflow"
expect_rejected \
  "a release workflow whose first registry writer bypasses approval" \
  --release-workflow "$bypassed_authorization_workflow"

bypassed_manifest_dependency_workflow="$temporary_directory/bypassed-manifest-release.yml"
sed '/^[[:space:]]*- build$/d' \
  "$repository_root/.github/workflows/release.yml" \
  > "$bypassed_manifest_dependency_workflow"
expect_rejected \
  "a release workflow whose manifest publisher bypasses digest builds" \
  --release-workflow "$bypassed_manifest_dependency_workflow"

queryless_build_workflow="$temporary_directory/queryless-build-images.yml"
sed '/"zinder-query:zinder-query:zinder-query"/d' \
  "$repository_root/.github/workflows/build-images.yml" \
  > "$queryless_build_workflow"
expect_rejected \
  "a pull-request image workflow omitting native query" \
  --build-images-workflow "$queryless_build_workflow"

compatless_build_workflow="$temporary_directory/compatless-build-images.yml"
sed '/"zinder-compat-lightwalletd:zinder-compat-lightwalletd:zinder-compat-lightwalletd"/d' \
  "$repository_root/.github/workflows/build-images.yml" \
  > "$compatless_build_workflow"
expect_rejected \
  "a pull-request image workflow omitting lightwalletd compatibility" \
  --build-images-workflow "$compatless_build_workflow"

helpless_build_workflow="$temporary_directory/helpless-build-images.yml"
sed '/docker run --rm --entrypoint="\$help_entrypoint"/d' \
  "$repository_root/.github/workflows/build-images.yml" \
  > "$helpless_build_workflow"
expect_rejected \
  "a pull-request image workflow without runtime help smokes" \
  --build-images-workflow "$helpless_build_workflow"

arm64less_build_workflow="$temporary_directory/arm64less-build-images.yml"
sed '/^[[:space:]]*-[[:space:]]*name: arm64$/,/^[[:space:]]*runner: ubuntu-24.04-arm$/d' \
  "$repository_root/.github/workflows/build-images.yml" \
  > "$arm64less_build_workflow"
expect_rejected \
  "a pull-request image workflow without native arm64 coverage" \
  --build-images-workflow "$arm64less_build_workflow"

platformless_build_workflow="$temporary_directory/platformless-build-images.yml"
sed '/--platform "\$PLATFORM_REF"/d' \
  "$repository_root/.github/workflows/build-images.yml" \
  > "$platformless_build_workflow"
expect_rejected \
  "a pull-request image workflow that ignores its target platform" \
  --build-images-workflow "$platformless_build_workflow"

queryless_prometheus="$temporary_directory/queryless-prometheus.yml"
sed '/targets: \["zinder-ingest:9106"\]/d' \
  "$repository_root/deploy/observability/prometheus.yml" \
  > "$queryless_prometheus"
expect_rejected \
  "a deploy Prometheus config omitting the native query ops target" \
  --prometheus-config "$queryless_prometheus"

mislabeled_query_prometheus="$temporary_directory/mislabeled-query-prometheus.yml"
sed '/job_name: "zinder-query"/,/service: "zinder-query"/s/service: "zinder-query"/service: "zinder-compat-lightwalletd"/' \
  "$repository_root/deploy/observability/prometheus.yml" \
  > "$mislabeled_query_prometheus"
expect_rejected \
  "a deploy Prometheus config mislabeling the native query target" \
  --prometheus-config "$mislabeled_query_prometheus"

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
