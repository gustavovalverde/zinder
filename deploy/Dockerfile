# syntax=docker/dockerfile:1.7

# Canonical Zinder runtime image builder.
#
# Build service images from the repository root with:
#
#     docker build -f deploy/Dockerfile --target zinder-ingest -t zinder-ingest .
#     docker build -f deploy/Dockerfile --target zinder-query -t zinder-query .
#     docker build -f deploy/Dockerfile --target zinder-explorer -t zinder-explorer .
#     docker build -f deploy/Dockerfile --target zinder-compat-lightwalletd -t zinder-compat-lightwalletd .
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

COPY . .

RUN --mount=type=cache,id=zinder-cargo-registry-${RUST_VERSION}-${TARGETARCH}${TARGETVARIANT},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=zinder-cargo-git-${RUST_VERSION}-${TARGETARCH}${TARGETVARIANT},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=zinder-cargo-target-${RUST_VERSION}-${TARGETARCH}${TARGETVARIANT},target=/workspace/target,sharing=locked \
    cargo build --locked --release \
        --bin zinder-ingest \
        --bin zinder-query \
        --bin zinder-explorer \
        --bin zinder-compat-lightwalletd \
    && mkdir -p /zinder-binaries \
    && cp /workspace/target/release/zinder-ingest /zinder-binaries/zinder-ingest \
    && cp /workspace/target/release/zinder-query /zinder-binaries/zinder-query \
    && cp /workspace/target/release/zinder-explorer /zinder-binaries/zinder-explorer \
    && cp /workspace/target/release/zinder-compat-lightwalletd /zinder-binaries/zinder-compat-lightwalletd

FROM debian:${DEBIAN_RELEASE}-slim AS zinder-runtime-base

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system --gid 1000 zinder \
    && useradd --system --uid 1000 --gid zinder --home /var/lib/zinder --shell /usr/sbin/nologin zinder \
    && mkdir -p /var/lib/zinder/store /var/lib/zinder/secondary /etc/zinder \
    && chown -R zinder:zinder /var/lib/zinder /etc/zinder

FROM zinder-runtime-base AS zinder-ingest

COPY --from=zinder-binaries /zinder-binaries/zinder-ingest /usr/local/bin/zinder-ingest

USER zinder:zinder
WORKDIR /var/lib/zinder

ENV ZINDER_OPS__LISTEN_ADDR=0.0.0.0:9105
ENV ZINDER_INGEST_CONTROL__LISTEN_ADDR=0.0.0.0:9100

EXPOSE 9100 9105

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://localhost:9105/readyz || exit 1

ENTRYPOINT ["zinder-ingest"]
CMD ["--config", "/etc/zinder/config.toml"]

FROM zinder-runtime-base AS zinder-query

COPY --from=zinder-binaries /zinder-binaries/zinder-query /usr/local/bin/zinder-query

USER zinder:zinder
WORKDIR /var/lib/zinder

ENV ZINDER_QUERY__LISTEN_ADDR=0.0.0.0:9101
ENV ZINDER_OPS__LISTEN_ADDR=0.0.0.0:9106

EXPOSE 9101 9106

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://localhost:9106/readyz || exit 1

ENTRYPOINT ["zinder-query"]
CMD ["--config", "/etc/zinder/config.toml"]

FROM zinder-runtime-base AS zinder-explorer

COPY --from=zinder-binaries /zinder-binaries/zinder-explorer /usr/local/bin/zinder-explorer

USER zinder:zinder
WORKDIR /var/lib/zinder

ENV ZINDER_EXPLORER__LISTEN_ADDR=0.0.0.0:9068
ENV ZINDER_OPS__LISTEN_ADDR=0.0.0.0:9069

EXPOSE 9068 9069

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://localhost:9069/readyz || exit 1

ENTRYPOINT ["zinder-explorer"]
CMD ["--config", "/etc/zinder/config.toml"]

FROM zinder-runtime-base AS zinder-compat-lightwalletd

COPY --from=zinder-binaries /zinder-binaries/zinder-compat-lightwalletd /usr/local/bin/zinder-compat-lightwalletd

USER zinder:zinder
WORKDIR /var/lib/zinder

ENV ZINDER_COMPAT__LISTEN_ADDR=0.0.0.0:9067
ENV ZINDER_OPS__LISTEN_ADDR=0.0.0.0:9107

EXPOSE 9067 9107

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://localhost:9107/readyz || exit 1

ENTRYPOINT ["zinder-compat-lightwalletd"]
CMD ["--config", "/etc/zinder/config.toml"]

FROM zinder-runtime-base AS zinder-single-container

ARG S6_OVERLAY_VERSION=3.2.0.2
ARG TARGETPLATFORM

RUN apt-get update \
    && apt-get install -y --no-install-recommends netcat-openbsd xz-utils \
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
COPY deploy/single-container/services/user/contents.d /etc/s6-overlay/s6-rc.d/user/contents.d
COPY deploy/single-container/config.example.ingest.toml /etc/zinder/config.example.ingest.toml
COPY deploy/single-container/config.example.query.toml /etc/zinder/config.example.query.toml
COPY deploy/single-container/config.example.explorer.toml /etc/zinder/config.example.explorer.toml

RUN cp /etc/zinder/config.example.ingest.toml /etc/zinder/ingest.toml \
    && cp /etc/zinder/config.example.query.toml /etc/zinder/query.toml \
    && cp /etc/zinder/config.example.explorer.toml /etc/zinder/explorer.toml \
    && chown zinder:zinder /etc/zinder/ingest.toml /etc/zinder/query.toml /etc/zinder/explorer.toml \
    && chmod +x /etc/s6-overlay/s6-rc.d/zinder-ingest/run \
                /etc/s6-overlay/s6-rc.d/zinder-ingest/finish \
                /etc/s6-overlay/s6-rc.d/zinder-query/run \
                /etc/s6-overlay/s6-rc.d/zinder-query/finish \
                /etc/s6-overlay/s6-rc.d/zinder-explorer/run \
                /etc/s6-overlay/s6-rc.d/zinder-explorer/finish

ENV S6_KEEP_ENV=1

EXPOSE 9101 9106 9068 9069

HEALTHCHECK --interval=30s --timeout=5s --start-period=90s --retries=3 \
    CMD curl -fsS http://localhost:9106/readyz || exit 1

ENTRYPOINT ["/init"]
