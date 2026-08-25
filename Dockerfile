# Two-stage build. The first stage clones openai/codex at a PINNED commit and
# builds against it; the second contains nothing but the binary.
#
# Why a clone during the build rather than a registry package: the Codex crates
# are not published, we pull them in by path (see README). Without a pin no build
# would be reproducible — upstream releases fast and the prompt/model data moves
# with it.

ARG RUST_VERSION=1.95
ARG DEBIAN_RELEASE=trixie

# --- Build -----------------------------------------------------------------
FROM rust:${RUST_VERSION}-slim-${DEBIAN_RELEASE} AS builder

# pkg-config + libssl-dev because openssl-sys is in the tree (the binary links
# libssl/libcrypto). protoc is NOT needed: the only crate that uses it vendors it
# and is not in our dependency tree anyway.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git ca-certificates pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# The clone has to sit NEXT TO the project, because Cargo.toml points at
# ../codex/codex-rs/*. Same layout as locally.
COPY CODEX_REV /src/CODEX_REV
RUN set -eux; \
    rev="$(tr -d '[:space:]' < /src/CODEX_REV)"; \
    mkdir -p /src/codex; \
    cd /src/codex; \
    git init -q .; \
    git remote add origin https://github.com/openai/codex.git; \
    git fetch -q --depth 1 origin "$rev"; \
    git checkout -q FETCH_HEAD

WORKDIR /src/codex-api-wrapper
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Deliberately WITHOUT `--mount=type=cache`: that requires BuildKit including
# buildx, and a Dockerfile the classic builder cannot build is a trap. Speed comes
# from layering instead (the pinned clone sits in its own layer above) and, in CI,
# from buildx's GHA cache.
#
# No cargo-chef: the dependency tree comes almost entirely from the clone, and a
# prewarm would need a second copy of the same sources.
RUN cargo build --release --locked \
    && cp target/release/codex-api-wrapper /usr/local/bin/codex-api-wrapper

# --- Runtime ---------------------------------------------------------------
FROM debian:${DEBIAN_RELEASE}-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 --shell /usr/sbin/nologin codex

COPY --from=builder /usr/local/bin/codex-api-wrapper /usr/local/bin/codex-api-wrapper

# /data MUST exist in the image and belong to codex before VOLUME declares it.
# Without that, Docker creates the volume directory on first start itself — as
# root. The process runs as uid 10001 and could then neither create the socket
# nor sign in. A freshly created named volume inherits owner and permissions from
# the image, which is why this works.
RUN mkdir -p /data /run/codex && chown codex:codex /data /run/codex

USER codex
WORKDIR /home/codex

# Credentials live here. MUST be writable and durable: `codex-login` writes back
# on token refresh, including a rotated refresh token. A read-only mount (a K8s
# secret, say) therefore breaks after the first refresh. Details in DEPLOY.md.
ENV CODEX_WRAPPER_HOME=/data
VOLUME ["/data"]

# Unix socket by default. For a sidecar that is the right choice: the file
# permissions are the access control, no secret has to be distributed, and no port
# stands open in the cluster. Share `/run/codex` as a volume and the neighbouring
# container reaches the socket.
#
# For TCP (own server behind a reverse proxy):
#   serve --listen 0.0.0.0:8080 --api-keys /data/keys.txt
# Without --api-keys, TCP deliberately refuses to start.
VOLUME ["/run/codex"]
CMD ["codex-api-wrapper", "serve", "--listen", "unix:/run/codex/sock"]
