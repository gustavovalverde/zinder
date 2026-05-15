# Multi-stage build for the zinder-compat-lightwalletd binary.
#
# Build context: repository root. Build command from the repo root:
#
#     docker build -f services/zinder-compat-lightwalletd/Dockerfile -t zinder-compat-lightwalletd .
#
# This image serves the vendored lightwalletd protocol so Zashi/Zodl and
# the Android SDK can integrate without changes.

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
RUN cargo build --release --bin zinder-compat-lightwalletd


FROM debian:${DEBIAN_RELEASE}-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system --gid 1000 zinder \
    && useradd --system --uid 1000 --gid zinder --home /var/lib/zinder --shell /usr/sbin/nologin zinder \
    && mkdir -p /var/lib/zinder/store /var/lib/zinder/secondary /etc/zinder \
    && chown -R zinder:zinder /var/lib/zinder /etc/zinder

COPY --from=builder /workspace/target/release/zinder-compat-lightwalletd /usr/local/bin/zinder-compat-lightwalletd

USER zinder:zinder
WORKDIR /var/lib/zinder

ENV ZINDER_COMPAT__LISTEN_ADDR=0.0.0.0:9067
ENV ZINDER_OPS_LISTEN_ADDR=0.0.0.0:9107

EXPOSE 9067 9107

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://localhost:9107/readyz || exit 1

ENTRYPOINT ["zinder-compat-lightwalletd"]
CMD ["--config", "/etc/zinder/config.toml"]
