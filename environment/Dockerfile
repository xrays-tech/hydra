# NOTE: no `# syntax=` frontend pin is used — the built-in BuildKit frontend
# handles this Dockerfile (standard instructions only), so builds never pull
# the docker/dockerfile frontend image from a (possibly rate-limited/mirrored)
# registry at build start.
#
# Hydra — Linux runtime image.
#
# The binary is CROSS-COMPILED on the dev machine and staged at `bin/hydra`
# (target x86_64-unknown-linux-gnu, dynamically linked to glibc; BoringSSL is
# statically compiled INTO the binary by the `boring` crate, so no OpenSSL is
# needed in the image). Therefore the image only needs: glibc (base), ca-certificates
# (to verify upstream TLS), libgcc (Rust cross-call/unwinding), and tini (PID 1 +
# signal forwarding for Pingora's graceful zero-downtime upgrade via SIGQUIT).
#
# Build context is the REPO ROOT (so `bin/hydra` is in scope):
#   ./environment/build.sh                       # cross-compile + stage + docker build (recommended)
#   docker build -t hydra:latest -f environment/Dockerfile .   # image only, after bin/hydra is staged

# Pin linux/amd64: the cross-compiled binary is x86_64-unknown-linux-gnu (via rust_build_linux).
# On Apple-Silicon hosts Docker Desktop runs this amd64 image under Rosetta/qemu.
FROM --platform=linux/amd64 debian:bookworm-slim

# Runtime deps. tzdata for correct timestamps; tini as PID 1 for signal handling.
# curl is needed for the container HEALTHCHECK (see docker-compose.yml).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        libgcc-s1 \
        tini \
        tzdata \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user
RUN useradd --system --uid 1000 --home-dir /app --shell /usr/sbin/nologin hydra

WORKDIR /app

# The cross-compiled binary (staged by build.sh)
COPY --chown=hydra:hydra bin/hydra /usr/local/bin/hydra

# Runtime layout. Config + certs + SQLite data are supplied at run time
# (volumes / mounts / env — see dev-docs/ops.md).
RUN mkdir -p /app/data /etc/hydra && chown -R hydra:hydra /app /etc/hydra

# Sensible defaults; override at `docker run` with -e / mounts.
# NOTE: the binary reads HYDRA_DB_URL (see crates/hydra-server/src/main.rs).
ENV HYDRA_DB_URL=sqlite:/app/data/hydra.db?mode=rwc \
    RUST_LOG=info

USER hydra

# Proxy: 443 (TLS/SNI), 80 (plain / localhost). Admin API + /metrics: 8081.
EXPOSE 443 80 8081
VOLUME ["/app/data", "/etc/hydra"]

# tini reaps zombies and forwards SIGQUIT (Pingora graceful upgrade: `kill -SIGQUIT 1`).
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/hydra"]
