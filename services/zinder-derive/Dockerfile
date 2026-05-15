# Multi-stage build for the zinder-derive binary.
#
# Build context: repository root. Build command from the repo root:
#
#     docker build -f services/zinder-derive/Dockerfile -t zinder-derive .
#
# This image serves the ExplorerQuery gRPC surface and is intended to be
# colocated with zinder-query (per the derive-plane federation pattern) rather
# than exposed publicly.

ARG RUST_VERSION=1.95
ARG DEBIAN_RELEASE=bookworm

FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS builder

ARG PROTOC_VERSION=29.3
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates clang cmake curl libclang-dev pkg-config unzip \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSLo /tmp/protoc.zip "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-$(uname -m | sed 's/aarch64/aarch_64/').zip" \
    && unzip -q /tmp/protoc.zip -d /usr/local \
    && rm -f /tmp/protoc.zip

WORKDIR /workspace
COPY . .
RUN cargo build --release --bin zinder-derive


FROM debian:${DEBIAN_RELEASE}-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system --gid 1000 zinder \
    && useradd --system --uid 1000 --gid zinder --home /var/lib/zinder --shell /usr/sbin/nologin zinder \
    && mkdir -p /var/lib/zinder/store /var/lib/zinder/secondary /etc/zinder \
    && chown -R zinder:zinder /var/lib/zinder /etc/zinder

COPY --from=builder /workspace/target/release/zinder-derive /usr/local/bin/zinder-derive

USER zinder:zinder
WORKDIR /var/lib/zinder

ENV ZINDER_DERIVE__EXPLORER__LISTEN_ADDR=0.0.0.0:9068
ENV ZINDER_OPS__LISTEN_ADDR=0.0.0.0:9069

EXPOSE 9068 9069

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://localhost:9069/readyz || exit 1

ENTRYPOINT ["zinder-derive"]
CMD ["--config", "/etc/zinder/config.toml"]
