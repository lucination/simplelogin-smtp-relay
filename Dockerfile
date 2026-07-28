# --- build stage ---
FROM rust:1-slim-bookworm AS builder
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates perl make gcc \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# --- runtime stage ---
FROM debian:bookworm-slim
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 relay

COPY --from=builder /build/target/release/smtp-relay /app/smtp-relay

USER relay
EXPOSE 8025

# Built-in healthcheck: the binary itself connects to its own listener and
# checks for an SMTP "2xx" greeting banner (see `--healthcheck` in main.rs).
# Avoids needing curl/nc/bash in this minimal image.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/app/smtp-relay", "--healthcheck"]

ENTRYPOINT ["/app/smtp-relay"]
