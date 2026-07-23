#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
packages=(zinder-core zinder-proto zinder-client)
version="$(
  cargo metadata --format-version 1 --no-deps --manifest-path "$repository_root/Cargo.toml" \
    | jq -r '.packages[] | select(.name == "zinder-core") | .version'
)"

cd "$repository_root"
scripts/check-sdk-package-policy.sh

for package_name in "${packages[@]}"; do
  package_config=()
  if [[ "$package_name" == zinder-proto || "$package_name" == zinder-client ]]; then
    package_config+=(
      --config
      'patch.crates-io.zinder-core.path="crates/zinder-core"'
    )
  fi
  if [[ "$package_name" == zinder-client ]]; then
    package_config+=(
      --config
      'patch.crates-io.zinder-proto.path="crates/zinder-proto"'
    )
  fi
  package_files="$(
    cargo package --allow-dirty --list -p "$package_name" "${package_config[@]}"
  )"
  grep -Fqx 'Cargo.toml' <<< "$package_files"
  grep -Fqx 'README.md' <<< "$package_files"
  if grep -Eq '(^|/)tests/' <<< "$package_files"; then
    echo >&2 "$package_name package contains workspace-only tests"
    exit 1
  fi
  cargo package \
    --allow-dirty \
    --no-verify \
    -p "$package_name" \
    "${package_config[@]}"
done

for package_name in "${packages[@]}"; do
  CARGO_INCREMENTAL=0 cargo check --locked -p "$package_name"
  CARGO_INCREMENTAL=0 cargo check --locked -p "$package_name" --no-default-features
  CARGO_INCREMENTAL=0 cargo check --locked -p "$package_name" --all-features
done

PROTOC=/does/not/exist CARGO_INCREMENTAL=0 cargo check --locked -p zinder-proto
RUSTDOCFLAGS='-D warnings' CARGO_INCREMENTAL=0 \
  cargo doc --locked --no-deps --all-features \
    -p zinder-core \
    -p zinder-proto \
    -p zinder-client

extraction_root="$(mktemp -d /tmp/zinder-sdk-package-consumer.XXXXXX)"
trap 'rm -rf "$extraction_root"' EXIT
for package_name in "${packages[@]}"; do
  tar -xzf "$repository_root/target/package/${package_name}-${version}.crate" \
    -C "$extraction_root"
done

consumer_dir="$extraction_root/consumer"
mkdir -p "$consumer_dir/src"
cat > "$consumer_dir/Cargo.toml" <<TOML
[package]
name = "zinder-sdk-package-consumer"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
zinder-client = "=${version}"

[patch.crates-io]
zinder-core = { path = "$extraction_root/zinder-core-${version}" }
zinder-proto = { path = "$extraction_root/zinder-proto-${version}" }
zinder-client = { path = "$extraction_root/zinder-client-${version}" }
TOML
cat > "$consumer_dir/src/lib.rs" <<'RUST'
use zinder_client::{
    Capability, CapabilityDescriptor, ErrorReason, ServerInfo,
};

pub fn supports_full_blocks(server: &ServerInfo) -> bool {
    server.supports(Capability::FullBlock)
}

pub fn preserve_future_reason(reason: &str) -> ErrorReason {
    ErrorReason::from_wire_name(reason)
}
RUST

PROTOC=/does/not/exist CARGO_INCREMENTAL=0 \
  cargo check --manifest-path "$consumer_dir/Cargo.toml"
