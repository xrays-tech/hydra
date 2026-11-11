# Hydra production runtime image.
#
# The release binary is DELIBERATELY NOT COPYed here. It is bind-mounted read-only at
# /usr/local/bin/hydra from prod/releases/hydra (see docker-compose.yml), so a release is a binary
# swap. This image supplies only the runtime the dynamically linked binary needs: glibc (base),
# ca-certificates (upstream provider TLS), libgcc (unwinding), curl (healthcheck) and tini (PID 1,
# signal forwarding for graceful drain).
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libgcc-s1 \
        tini \
        curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user (fixed uid 1000). The bind-mounted binary and .env must be readable by
# this uid; ops/setup-dirs.sh sets the data dirs up for it.
# ubuntu:24.04 already ships a uid-1000 `ubuntu` user, so use a dedicated high uid.
RUN useradd --system --uid 10001 --user-group --home-dir /app --shell /usr/sbin/nologin hydra

WORKDIR /app

# /app/data is the mount point for the state volume; /app/.env is the mounted env.
RUN mkdir -p /app/data && chown -R hydra:hydra /app

USER hydra

# The edge data plane (:80 plaintext, :443 per-tenant TLS/SNI) and the admin port.
EXPOSE 80 443 8081

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/hydra"]
