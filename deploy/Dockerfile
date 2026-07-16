# syntax=docker/dockerfile:1.7

# Canonical Zinder runtime image builder.
#
# Build service images from the repository root with:
#
#     docker build -f deploy/Dockerfile --target zinder-ingest -t zinder-ingest .
#     docker build -f deploy/Dockerfile --target zinder-query -t zinder-query .
#     docker build -f deploy/Dockerfile --target zinder-explorer -t zinder-explorer .
#     docker build -f deploy/Dockerfile --target zinder-compat-lightwalletd -t zinder-compat-lightwalletd .
#     docker build -f deploy/Dockerfile --target zinder-bench -t zinder-bench .
#     docker build -f deploy/Dockerfile --target zinder-ingest-live-tests -t zinder-ingest-live-tests .
#     docker build -f deploy/Dockerfile --target zinder-single-container -t zinder .
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
        --bin zinder-query \
        --bin zinder-explorer \
        --bin zinder-compat-lightwalletd \
        --bin zinder-bench \
    && mkdir -p /zinder-binaries \
    && cp /workspace/target/release/zinder-ingest /zinder-binaries/zinder-ingest \
    && cp /workspace/target/release/zinder-query /zinder-binaries/zinder-query \
    && cp /workspace/target/release/zinder-explorer /zinder-binaries/zinder-explorer \
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

ENV ZINDER_OPS__LISTEN_ADDR=[::]:9105
ENV ZINDER_SECURITY__ALLOW_PUBLIC_BIND=true

EXPOSE 9105

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://localhost:9105/healthz || exit 1

ENTRYPOINT ["zinder-ingest"]

FROM zinder-runtime-base AS zinder-ingest-live-tests

COPY --from=zinder-ingest-live-test-binary /zinder-live-tests/zinder-ingest-acceptance /usr/local/bin/zinder-ingest-acceptance

USER zinder:zinder
WORKDIR /var/lib/zinder

ENTRYPOINT ["zinder-ingest-acceptance"]

FROM zinder-runtime-base AS zinder-query

COPY --from=zinder-binaries /zinder-binaries/zinder-query /usr/local/bin/zinder-query

USER zinder:zinder
WORKDIR /var/lib/zinder

ENV ZINDER_QUERY__LISTEN_ADDR=[::]:9101
ENV ZINDER_OPS__LISTEN_ADDR=[::]:9106
ENV ZINDER_SECURITY__ALLOW_PUBLIC_BIND=true

EXPOSE 9101 9106

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://localhost:9106/readyz || exit 1

ENTRYPOINT ["zinder-query"]

FROM zinder-runtime-base AS zinder-explorer

COPY --from=zinder-binaries /zinder-binaries/zinder-explorer /usr/local/bin/zinder-explorer

USER zinder:zinder
WORKDIR /var/lib/zinder

ENV ZINDER_EXPLORER__LISTEN_ADDR=[::]:9068
ENV ZINDER_OPS__LISTEN_ADDR=[::]:9069
ENV ZINDER_SECURITY__ALLOW_PUBLIC_BIND=true

EXPOSE 9068 9069

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://localhost:9069/readyz || exit 1

ENTRYPOINT ["zinder-explorer"]

FROM zinder-runtime-base AS zinder-compat-lightwalletd

COPY --from=zinder-binaries /zinder-binaries/zinder-compat-lightwalletd /usr/local/bin/zinder-compat-lightwalletd

USER zinder:zinder
WORKDIR /var/lib/zinder

ENV ZINDER_COMPAT__LISTEN_ADDR=[::]:9067
ENV ZINDER_OPS__LISTEN_ADDR=[::]:9107
ENV ZINDER_SECURITY__ALLOW_PUBLIC_BIND=true

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

FROM zinder-runtime-base AS zinder-single-container

ARG S6_OVERLAY_VERSION=3.2.0.2
ARG TARGETPLATFORM

RUN apt-get update \
    && apt-get install -y --no-install-recommends netcat-openbsd xz-utils nginx \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    case "${TARGETPLATFORM:-linux/amd64}" in \
        linux/amd64)  S6_ARCH=x86_64 ;; \
        linux/arm64)  S6_ARCH=aarch64 ;; \
        linux/arm/v7) S6_ARCH=arm ;; \
        *) echo "unsupported TARGETPLATFORM: ${TARGETPLATFORM:-unset}" >&2; exit 1 ;; \
    esac; \
    curl -fsSLo /tmp/s6-noarch.tar.xz "https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz"; \
    curl -fsSLo /tmp/s6-arch.tar.xz "https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-${S6_ARCH}.tar.xz"; \
    tar -C / -Jxpf /tmp/s6-noarch.tar.xz; \
    tar -C / -Jxpf /tmp/s6-arch.tar.xz; \
    rm -f /tmp/s6-noarch.tar.xz /tmp/s6-arch.tar.xz

COPY --from=zinder-binaries /zinder-binaries/zinder-ingest /usr/local/bin/zinder-ingest
COPY --from=zinder-binaries /zinder-binaries/zinder-query /usr/local/bin/zinder-query
COPY --from=zinder-binaries /zinder-binaries/zinder-explorer /usr/local/bin/zinder-explorer

COPY deploy/single-container/services/zinder-ingest/type /etc/s6-overlay/s6-rc.d/zinder-ingest/type
COPY deploy/single-container/services/zinder-ingest/run /etc/s6-overlay/s6-rc.d/zinder-ingest/run
COPY deploy/single-container/services/zinder-ingest/finish /etc/s6-overlay/s6-rc.d/zinder-ingest/finish
COPY deploy/single-container/services/zinder-query/type /etc/s6-overlay/s6-rc.d/zinder-query/type
COPY deploy/single-container/services/zinder-query/run /etc/s6-overlay/s6-rc.d/zinder-query/run
COPY deploy/single-container/services/zinder-query/finish /etc/s6-overlay/s6-rc.d/zinder-query/finish
COPY deploy/single-container/services/zinder-query/dependencies.d /etc/s6-overlay/s6-rc.d/zinder-query/dependencies.d
COPY deploy/single-container/services/zinder-explorer/type /etc/s6-overlay/s6-rc.d/zinder-explorer/type
COPY deploy/single-container/services/zinder-explorer/run /etc/s6-overlay/s6-rc.d/zinder-explorer/run
COPY deploy/single-container/services/zinder-explorer/finish /etc/s6-overlay/s6-rc.d/zinder-explorer/finish
COPY deploy/single-container/services/zinder-explorer/dependencies.d /etc/s6-overlay/s6-rc.d/zinder-explorer/dependencies.d
COPY deploy/single-container/services/zinder-multiplexer/type /etc/s6-overlay/s6-rc.d/zinder-multiplexer/type
COPY deploy/single-container/services/zinder-multiplexer/run /etc/s6-overlay/s6-rc.d/zinder-multiplexer/run
COPY deploy/single-container/services/zinder-multiplexer/finish /etc/s6-overlay/s6-rc.d/zinder-multiplexer/finish
COPY deploy/single-container/services/zinder-multiplexer/dependencies.d /etc/s6-overlay/s6-rc.d/zinder-multiplexer/dependencies.d
COPY deploy/single-container/services/user/contents.d /etc/s6-overlay/s6-rc.d/user/contents.d
COPY deploy/single-container/nginx.conf /etc/nginx/nginx.conf

RUN chmod +x /etc/s6-overlay/s6-rc.d/zinder-ingest/run \
             /etc/s6-overlay/s6-rc.d/zinder-ingest/finish \
             /etc/s6-overlay/s6-rc.d/zinder-query/run \
             /etc/s6-overlay/s6-rc.d/zinder-query/finish \
             /etc/s6-overlay/s6-rc.d/zinder-explorer/run \
             /etc/s6-overlay/s6-rc.d/zinder-explorer/finish \
             /etc/s6-overlay/s6-rc.d/zinder-multiplexer/run \
             /etc/s6-overlay/s6-rc.d/zinder-multiplexer/finish

# Single-container deployment shape: every binary loads its configuration
# from `ZINDER_*` env vars + `with_default` baselines compiled into the
# binary. No `/etc/zinder/*.toml` files exist in this image, no `--config`
# flag is passed by the s6 run scripts. Operators override individual
# fields by setting `ZINDER_<SECTION>__<KEY>` on the host (or equivalent
# PaaS env), matching 12-factor convention.
#
# Listen addresses bind on `[::]` (IPv6 wildcard) so the listeners accept
# both IPv6 and IPv4-mapped connections. Railway's legacy private network
# resolves `*.railway.internal` to IPv6-only addresses, so an IPv4-only
# bind (`0.0.0.0`) is unreachable from sibling services. The non-single-
# container per-binary stages above set the same vars per-stage; this
# duplication is deliberate so each deployment shape stays self-contained.
ENV S6_KEEP_ENV=1 \
    ZINDER_INGEST_CONTROL__LISTEN_ADDR=[::]:9100 \
    ZINDER_QUERY__LISTEN_ADDR=[::]:9101 \
    ZINDER_EXPLORER__LISTEN_ADDR=[::]:9068 \
    ZINDER_SECURITY__ALLOW_PUBLIC_BIND=true

EXPOSE 9068 9069 9099 9100 9101 9106

HEALTHCHECK --interval=30s --timeout=5s --start-period=90s --retries=3 \
    CMD curl -fsS http://localhost:9106/readyz || exit 1

ENTRYPOINT ["/init"]
