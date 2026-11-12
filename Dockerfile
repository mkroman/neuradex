# syntax=docker/dockerfile:1

ARG RUST_VERSION=1.99-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c

# Official mise image. The release workflow verifies the mise binary against
# signed checksums before publishing; pin the image by digest.
FROM ghcr.io/jdx/mise:2026.11.7@sha256:36a8408d3398aff30a761b4ae57f71f50942d64f4e7ecd5353ffaee7ec4fa63a AS mise

# Base layer with build tools. BoringSSL (wreq) needs cmake, and bindgen needs libclang.
FROM rust:${RUST_VERSION} AS chef
WORKDIR /usr/src/app
ENV CARGO_TERM_COLOR=always \
    CARGO_INCREMENTAL=0 \
    CARGO_NET_RETRY=10 \
    RUSTUP_MAX_RETRIES=10

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        binutils \
        ca-certificates \
        cmake \
        libclang-dev && \
    rm -rf /var/lib/apt/lists/*

# Install cargo-chef and cargo-auditable from their checksummed GitHub
# releases (prebuilt binaries — much faster in CI than compiling them).
# mise verifies both against mise.docker.lock before exposing their shims.
COPY --from=mise /usr/local/bin/mise /usr/local/bin/mise
ENV MISE_DATA_DIR=/mise \
    MISE_CONFIG_DIR=/mise \
    MISE_CACHE_DIR=/mise/cache \
    MISE_ENV=docker \
    PATH="/mise/shims:${PATH}"
COPY mise.docker.toml mise.docker.lock ./
RUN --mount=type=cache,target=/mise/cache \
    mise trust && \
    mise install --locked

# Analyze project dependencies
FROM chef AS planner

COPY Cargo.toml Cargo.lock ./
COPY crates crates

RUN cargo chef prepare --recipe-path recipe.json

# Build application binary
FROM chef AS builder
COPY --from=planner /usr/src/app/recipe.json recipe.json

# Cook dependencies — cached as long as recipe.json is unchanged.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY crates crates

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo auditable build --release --locked --package neuradex --bin neuradex && \
    strip target/release/neuradex && \
    cp target/release/neuradex /usr/local/bin/neuradex

# Runtime image: minimal glibc + libgcc + CA certificates, no shell or package
# manager. kagi uses rustls-platform-verifier, which reads the OS trust store —
# the ca-certificates bundled with distroless cc/base are required.
FROM gcr.io/distroless/cc-debian12:nonroot

LABEL org.opencontainers.image.title="neuradex" \
      org.opencontainers.image.description="A small API service implementing tools for LLM agents" \
      org.opencontainers.image.licenses="MIT,Apache-2.0" \
      org.opencontainers.image.source="https://github.com/mkroman/neuradex" \
      org.opencontainers.image.vendor="Mikkel Kroman <mk@maero.dk>"

COPY --from=builder /usr/local/bin/neuradex /usr/local/bin/neuradex

# The service binds to 127.0.0.1 by default; containers must listen on all interfaces.
ENV LISTEN_ADDR=0.0.0.0:8080

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/neuradex"]
