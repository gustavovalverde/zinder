# Multi-stage build for the zinder-ingest writer binary.
#
# Build context: repository root (the workspace, so cargo can resolve every
# crate). Build command from the repo root:
#
#     docker build -f services/zinder-ingest/Dockerfile -t zinder-ingest .
#
# See PRD-0002 REQ-1 and ADR-0007 for the writer/reader topology this image
# fits into.

ARG RUST_VERSION=1.91
ARG DEBIAN_RELEASE=bookworm

FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS builder

# protoc is required by zinder-proto's build.rs (tonic-prost-build).
# Pinned to match .github/workflows/ci-pr.yml's PROTOC_VERSION.
ARG PROTOC_VERSION=29.3
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates clang cmake curl libclang-dev pkg-config unzip \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSLo /tmp/protoc.zip "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-$(uname -m | sed 's/aarch64/aarch_64/').zip" \
    && unzip -q /tmp/protoc.zip -d /usr/local \
    && rm -f /tmp/protoc.zip

WORKDIR /workspace
COPY . .
RUN cargo build --release --bin zinder-ingest


FROM debian:${DEBIAN_RELEASE}-slim AS runtime

# curl powers the HEALTHCHECK against the ops endpoint; ca-certificates is
# required for outbound TLS (e.g. when the operator runs Zinder behind a
# managed reverse proxy that needs to hit external services).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root user with deterministic uid/gid so volume permissions are stable
# across hosts.
RUN groupadd --system --gid 1000 zinder \
    && useradd --system --uid 1000 --gid zinder --home /var/lib/zinder --shell /usr/sbin/nologin zinder \
    && mkdir -p /var/lib/zinder/store /var/lib/zinder/secondary /etc/zinder \
    && chown -R zinder:zinder /var/lib/zinder /etc/zinder

COPY --from=builder /workspace/target/release/zinder-ingest /usr/local/bin/zinder-ingest

USER zinder:zinder
WORKDIR /var/lib/zinder

# Default ops endpoint inside the container; operators override via
# --ops-listen-addr or ZINDER_OPS_LISTEN_ADDR.
ENV ZINDER_OPS_LISTEN_ADDR=0.0.0.0:9105

# Ingest-control gRPC stays internal: bind on loopback by default. Inside
# a single-container deployment this matches the topology in ADR-0007
# (writer and reader colocated). For Kubernetes-style multi-pod setups
# operators override to 0.0.0.0 explicitly and front the surface with
# their cluster mTLS.
ENV ZINDER_INGEST__CONTROL__LISTEN_ADDR=0.0.0.0:9100

EXPOSE 9100 9105

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://localhost:9105/readyz || exit 1

ENTRYPOINT ["zinder-ingest"]
CMD ["tip-follow", "--config", "/etc/zinder/config.toml"]
