#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage:
  validate-deployment-admission.sh --deployment-class CLASS --target TARGET
  validate-deployment-admission.sh --deployment-class CLASS --railway-default
  validate-deployment-admission.sh --release-images-workflow PATH
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
release_images_workflow=""
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
    --release-images-workflow)
      [[ $# -ge 2 ]] || usage
      release_images_workflow="$2"
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

if [[ -n "$compose_contract" ]]; then
  [[ -z "$deployment_class" && -z "$target" && "$railway_default" = false && -z "$release_images_workflow" && "$verify_railway_default" = false ]] || usage
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
    volume_mounts_at("state-init"; "/var/lib/zinder") as $state_data
    | volume_mounts_at("state-init"; "/var/lib/zinder/checkpoints") as $state_checkpoint
    | volume_mounts_at("zinder-ingest"; "/var/lib/zinder/checkpoints") as $ingest_checkpoint
    | volume_mounts_at("zinder-projector"; "/var/lib/zinder/checkpoints") as $projector_checkpoint
    | volume_mounts_at("zinder-compat-lightwalletd"; "/var/lib/zinder/checkpoints") as $compat_checkpoint
    | ($services | has("state-init"))
    and ($services | has("zinder-ingest"))
    and ($services | has("zinder-projector"))
    and ($services | has("zinder-compat-lightwalletd"))
    and ($services["state-init"].user == "0:0")
    and ($services["state-init"].network_mode == "none")
    and ($services["state-init"].restart == "no")
    and ($services["state-init"].command == [
      "sh",
      "-c",
      "chown -R 1000:1000 /var/lib/zinder /var/lib/zinder/checkpoints"
    ])
    and ($state_data | length == 1)
    and ($state_checkpoint | length == 1)
    and ($ingest_checkpoint | length == 1)
    and ($projector_checkpoint | length == 1)
    and ($compat_checkpoint | length == 0)
    and ($state_data[0].source != $state_checkpoint[0].source)
    and (resolved_volume_name($state_data[0].source) != resolved_volume_name($state_checkpoint[0].source))
    and ($state_checkpoint[0].source == $ingest_checkpoint[0].source)
    and ($state_checkpoint[0].source == $projector_checkpoint[0].source)
    and ($services["zinder-ingest"].depends_on["state-init"].condition == "service_completed_successfully")
    and ($services["zinder-projector"].network_mode == "service:zinder-ingest")
    and ($services["zinder-compat-lightwalletd"].network_mode == "service:zinder-ingest")
    and (($services["zinder-ingest"].ports // []) | length == 4)
    and (($services["zinder-ingest"].ports // [])
      | all(.host_ip == "127.0.0.1" and .protocol == "tcp" and .mode == "ingress"))
    and (($services["zinder-ingest"].ports | map(.target) | sort) == [9067, 9105, 9107, 9110])
    and ([ $services[] | (.ports // [])[] ] | all(.host_ip == "127.0.0.1"))
    and ([ $services[] | (.ports // [])[] | .target ] | index(9100) == null)
    and (($services["zinder-ingest"].volumes // [])
      | any(.target == "/var/run/zinder-checkpoint/checkpoint.token" and .read_only == true))
    and (($services["zinder-projector"].volumes // [])
      | any(.target == "/var/run/zinder-checkpoint/checkpoint.token" and .read_only == true))
    and (($services["zinder-compat-lightwalletd"].volumes // [])
      | all(.target != "/var/run/zinder-checkpoint/checkpoint.token"))
    and ($services["zinder-ingest"].healthcheck | exact_healthcheck("http://localhost:9105/readyz"))
    and ($services["zinder-projector"].healthcheck | exact_healthcheck("http://localhost:9110/readyz"))
    and ($services["zinder-compat-lightwalletd"].healthcheck | exact_healthcheck("http://localhost:9107/readyz"))
    and ($services["zinder-projector"].depends_on["zinder-ingest"].condition == "service_healthy")
    and ($services["zinder-compat-lightwalletd"].depends_on["zinder-ingest"].condition == "service_healthy")
    and ($services["zinder-compat-lightwalletd"].depends_on["zinder-projector"].condition == "service_healthy")
    and ($services["zinder-ingest"].environment.ZINDER_OPS__LISTEN_ADDR == "[::]:9105")
    and ($services["zinder-ingest"].environment.ZINDER_SECURITY__ALLOW_PUBLIC_BIND == "true")
    and ($services["zinder-projector"].environment.ZINDER_OPS__LISTEN_ADDR == "[::]:9110")
    and ($services["zinder-projector"].environment.ZINDER_SECURITY__ALLOW_PUBLIC_BIND == "true")
    and ($services["zinder-compat-lightwalletd"].environment.ZINDER_COMPAT__LISTEN_ADDR == "[::]:9067")
    and ($services["zinder-compat-lightwalletd"].environment.ZINDER_OPS__LISTEN_ADDR == "[::]:9107")
    and ($services["zinder-compat-lightwalletd"].environment.ZINDER_SECURITY__ALLOW_PUBLIC_BIND == "true")
  ' "$compose_contract" >/dev/null || {
    cat >&2 <<'EOF'
release admission rejected: resolved Compose contract does not preserve the
root-owned data and isolated checkpoint-volume initialization, three-runtime
shared namespace, exact readiness probes, explicit container listener opt-ins,
loopback-only host publications, private control port, or the
ingest/projector-only checkpoint capability and staging mounts.
EOF
    exit 1
  }
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
  [[ -z "$deployment_class" && -z "$target" && "$railway_default" = false && -z "$release_images_workflow" && -z "$compose_contract" ]] || usage

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

if [[ -n "$release_images_workflow" ]]; then
  [[ -z "$deployment_class" && -z "$target" && "$railway_default" = false ]] || usage
  [[ -r "$release_images_workflow" ]] || {
    echo "release admission rejected: cannot read image workflow $release_images_workflow" >&2
    exit 1
  }

  if grep -Fq 'zinder-single-container' "$release_images_workflow"; then
    cat >&2 <<'EOF'
release admission rejected: the image workflow would publish the mixed
zinder-single-container bundle. Phase 7 must supply and certify a complete
version-1 topology before a bundled production image can be published.
EOF
    exit 1
  fi

  if grep -Eq '"zinder-(query|explorer):zinder-(query|explorer)"' "$release_images_workflow"; then
    cat >&2 <<'EOF'
release admission rejected: the image workflow would publish a superseded
query or explorer ownership runtime. Those images return only after their
separate fact-first secondary migrations pass parity and deletion gates.
EOF
    exit 1
  fi

  if ! grep -Fq '"zinder-projector:zinder-projector"' "$release_images_workflow"; then
    cat >&2 <<'EOF'
release admission rejected: the image workflow omits the independent
zinder-projector runtime required to construct and continuously follow the
fact-first wallet projection.
EOF
    exit 1
  fi

  exit 0
fi

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
release admission rejected: zinder-single-container combines canonical-v1
ingest with legacy reader ownership and omits zinder-compat-lightwalletd.
It is not a production candidate.
EOF
    exit 1
    ;;
  production:zinder-canonical-runtime)
    cat >&2 <<'EOF'
release admission rejected: zinder-canonical-runtime is an ingest-only
diagnostic canary. It does not provide the complete version-1 topology or a
public compatibility route.
EOF
    exit 1
    ;;
  *:zinder-admission-required)
    cat >&2 <<'EOF'
release admission rejected: Railway requires an explicit admitted deployment
class and target. Only the zinder-canonical-runtime diagnostic/canary target
is admitted before Phase 7.
EOF
    exit 1
    ;;
  production:*)
    cat >&2 <<EOF
release admission rejected: $target is not a certified complete version-1
production topology. Phase 7 admission is still required.
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
