#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage:
  validate-deployment-admission.sh --deployment-class CLASS --target TARGET
  validate-deployment-admission.sh --deployment-class CLASS --railway-default
  validate-deployment-admission.sh --release-workflow PATH [--release-images-catalog PATH]
  validate-deployment-admission.sh --build-images-workflow PATH [--release-images-catalog PATH]
  validate-deployment-admission.sh --prometheus-config PATH
  validate-deployment-admission.sh --verify-railway-default
  validate-deployment-admission.sh --compose-contract RESOLVED_COMPOSE_JSON

CLASS is one of production, canary, or diagnostic.
EOF
  exit 2
}

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
deployment_class=""
target=""
railway_default=false
release_workflow=""
build_images_workflow=""
release_images_catalog="$repository_root/deploy/release-images.json"
release_images_catalog_explicit=false
prometheus_config=""
verify_railway_default=false
compose_contract=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --deployment-class)
      [[ $# -ge 2 ]] || usage
      deployment_class="$2"
      shift 2
      ;;
    --target)
      [[ $# -ge 2 ]] || usage
      target="$2"
      shift 2
      ;;
    --railway-default)
      railway_default=true
      shift
      ;;
    --release-workflow)
      [[ $# -ge 2 ]] || usage
      release_workflow="$2"
      shift 2
      ;;
    --build-images-workflow)
      [[ $# -ge 2 ]] || usage
      build_images_workflow="$2"
      shift 2
      ;;
    --release-images-catalog)
      [[ $# -ge 2 ]] || usage
      release_images_catalog="$2"
      release_images_catalog_explicit=true
      shift 2
      ;;
    --prometheus-config)
      [[ $# -ge 2 ]] || usage
      prometheus_config="$2"
      shift 2
      ;;
    --verify-railway-default)
      verify_railway_default=true
      shift
      ;;
    --compose-contract)
      [[ $# -ge 2 ]] || usage
      compose_contract="$2"
      shift 2
      ;;
    *)
      usage
      ;;
  esac
done

validate_release_images_catalog() {
  [[ -r "$release_images_catalog" ]] || {
    echo "release admission rejected: cannot read release image catalog $release_images_catalog" >&2
    exit 1
  }

  jq -e '
    type == "array"
    and length == 4
    and all(.[]; type == "string" and test("^zinder-[a-z0-9-]+$"))
    and ([.[]] | unique | length) == length
  ' "$release_images_catalog" >/dev/null || {
    cat >&2 <<'EOF'
release admission rejected: the release image catalog must be a JSON array of
four unique, safely named Zinder runtime images.
EOF
    exit 1
  }

  required_release_images="$({
    printf '%s\n' zinder-ingest zinder-projector zinder-query zinder-compat-lightwalletd
  } | sort)"
  configured_release_images="$(jq -r '.[]' "$release_images_catalog" | sort)"
  if [[ "$configured_release_images" != "$required_release_images" ]]; then
    cat >&2 <<'EOF'
release admission rejected: the release image catalog must contain exactly
ingest, projector, native query, and lightwalletd compatibility images.
EOF
    exit 1
  fi
}

if [[ -n "$compose_contract" ]]; then
  [[ -z "$deployment_class" && -z "$target" && "$railway_default" = false && -z "$release_workflow" && -z "$build_images_workflow" && "$release_images_catalog_explicit" = false && -z "$prometheus_config" && "$verify_railway_default" = false ]] || usage
  [[ -r "$compose_contract" ]] || {
    echo "release admission rejected: cannot read resolved Compose contract $compose_contract" >&2
    exit 1
  }

  jq -e '
    .services as $services
    |
    def exact_healthcheck($url):
      .test == ["CMD", "curl", "-fsS", $url]
      and .interval == "30s"
      and .timeout == "5s"
      and .start_period == "1m0s"
      and .retries == 3;
    def volume_mounts_at($service; $target):
      [ $services[$service].volumes[]? | select(.type == "volume" and .target == $target) ];
    def resolved_volume_name($source):
      (.volumes[$source].name // $source);
    def exact_read_only_bind($service; $target; $source_suffix):
      ([ $services[$service].volumes[]?
        | select(
            .type == "bind"
            and .target == $target
            and .read_only == true
            and (.source | endswith($source_suffix))
          )
      ] | length) == 1;
    def has_checkpoint_secret_source($service):
      [ $services[$service].volumes[]?
        | select(.type == "bind" and (.source | endswith("/checkpoint.token")))
      ] | length > 0;
    volume_mounts_at("state-init"; "/var/lib/zinder") as $state_data
    | volume_mounts_at("state-init"; "/var/lib/zinder/checkpoints") as $state_checkpoint
    | volume_mounts_at("zinder-ingest"; "/var/lib/zinder/checkpoints") as $ingest_checkpoint
    | volume_mounts_at("zinder-projector"; "/var/lib/zinder/checkpoints") as $projector_checkpoint
    | volume_mounts_at("zinder-query"; "/var/lib/zinder/checkpoints") as $query_checkpoint
    | volume_mounts_at("zinder-compat-lightwalletd"; "/var/lib/zinder/checkpoints") as $compat_checkpoint
    | ($services | has("state-init"))
    and ($services | has("zinder-ingest"))
    and ($services | has("zinder-projector"))
    and ($services | has("zinder-query"))
    and ($services | has("zinder-compat-lightwalletd"))
    and ($services["state-init"].user == "0:0")
    and ($services["state-init"].network_mode == "none")
    and ($services["state-init"].restart == "no")
    and ($services["state-init"].command == [
      "sh",
      "-c",
      "mkdir -p /var/lib/zinder/checkpoints /var/lib/zinder/projector/canonical-secondary /var/lib/zinder/query/canonical-secondary /var/lib/zinder/query/wallet-secondary /var/lib/zinder/compat/canonical-secondary /var/lib/zinder/compat/wallet-secondary && chown -R 1000:1000 /var/lib/zinder"
    ])
    and ($state_data | length == 1)
    and ($state_checkpoint | length == 1)
    and ($ingest_checkpoint | length == 1)
    and ($projector_checkpoint | length == 1)
    and ($query_checkpoint | length == 0)
    and ($compat_checkpoint | length == 0)
    and ($state_data[0].source != $state_checkpoint[0].source)
    and (resolved_volume_name($state_data[0].source) != resolved_volume_name($state_checkpoint[0].source))
    and ($state_checkpoint[0].source == $ingest_checkpoint[0].source)
    and ($state_checkpoint[0].source == $projector_checkpoint[0].source)
    and ($services["zinder-ingest"].depends_on["state-init"].condition == "service_completed_successfully")
    and ($services["zinder-projector"].network_mode == "service:zinder-ingest")
    and ($services["zinder-query"].network_mode == "service:zinder-ingest")
    and ($services["zinder-compat-lightwalletd"].network_mode == "service:zinder-ingest")
    and (($services["zinder-ingest"].ports // []) | length == 6)
    and (($services["zinder-ingest"].ports // [])
      | all(.host_ip == "127.0.0.1" and .protocol == "tcp" and .mode == "ingress"))
    and (($services["zinder-ingest"].ports | map(.published) | unique | length) == 6)
    and (($services["zinder-ingest"].ports | map(.target) | sort) == [9067, 9102, 9105, 9106, 9107, 9110])
    and ([ $services[] | (.ports // [])[] ] | all(.host_ip == "127.0.0.1"))
    and ([ $services[] | (.ports // [])[] | .target ] | index(9100) == null)
    and (($services["zinder-ingest"].volumes // [])
      | any(.target == "/var/run/zinder-checkpoint/checkpoint.token" and .read_only == true))
    and (($services["zinder-projector"].volumes // [])
      | any(.target == "/var/run/zinder-checkpoint/checkpoint.token" and .read_only == true))
    and (($services["zinder-query"].volumes // [])
      | all(.target != "/var/run/zinder-checkpoint/checkpoint.token"))
    and (($services["zinder-compat-lightwalletd"].volumes // [])
      | all(.target != "/var/run/zinder-checkpoint/checkpoint.token"))
    and (has_checkpoint_secret_source("zinder-query") | not)
    and (has_checkpoint_secret_source("zinder-compat-lightwalletd") | not)
    and exact_read_only_bind("zinder-query"; "/etc/zinder/config.toml"; "/deploy/config/query.toml")
    and exact_read_only_bind("zinder-compat-lightwalletd"; "/etc/zinder/config.toml"; "/deploy/config/compat-lightwalletd.toml")
    and exact_read_only_bind("zinder-query"; "/var/run/zinder-control/ingest.token"; "/ingest.token")
    and exact_read_only_bind("zinder-compat-lightwalletd"; "/var/run/zinder-control/ingest.token"; "/ingest.token")
    and ($services["zinder-ingest"].healthcheck | exact_healthcheck("http://localhost:9105/readyz"))
    and ($services["zinder-projector"].healthcheck | exact_healthcheck("http://localhost:9110/readyz"))
    and ($services["zinder-query"].healthcheck | exact_healthcheck("http://localhost:9106/readyz"))
    and ($services["zinder-compat-lightwalletd"].healthcheck | exact_healthcheck("http://localhost:9107/readyz"))
    and ($services["zinder-projector"].depends_on["zinder-ingest"].condition == "service_healthy")
    and ($services["zinder-compat-lightwalletd"].depends_on["zinder-ingest"].condition == "service_healthy")
    and ($services["zinder-compat-lightwalletd"].depends_on["zinder-projector"].condition == "service_healthy")
    and ($services["zinder-query"].depends_on["zinder-ingest"].condition == "service_healthy")
    and ($services["zinder-query"].depends_on["zinder-projector"].condition == "service_healthy")
    and ($services["zinder-ingest"].environment.ZINDER_OPS__LISTEN_ADDR == "[::]:9105")
    and ($services["zinder-ingest"].environment.ZINDER_SECURITY__ALLOW_PUBLIC_BIND == "true")
    and ($services["zinder-projector"].environment.ZINDER_OPS__LISTEN_ADDR == "[::]:9110")
    and ($services["zinder-projector"].environment.ZINDER_SECURITY__ALLOW_PUBLIC_BIND == "true")
    and ($services["zinder-query"].environment.ZINDER_QUERY__LISTEN_ADDR == "[::]:9102")
    and ($services["zinder-query"].environment.ZINDER_OPS__LISTEN_ADDR == "[::]:9106")
    and ($services["zinder-query"].environment.ZINDER_SECURITY__ALLOW_PUBLIC_BIND == "true")
    and ($services["zinder-compat-lightwalletd"].environment.ZINDER_COMPAT__LISTEN_ADDR == "[::]:9067")
    and ($services["zinder-compat-lightwalletd"].environment.ZINDER_OPS__LISTEN_ADDR == "[::]:9107")
    and ($services["zinder-compat-lightwalletd"].environment.ZINDER_SECURITY__ALLOW_PUBLIC_BIND == "true")
  ' "$compose_contract" >/dev/null || {
    cat >&2 <<'EOF'
release admission rejected: resolved Compose contract does not preserve the
root-owned data and isolated checkpoint-volume initialization, four-runtime
shared namespace, exact readiness probes, explicit container listener opt-ins,
loopback-only host publications, private control port, distinct native and
compatibility readers, or the ingest/projector-only checkpoint capability and staging mounts.
EOF
    exit 1
  }

  toml_section_value() {
    local config_path="$1"
    local section="$2"
    local key="$3"
    awk -v heading="[$section]" -v key="$key" '
      $0 == heading { in_section = 1; next }
      /^\[/ { in_section = 0 }
      in_section && $1 == key {
        value = $0
        sub(/^[^=]*=[[:space:]]*"/, "", value)
        sub(/".*$/, "", value)
        print value
        exit
      }
    ' "$config_path"
  }

  query_config_source="$(jq -r '
    .services["zinder-query"].volumes[]
    | select(.target == "/etc/zinder/config.toml")
    | .source
  ' "$compose_contract")"
  compat_config_source="$(jq -r '
    .services["zinder-compat-lightwalletd"].volumes[]
    | select(.target == "/etc/zinder/config.toml")
    | .source
  ' "$compose_contract")"
  reader_secondary_paths=(
    "$(toml_section_value "$query_config_source" storage secondary_path)"
    "$(toml_section_value "$query_config_source" wallet secondary_path)"
    "$(toml_section_value "$compat_config_source" storage secondary_path)"
    "$(toml_section_value "$compat_config_source" wallet secondary_path)"
  )
  nonempty_reader_secondary_path_count="$(
    printf '%s\n' "${reader_secondary_paths[@]}" | sed '/^$/d' | wc -l
  )"
  unique_reader_secondary_path_count="$(
    printf '%s\n' "${reader_secondary_paths[@]}" | sed '/^$/d' | sort -u | wc -l
  )"
  if [[ "$nonempty_reader_secondary_path_count" -ne 4 \
    || "$unique_reader_secondary_path_count" -ne 4 ]]; then
    cat >&2 <<'EOF'
release admission rejected: native query and compatibility must configure four
distinct canonical and wallet secondary roots.
EOF
    exit 1
  fi
  normalized_reader_secondary_paths=()
  for reader_secondary_path in "${reader_secondary_paths[@]}"; do
    normalized_reader_secondary_paths+=("$(realpath -m -- "$reader_secondary_path")")
  done
  for ((left_index = 0; left_index < ${#normalized_reader_secondary_paths[@]}; left_index++)); do
    for ((right_index = left_index + 1; right_index < ${#normalized_reader_secondary_paths[@]}; right_index++)); do
      left_path="${normalized_reader_secondary_paths[$left_index]}"
      right_path="${normalized_reader_secondary_paths[$right_index]}"
      if [[ "$left_path" == "$right_path" \
        || "$left_path" == "$right_path"/* \
        || "$right_path" == "$left_path"/* ]]; then
        cat >&2 <<'EOF'
release admission rejected: native query and compatibility secondary roots
must be path-disjoint; no reader root may equal or contain another.
EOF
        exit 1
      fi
    done
  done
  exit 0
fi

resolve_railway_default_target() {
  local railway_config="$repository_root/railway.toml"
  local railway_dockerfile

  railway_dockerfile="$(sed -n 's/^dockerfilePath = "\([^"]*\)"$/\1/p' "$railway_config")"
  [[ -n "$railway_dockerfile" ]] || {
    echo "release admission rejected: railway.toml has no Dockerfile path" >&2
    return 1
  }

  sed -n 's/^ARG RAILWAY_DOCKER_TARGET_STAGE=\([A-Za-z0-9_-]*\)$/\1/p' \
    "$repository_root/$railway_dockerfile"
}

if [[ "$verify_railway_default" = true ]]; then
  [[ -z "$deployment_class" && -z "$target" && "$railway_default" = false && -z "$release_workflow" && -z "$build_images_workflow" && "$release_images_catalog_explicit" = false && -z "$prometheus_config" && -z "$compose_contract" ]] || usage

  target="$(resolve_railway_default_target)"
  [[ "$target" = "zinder-admission-required" ]] || {
    echo "release admission rejected: Railway default target must be zinder-admission-required, got ${target:-unset}" >&2
    exit 1
  }
  railway_dockerfile="$(sed -n 's/^dockerfilePath = "\([^"]*\)"$/\1/p' "$repository_root/railway.toml")"
  grep -Fqx 'FROM zinder-canonical-runtime AS zinder-railway-runtime' \
    "$repository_root/$railway_dockerfile" || {
    echo "release admission rejected: Railway final stage is not fixed to zinder-canonical-runtime" >&2
    exit 1
  }
  # The literal shell expression is the admission contract being inspected.
  # shellcheck disable=SC2016
  grep -Fq 'test "${RAILWAY_DOCKER_TARGET_STAGE}" = "zinder-canonical-runtime"' \
    "$repository_root/$railway_dockerfile" || {
    echo "release admission rejected: Railway target argument is not guarded in the image build" >&2
    exit 1
  }
  exit 0
fi

if [[ -n "$build_images_workflow" ]]; then
  [[ -z "$deployment_class" && -z "$target" && "$railway_default" = false && -z "$release_workflow" && -z "$prometheus_config" && "$verify_railway_default" = false && -z "$compose_contract" ]] || usage
  [[ -r "$build_images_workflow" ]] || {
    echo "release admission rejected: cannot read build image workflow $build_images_workflow" >&2
    exit 1
  }

  validate_release_images_catalog
  required_build_platforms="$({
    printf '%s\n' linux/amd64 linux/arm64
  } | sort)"
  configured_build_platforms="$(
    sed -n 's/^[[:space:]]*ref: \(linux\/[a-z0-9]*\)$/\1/p' \
      "$build_images_workflow" | sort -u
  )"
  # These literals are workflow expressions and shell variables being inspected.
  # shellcheck disable=SC2016
  if [[ "$configured_build_platforms" != "$required_build_platforms" ]] \
    || ! grep -Fq 'runner: ubuntu-24.04-arm' "$build_images_workflow" \
    || ! grep -Fq 'needs: verify' "$build_images_workflow" \
    || ! grep -Fq 'release_images: ${{ steps.images.outputs.release_images }}' "$build_images_workflow" \
    || ! grep -Fq 'RELEASE_IMAGES_JSON: ${{ needs.verify.outputs.release_images }}' "$build_images_workflow" \
    || ! grep -Fq "jq -c '.' deploy/release-images.json" "$build_images_workflow" \
    || ! grep -Fq "jq -r '.[]' <<< \"\$RELEASE_IMAGES_JSON\"" "$build_images_workflow" \
    || ! grep -Fq -- '--platform "$PLATFORM_REF"' "$build_images_workflow" \
    || ! grep -Eq 'SMOKE_BUILD_GIT_COMMIT: [0-9a-f]{40}$' "$build_images_workflow" \
    || ! grep -Fq -- '--build-arg "ZINDER_BUILD_GIT_COMMIT=${SMOKE_BUILD_GIT_COMMIT}"' \
      "$build_images_workflow" \
    || ! grep -Fq 'docker run --rm --entrypoint="$image_name" "${image_name}:${PR_TAG}" --help' \
      "$build_images_workflow"; then
    cat >&2 <<'EOF'
release admission rejected: the pull-request image workflow must build and run
the --help smoke for the validated release image catalog on native amd64 and
arm64 runners.
EOF
    exit 1
  fi
  exit 0
fi

if [[ -n "$prometheus_config" ]]; then
  [[ -z "$deployment_class" && -z "$target" && "$railway_default" = false && -z "$release_workflow" && -z "$build_images_workflow" && "$release_images_catalog_explicit" = false && "$verify_railway_default" = false && -z "$compose_contract" ]] || usage
  [[ -r "$prometheus_config" ]] || {
    echo "release admission rejected: cannot read Prometheus config $prometheus_config" >&2
    exit 1
  }

  query_job_count="$(grep -Fc 'job_name: "zinder-query"' "$prometheus_config")"
  query_job="$({
    awk '
      /job_name: "zinder-query"/ { in_query = 1 }
      in_query && /job_name:/ && !/job_name: "zinder-query"/ { exit }
      in_query { print }
    ' "$prometheus_config"
  })"
  if [[ "$query_job_count" -ne 1 ]] \
    || ! grep -Fq 'targets: ["zinder-ingest:9106"]' <<< "$query_job" \
    || ! grep -Fq 'service: "zinder-query"' <<< "$query_job"; then
    cat >&2 <<'EOF'
release admission rejected: deploy Prometheus must scrape the native query ops
endpoint at zinder-ingest:9106 and label the target as zinder-query.
EOF
    exit 1
  fi
  exit 0
fi

if [[ -n "$release_workflow" ]]; then
  [[ -z "$deployment_class" && -z "$target" && "$railway_default" = false && -z "$build_images_workflow" && -z "$prometheus_config" && "$verify_railway_default" = false && -z "$compose_contract" ]] || usage
  [[ -r "$release_workflow" ]] || {
    echo "release admission rejected: cannot read release workflow $release_workflow" >&2
    exit 1
  }
  validate_release_images_catalog

  if grep -Fq 'workflow_dispatch:' "$release_workflow"; then
    cat >&2 <<'EOF'
release admission rejected: the publishing workflow must not expose a manual
dispatch path. Use the pull-request image workflow for build-only smoke tests.
EOF
    exit 1
  fi

  release_before_stable_promotion="$(sed '/^  promote-latest:/,$d' "$release_workflow")"
  if grep -Fq ':latest' <<< "$release_before_stable_promotion"; then
    cat >&2 <<'EOF'
release admission rejected: latest may only move in the final stable promotion
job after exact image manifests and the GitHub Release have succeeded.
EOF
    exit 1
  fi

  authorization_job="$(
    awk '
      $0 == "  authorize:" { in_job = 1; next }
      in_job && /^  [a-zA-Z0-9_-]+:/ { exit }
      in_job { print }
    ' "$release_workflow"
  )"
  if [[ -z "$authorization_job" ]] \
    || ! grep -Fq 'environment: release' <<< "$authorization_job"; then
    cat >&2 <<'EOF'
release admission rejected: publication requires one protected
release-environment authorization gate.
EOF
    exit 1
  fi

  publishing_jobs=(build merge prepare-release publish-release promote-latest)
  required_predecessors=(authorize build merge prepare-release publish-release)
  for index in "${!publishing_jobs[@]}"; do
    publishing_job="${publishing_jobs[$index]}"
    required_predecessor="${required_predecessors[$index]}"
    job_body="$(
      awk -v heading="  ${publishing_job}:" '
        $0 == heading { in_job = 1; next }
        in_job && /^  [a-zA-Z0-9_-]+:/ { exit }
        in_job { print }
      ' "$release_workflow"
    )"
    if [[ -z "$job_body" ]] \
      || ! grep -Eq "^[[:space:]]+- ${required_predecessor}$" <<< "$job_body"; then
      cat >&2 <<EOF
release admission rejected: publishing job $publishing_job must depend on
$required_predecessor so it cannot bypass release authorization.
EOF
      exit 1
    fi
  done

  stable_promotion_job="$(sed -n '/^  promote-latest:/,$p' "$release_workflow")"
  if ! grep -Fq "if: needs.validate.outputs.stable == 'true'" \
    <<< "$stable_promotion_job"; then
    cat >&2 <<'EOF'
release admission rejected: latest promotion must be restricted to a validated
stable SemVer tag.
EOF
    exit 1
  fi

  if grep -Fq 'zinder-single-container' "$release_workflow"; then
    cat >&2 <<'EOF'
release admission rejected: the image workflow would publish the mixed
zinder-single-container bundle. The Railway image set contains only the
ingest-only diagnostic or canary runtime.
EOF
    exit 1
  fi

  if grep -Eq '"zinder-explorer:zinder-explorer"' "$release_workflow"; then
    cat >&2 <<'EOF'
release admission rejected: the release image set does not publish the optional
explorer runtime.
EOF
    exit 1
  fi

  # These literals are workflow expressions and shell variables being inspected.
  # shellcheck disable=SC2016
  if ! grep -Fq 'release_images: ${{ steps.images.outputs.release_images }}' \
    "$release_workflow" \
    || ! grep -Fq "jq -c '.' \"\$RELEASE_IMAGES_FILE\"" "$release_workflow" \
    || ! grep -Fq 'RELEASE_IMAGES_JSON: ${{ needs.validate.outputs.release_images }}' \
      "$release_workflow" \
    || ! grep -Fq "jq -r '.[]' <<< \"\$RELEASE_IMAGES_JSON\"" "$release_workflow" \
    || ! grep -Fq 'image: ${{ fromJSON(needs.validate.outputs.release_images) }}' \
      "$release_workflow"; then
    cat >&2 <<'EOF'
release admission rejected: digest builds, manifest publication, and stable
promotion must consume the validated release image catalog output.
EOF
    exit 1
  fi

  exit 0
fi

[[ "$release_images_catalog_explicit" = false ]] || usage

case "$deployment_class" in
  production|canary|diagnostic) ;;
  *) usage ;;
esac

if [[ "$railway_default" = true ]]; then
  [[ -z "$target" ]] || usage

  target="$(resolve_railway_default_target)"
  [[ -n "$target" ]] || {
    echo "release admission rejected: Railway Dockerfile has no default target stage" >&2
    exit 1
  }
fi

[[ -n "$target" ]] || usage

case "$deployment_class:$target" in
  canary:zinder-canonical-runtime|diagnostic:zinder-canonical-runtime)
    exit 0
    ;;
  production:zinder-single-container)
    cat >&2 <<'EOF'
release admission rejected: zinder-single-container combines canonical
ingest with reader ownership and omits zinder-compat-lightwalletd.
It is not a production candidate.
EOF
    exit 1
    ;;
  production:zinder-canonical-runtime)
    cat >&2 <<'EOF'
release admission rejected: zinder-canonical-runtime is an ingest-only
diagnostic canary. It does not provide the complete topology or a
public compatibility route.
EOF
    exit 1
    ;;
  *:zinder-admission-required)
    cat >&2 <<'EOF'
release admission rejected: Railway requires an explicit admitted deployment
class and target. Only the zinder-canonical-runtime diagnostic/canary target
is admitted.
EOF
    exit 1
    ;;
  production:*)
    cat >&2 <<EOF
release admission rejected: $target is not a certified complete version-1
production topology.
EOF
    exit 1
    ;;
  canary:*|diagnostic:*)
    cat >&2 <<EOF
release admission rejected: $target is not the explicitly admitted
zinder-canonical-runtime diagnostic/canary target.
EOF
    exit 1
    ;;
esac
