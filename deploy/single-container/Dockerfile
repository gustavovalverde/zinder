# Single-container Zinder image: zinder-ingest (writer) + zinder-query
# (public reader) supervised by s6-overlay, sharing one canonical volume.
#
# This is the recommended deployment shape for single-operator self-hosting
# (PRD-0002 REQ-2 and ADR-0007 §Single-host topology). Operators on PaaS
# targets that lack shared filesystems across services (Railway, Fly.io,
# Render) deploy this image once; Kubernetes operators wanting sidecar
# separation use the per-service Dockerfiles in services/<svc>/Dockerfile
# instead.

ARG RUST_VERSION=1.91
ARG DEBIAN_RELEASE=bookworm

FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS builder

ARG PROTOC_VERSION=29.3
# `libclang-dev` is required by `zstd-sys`'s build script (transitive through
# RocksDB via `bindgen`). `cmake` and `pkg-config` cover the C dependencies
# that the rocksdb crate compiles on first build.
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

WORKDIR /workspace
COPY . .
RUN cargo build --release --bin zinder-ingest --bin zinder-query


FROM debian:${DEBIAN_RELEASE}-slim AS runtime

# s6-overlay version pinned; bump deliberately. Architecture map covers
# common runner targets (x86_64, aarch64).
ARG S6_OVERLAY_VERSION=3.2.0.2
ARG TARGETPLATFORM

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        netcat-openbsd \
        xz-utils \
    && rm -rf /var/lib/apt/lists/*

# Install s6-overlay. The release ships separate tarballs per arch plus a
# noarch "base" tarball; both must be unpacked into / as the s6 entrypoint
# expects.
RUN set -eux; \
    case "${TARGETPLATFORM:-linux/amd64}" in \
        linux/amd64)  S6_ARCH=x86_64 ;; \
        linux/arm64)  S6_ARCH=aarch64 ;; \
        linux/arm/v7) S6_ARCH=arm ;; \
        *) echo "unsupported TARGETPLATFORM: ${TARGETPLATFORM:-unset}" >&2; exit 1 ;; \
    esac; \
    curl -fsSLo /tmp/s6-noarch.tar.xz "https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz"; \
    curl -fsSLo /tmp/s6-arch.tar.xz   "https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-${S6_ARCH}.tar.xz"; \
    tar -C / -Jxpf /tmp/s6-noarch.tar.xz; \
    tar -C / -Jxpf /tmp/s6-arch.tar.xz; \
    rm -f /tmp/s6-noarch.tar.xz /tmp/s6-arch.tar.xz

RUN groupadd --system --gid 1000 zinder \
    && useradd --system --uid 1000 --gid zinder --home /var/lib/zinder --shell /usr/sbin/nologin zinder \
    && mkdir -p /var/lib/zinder/store /var/lib/zinder/secondary /etc/zinder \
    && chown -R zinder:zinder /var/lib/zinder /etc/zinder

COPY --from=builder /workspace/target/release/zinder-ingest /usr/local/bin/zinder-ingest
COPY --from=builder /workspace/target/release/zinder-query  /usr/local/bin/zinder-query

# s6-overlay service definitions live under /etc/s6-overlay/s6-rc.d/.
# Each "longrun" service has a run script (the supervised process) plus a
# finish script that propagates fatal exits to the container.
COPY deploy/single-container/services/zinder-ingest/type        /etc/s6-overlay/s6-rc.d/zinder-ingest/type
COPY deploy/single-container/services/zinder-ingest/run         /etc/s6-overlay/s6-rc.d/zinder-ingest/run
COPY deploy/single-container/services/zinder-ingest/finish      /etc/s6-overlay/s6-rc.d/zinder-ingest/finish
COPY deploy/single-container/services/zinder-query/type         /etc/s6-overlay/s6-rc.d/zinder-query/type
COPY deploy/single-container/services/zinder-query/run          /etc/s6-overlay/s6-rc.d/zinder-query/run
COPY deploy/single-container/services/zinder-query/finish       /etc/s6-overlay/s6-rc.d/zinder-query/finish
COPY deploy/single-container/services/zinder-query/dependencies.d /etc/s6-overlay/s6-rc.d/zinder-query/dependencies.d
COPY deploy/single-container/services/user/contents.d           /etc/s6-overlay/s6-rc.d/user/contents.d
COPY deploy/single-container/config.example.ingest.toml         /etc/zinder/config.example.ingest.toml
COPY deploy/single-container/config.example.query.toml          /etc/zinder/config.example.query.toml

# The run scripts launch each binary with `--config /etc/zinder/{ingest,query}.toml`.
# Seed those paths from the example configs so the image is immediately runnable
# on PaaS targets that drive every dynamic field through `ZINDER_*` env vars
# without requiring a mounted config file (ADR-0018). Operators that ship their
# own configs simply mount over these defaults.
RUN cp /etc/zinder/config.example.ingest.toml /etc/zinder/ingest.toml \
    && cp /etc/zinder/config.example.query.toml /etc/zinder/query.toml \
    && chown zinder:zinder /etc/zinder/ingest.toml /etc/zinder/query.toml \
    && chmod +x /etc/s6-overlay/s6-rc.d/zinder-ingest/run \
                /etc/s6-overlay/s6-rc.d/zinder-ingest/finish \
                /etc/s6-overlay/s6-rc.d/zinder-query/run \
                /etc/s6-overlay/s6-rc.d/zinder-query/finish

# Each service strict-parses its own TOML schema, so the writer's config
# (`/etc/zinder/ingest.toml`) and the reader's config (`/etc/zinder/query.toml`)
# live separately and drive their own listen addresses. Setting
# `ZINDER_QUERY__*` or `ZINDER_INGEST__*` at the container level would force
# both binaries to see the same env-var-injected key (config-rs merges every
# `ZINDER_*` variable into the resolved config), which the strict schemas
# reject; supply per-service overrides via CLI flags or per-binary env files
# instead. Ops endpoints bind via the s6 run scripts
# (`--ops-listen-addr`): the writer's loopback-only ops on 9105, the
# reader's public ops on 9106 for the load-balancer healthcheck.
ENV S6_KEEP_ENV=1

EXPOSE 9101 9106

HEALTHCHECK --interval=30s --timeout=5s --start-period=90s --retries=3 \
    CMD curl -fsS http://localhost:9106/readyz || exit 1

# /init is s6-overlay's PID 1; it brings up the s6-rc service tree.
ENTRYPOINT ["/init"]
