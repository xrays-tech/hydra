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

# Runtime layout. Config + certs live in the relational database; /app/data is
# kept for operator-supplied files (mounts — see dev-docs/ops.md).
RUN mkdir -p /app/data /etc/hydra && chown -R hydra:hydra /app /etc/hydra

# NO DATABASE URL DEFAULT IS BAKED IN. The binary is built with exactly one backend
# (`db-postgres` or `db-mysql` — see environment/build.sh), and the matching URL must
# come from the deployment (compose `environment:`, Kubernetes secret, or -e):
#
#   postgres (hydra:postgres / hydra:latest):
#     HYDRA_DB_URL=postgres://hydra:hydra@postgres:5432/hydra
#   mysql (hydra:mysql):
#     HYDRA_DB_URL=mysql://hydra:hydra@mysql:3306/hydra
#
# The example credentials (`hydra/hydra`) are the local compose defaults, not a
# recommendation for production. Hardcoding one backend here would let a mysql image
# silently start against a postgres URL (and vice versa); `db/backend.rs` refuses a
# build with neither backend selected, so the URL must agree with the compiled feature.
ENV RUST_LOG=info

USER hydra

# Proxy: 8080 (plain, always bound) + 8443 (TLS/SNI, only when HYDRA_TLS_LISTEN is
# set). Admin API + /metrics: 8081. The old `EXPOSE 443 80` advertised the ports
# design.md drew while the process actually listened on 8080 — exactly the kind of
# declared-vs-actual mismatch that hid the 2026-09-16 listener flip.
EXPOSE 8080 8081 8443
VOLUME ["/app/data", "/etc/hydra"]

# tini reaps zombies and forwards SIGQUIT (Pingora graceful upgrade: `kill -SIGQUIT 1`).
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/hydra"]
