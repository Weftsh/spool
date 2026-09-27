# syntax=docker/dockerfile:1
# The deployable Stratum image: one binary (server + admin CLI) plus the
# built marketing site and dashboard. Multi-stage; the runtime stage is
# debian-slim + git (the engine shells out to it) + tini (reaps orphaned
# git children, forwards SIGTERM to the server's graceful shutdown).
#
# Corporate/CI proxies: pass a CA via `--secret id=extra_ca,src=…` and the
# predefined HTTP_PROXY/HTTPS_PROXY build args; absent, the steps no-op.

FROM rust:1.98-bookworm AS build
WORKDIR /src
RUN --mount=type=secret,id=extra_ca,required=false \
    if [ -s /run/secrets/extra_ca ]; then \
      mkdir -p /usr/local/share/ca-certificates \
      && cp /run/secrets/extra_ca /usr/local/share/ca-certificates/extra-ca.crt \
      && update-ca-certificates; fi
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=secret,id=extra_ca,required=false \
    cargo build --release -p stratum-server \
    && cp target/release/stratum-server /stratum-server \
    && strip /stratum-server

FROM node:22-bookworm-slim AS site
WORKDIR /web/site
# node-slim has no update-ca-certificates; node reads extra roots itself.
RUN --mount=type=secret,id=extra_ca,required=false \
    mkdir -p /usr/local/share \
    && ([ -s /run/secrets/extra_ca ] && cp /run/secrets/extra_ca /usr/local/share/extra-ca.crt || touch /usr/local/share/extra-ca.crt)
ENV NODE_EXTRA_CA_CERTS=/usr/local/share/extra-ca.crt
COPY web/shared /web/shared
COPY web/site /web/site
RUN npm ci && npm run build

FROM node:22-bookworm-slim AS dashboard
WORKDIR /web/dashboard
RUN --mount=type=secret,id=extra_ca,required=false \
    mkdir -p /usr/local/share \
    && ([ -s /run/secrets/extra_ca ] && cp /run/secrets/extra_ca /usr/local/share/extra-ca.crt || touch /usr/local/share/extra-ca.crt)
ENV NODE_EXTRA_CA_CERTS=/usr/local/share/extra-ca.crt
COPY web/shared /web/shared
COPY web/dashboard /web/dashboard
RUN npm ci && npx vite build

FROM debian:bookworm-slim AS runtime
# `ca-certificates` is a Recommends of git and curl, and
# `--no-install-recommends` leaves it out: the first image shipped
# without a CA store, so every `git` over HTTPS from the server —
# the origin probe, mirror syncs, imports — failed certificate
# verification while the Rust client, which bundles its own roots,
# reached Stripe fine. The smoke's origin probe now pins it.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git curl tini ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 -d /var/lib/stratum stratum \
    && mkdir -p /var/lib/stratum \
    && chown stratum:stratum /var/lib/stratum
COPY --from=build /stratum-server /usr/local/bin/stratum-server
COPY --from=site /web/site/dist /app/site
COPY --from=dashboard /web/dashboard/dist /app/dashboard
ENV STRATUM_BIND=0.0.0.0:8080 \
    STRATUM_SITE_DIR=/app/site \
    STRATUM_DASHBOARD_DIR=/app/dashboard \
    STRATUM_DATA_DIR=/var/lib/stratum
USER stratum
EXPOSE 8080 2222
# Liveness only — /readyz costs a Postgres query and an S3 LIST per probe.
HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=3 \
  CMD ["curl", "-fsS", "http://127.0.0.1:8080/healthz"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/stratum-server"]
