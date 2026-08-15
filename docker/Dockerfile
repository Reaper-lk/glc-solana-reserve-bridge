# glc-bridge-daemon deployment image (docs/16-p0-checkpoint.md P1 item:
# docker/ was empty; this is the first deployment manifest in this
# repository). Build from the REPO ROOT so the build context includes
# service/:
#
#   docker build -f docker/Dockerfile -t glc-bridge-daemon .
#
# Run with config and DEV/TEST-posture key files mounted read-only — this
# image never bakes in any key material or config, production or
# otherwise (see service/src/config.rs module docs: key loading is
# explicitly DEV/TEST posture pending the HSM/KMS work in
# docs/16-p0-checkpoint.md P2, and this image does not change that):
#
#   docker run --rm \
#     -v /path/to/config.toml:/etc/glc-bridge/config.toml:ro \
#     -v /path/to/keys:/etc/glc-bridge/keys:ro \
#     -v glc-bridge-data:/var/lib/glc-bridge \
#     -p 127.0.0.1:9100:9100 \
#     glc-bridge-daemon --config /etc/glc-bridge/config.toml

# ---------------------------------------------------------------- build --
# Nightly, not the repo's pinned stable 1.85.0 (rust-toolchain.toml):
# service/'s dependency tree has drifted past what 1.85.0's MSRV supports
# (reqwest's transitive icu_*/time/serde_with versions now need rustc
# 1.86-1.88) — see .github/workflows/ci.yml's comment for the same,
# already-documented finding. This is not a new workaround invented here.
FROM rustlang/rust:nightly-bookworm AS build

WORKDIR /src
COPY shared/ shared/
COPY service/ service/

WORKDIR /src/service
RUN cargo build --release --bin glc-bridge-daemon

# -------------------------------------------------------------- runtime --
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Never root: this process only ever needs to read a mounted config/key
# directory and read/write its own SQLite ledger file, nothing that
# requires elevated privileges.
RUN useradd --system --create-home --home-dir /var/lib/glc-bridge glc-bridge
USER glc-bridge
WORKDIR /var/lib/glc-bridge

COPY --from=build /src/service/target/release/glc-bridge-daemon /usr/local/bin/glc-bridge-daemon

# /health and /metrics — bind this privately (ops::health module docs: no
# auth/TLS by design). The bridge API, if service.api_bind_addr is
# configured, is a separate port an operator chooses in their own config.
EXPOSE 9100

ENTRYPOINT ["/usr/local/bin/glc-bridge-daemon"]
