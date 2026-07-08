# =============================================================================
# Multi-stage Dockerfile for building pg_failover_slots_rs with pgrx
# and running integration tests.
#
# Build arg PG_MAJOR selects the PostgreSQL major version (default: 16).
# =============================================================================
ARG PG_MAJOR=16

# ---------------------------------------------------------------------------
# Stage 1: Build the extension with cargo-pgrx
# ---------------------------------------------------------------------------
FROM rust:1.96-bookworm AS builder

ARG PG_MAJOR

# Install PostgreSQL dev headers + libpq-dev for the chosen major version
RUN apt-get update && apt-get install -y --no-install-recommends \
        gnupg2 curl ca-certificates lsb-release \
    && echo "deb http://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main" \
       > /etc/apt/sources.list.d/pgdg.list \
    && curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc | gpg --dearmor -o /etc/apt/trusted.gpg.d/pgdg.gpg \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        postgresql-${PG_MAJOR} \
        postgresql-server-dev-${PG_MAJOR} \
        libpq-dev \
        pkg-config \
        libclang-dev \
        clang \
        make \
    && rm -rf /var/lib/apt/lists/*

# Make sure pg_config for the target version is on PATH.
# libpq-sys discovers libpq automatically via pkg-config or pg_config --libdir.
ENV PATH="/usr/lib/postgresql/${PG_MAJOR}/bin:${PATH}"

# Install cargo-pgrx matching the version used in the project
RUN cargo install --locked cargo-pgrx --version "=0.19.1"

# Initialize pgrx for the target PG version from system install
RUN cargo pgrx init --pg${PG_MAJOR} /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config

# Copy source
WORKDIR /build
COPY . .

# Build the extension
RUN cargo pgrx package --features pg${PG_MAJOR} --no-default-features --pg-config /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config

# ---------------------------------------------------------------------------
# Stage 2: Runtime image with the extension installed
# ---------------------------------------------------------------------------
FROM postgres:${PG_MAJOR}-bookworm

ARG PG_MAJOR

# Copy the built extension artifacts into the PG installation
COPY --from=builder /build/target/release/pg_failover_slots_rs-pg${PG_MAJOR}/usr/lib/postgresql/${PG_MAJOR}/lib/ \
     /usr/lib/postgresql/${PG_MAJOR}/lib/
COPY --from=builder /build/target/release/pg_failover_slots_rs-pg${PG_MAJOR}/usr/share/postgresql/${PG_MAJOR}/extension/ \
     /usr/share/postgresql/${PG_MAJOR}/extension/

# Default env — can be overridden in docker-compose
ENV POSTGRES_PASSWORD=testpass
ENV POSTGRES_USER=postgres
ENV POSTGRES_DB=postgres
