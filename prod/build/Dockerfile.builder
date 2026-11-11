# Build recipe for the Hydra release binary, used by prod/build/build-binary.sh.
#
# Rust toolchain 1.98.0 matches rust-toolchain.toml / CI. The build runs natively inside the
# container; the resulting /src/target/release/hydra is extracted with `docker cp` (the binary is
# never executed here).
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        perl \
        pkg-config \
        curl \
        git \
        ca-certificates \
        clang \
        libclang-dev \
        llvm \
    && rm -rf /var/lib/apt/lists/*

# Pin the same toolchain the repository and CI use.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain 1.98.0 \
    && rustc --version && cargo --version

WORKDIR /src

# The whole repository is the build context (see build-binary.sh).
COPY . .

# `server` + shared Redis data plane + the GreptimeDB usage sink + the Postgres backend.
# Exactly one relational backend is compiled in (db/backend.rs refuses neither/both).
# Cache mounts keep the crates.io registry and the target dir across rebuilds; without them a
# failed or repeated image build recompiles the whole tree (BoringSSL included).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p hydra-server \
        --features server,cluster-redis,usage-greptimedb,db-postgres \
        --bin hydra

# /src/target/release/hydra is the artifact build-binary.sh extracts.
