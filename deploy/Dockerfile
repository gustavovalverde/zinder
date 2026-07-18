# syntax=docker/dockerfile:1.7

# Canonical Zinder runtime image builder.
#
# Build service images from the repository root with:
#
#     docker build -f deploy/Dockerfile --target zinder-ingest -t zinder-ingest .
#     docker build -f deploy/Dockerfile --target zinder-projector -t zinder-projector .
#     docker build -f deploy/Dockerfile --target zinder-compat-lightwalletd -t zinder-compat-lightwalletd .
#     docker build -f deploy/Dockerfile --target zinder-bench -t zinder-bench .
#     docker build -f deploy/Dockerfile --target zinder-ingest-live-tests -t zinder-ingest-live-tests .
#
# Every runtime target depends on the same `zinder-binaries` stage. That stage
# compiles all shipped binaries in one Cargo invocation and keeps Cargo registry,
# git, and target artifacts in BuildKit cache mounts. Rebuilding one service
# warms the shared cache for every other service image.

ARG RUST_VERSION=1.95
ARG DEBIAN_RELEASE=bookworm

FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS zinder-build-base

ARG PROTOC_VERSION=29.3

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        clang \
        cmake \
        curl \
        libclang-dev \
        pkg-config \
        unzip \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSLo /tmp/protoc.zip "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-$(uname -m | sed 's/aarch64/aarch_64/').zip" \
    && unzip -q /tmp/protoc.zip -d /usr/local \
    && rm -f /tmp/protoc.zip

ENV CARGO_TARGET_DIR=/workspace/target
WORKDIR /workspace

# Keep rustup's toolchain/component hydration in a source-independent layer.
# `cargo build` reads rust-toolchain.toml, and if this file is first seen after
# `COPY . .`, every source-change rebuild can pay for component synchronization.
COPY rust-toolchain.toml ./
RUN rustup show active-toolchain >/dev/null

FROM zinder-build-base AS zinder-binaries

ARG RUST_VERSION
ARG TARGETARCH
ARG TARGETVARIANT

# x86-64-v3 also reaches the vendored RocksDB C++ (hardware CRC32c, AVX2);
# scoping to the target triple keeps build scripts and non-x86 images on the
# baseline ISA. Runtime hosts are verified at startup before storage opens.
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+pclmulqdq"

COPY . .

# A persisted target cache can contain prost output newer than Docker's copied
# source mtimes. Clean only this package so every image regenerates native
# contracts while retaining dependency and downstream compilation caches.
RUN --mount=type=cache,id=zinder-cargo-registry-${RUST_VERSION}-${TARGETARCH}${TARGETVARIANT},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=zinder-cargo-git-${RUST_VERSION}-${TARGETARCH}${TARGETVARIANT},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=zinder-cargo-target-${RUST_VERSION}-${TARGETARCH}${TARGETVARIANT},target=/workspace/target,sharing=locked \
    cargo clean --release -p zinder-proto \
    && cargo build --locked --release \
        --bin zinder-ingest \
        --bin zinder-projector \
        --bin zinder-compat-lightwalletd \
        --bin zinder-bench \
    && mkdir -p /zinder-binaries \
    && cp /workspace/target/release/zinder-ingest /zinder-binaries/zinder-ingest \
    && cp /workspace/target/release/zinder-projector /zinder-binaries/zinder-projector \
    && cp /workspace/target/release/zinder-compat-lightwalletd /zinder-binaries/zinder-compat-lightwalletd \
    && cp /workspace/target/release/zinder-bench /zinder-binaries/zinder-bench

FROM zinder-build-base AS zinder-ingest-live-test-binary

ARG RUST_VERSION
ARG TARGETARCH
ARG TARGETVARIANT

ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+pclmulqdq"

COPY . .

RUN --mount=type=cache,id=zinder-cargo-registry-${RUST_VERSION}-${TARGETARCH}${TARGETVARIANT},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=zinder-cargo-git-${RUST_VERSION}-${TARGETARCH}${TARGETVARIANT},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=zinder-cargo-target-${RUST_VERSION}-${TARGETARCH}${TARGETVARIANT},target=/workspace/target,sharing=locked \
    cargo test --locked --release -p zinder-ingest --test acceptance --no-run \
    && mkdir -p /zinder-live-tests \
    && test_binary="$(find /workspace/target/release/deps -maxdepth 1 -type f -name 'acceptance-*' -perm /111 -printf '%T@ %p\n' | sort -nr | head -n 1 | cut -d' ' -f 2-)" \
    && test -n "$test_binary" \
    && cp "$test_binary" /zinder-live-tests/zinder-ingest-acceptance

FROM debian:${DEBIAN_RELEASE}-slim AS zinder-runtime-base

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system --gid 1000 zinder \
    && useradd --system --uid 1000 --gid zinder --home /var/lib/zinder --shell /usr/sbin/nologin zinder \
    && mkdir -p /var/lib/zinder/secondary /etc/zinder \
    && chown -R zinder:zinder /var/lib/zinder /etc/zinder

FROM zinder-runtime-base AS zinder-ingest

COPY --from=zinder-binaries /zinder-binaries/zinder-ingest /usr/local/bin/zinder-ingest

USER zinder:zinder
WORKDIR /var/lib/zinder

EXPOSE 9105

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://localhost:9105/healthz || exit 1

ENTRYPOINT ["zinder-ingest"]

FROM zinder-runtime-base AS zinder-projector

COPY --from=zinder-binaries /zinder-binaries/zinder-projector /usr/local/bin/zinder-projector

USER zinder:zinder
WORKDIR /var/lib/zinder

EXPOSE 9110

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://localhost:9110/readyz || exit 1

ENTRYPOINT ["zinder-projector"]

FROM zinder-runtime-base AS zinder-ingest-live-tests

COPY --from=zinder-ingest-live-test-binary /zinder-live-tests/zinder-ingest-acceptance /usr/local/bin/zinder-ingest-acceptance

USER zinder:zinder
WORKDIR /var/lib/zinder

ENTRYPOINT ["zinder-ingest-acceptance"]

FROM zinder-runtime-base AS zinder-compat-lightwalletd

COPY --from=zinder-binaries /zinder-binaries/zinder-compat-lightwalletd /usr/local/bin/zinder-compat-lightwalletd

USER zinder:zinder
WORKDIR /var/lib/zinder

EXPOSE 9067 9107

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://localhost:9107/readyz || exit 1

ENTRYPOINT ["zinder-compat-lightwalletd"]

FROM zinder-runtime-base AS zinder-bench

COPY --from=zinder-binaries /zinder-binaries/zinder-bench /usr/local/bin/zinder-bench
COPY --chmod=0755 deploy/capture-container-resource-evidence.sh /usr/local/bin/zinder-capture-container-resource-evidence

USER zinder:zinder
WORKDIR /var/lib/zinder

ENTRYPOINT ["zinder-capture-container-resource-evidence", "zinder-bench"]
