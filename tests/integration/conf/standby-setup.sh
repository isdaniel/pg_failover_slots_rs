#!/bin/bash
# =============================================================================
# Standby setup script — creates a base backup from primary, configures
# streaming replication with pg_failover_slots_rs, and starts PostgreSQL.
# =============================================================================
set -e

echo "=== STANDBY SETUP: waiting for primary ==="

# Wait for the primary to be ready
until pg_isready -h primary -U postgres; do
    sleep 1
done

echo "=== STANDBY SETUP: taking base backup from primary ==="

# Remove any existing data directory (Docker may have created one)
rm -rf /var/lib/postgresql/data/*

# Create base backup from the primary using the physical slot
pg_basebackup \
    -h primary \
    -U postgres \
    -D /var/lib/postgresql/data \
    -X stream \
    -S standby_slot \
    -R \
    -P \
    -v

echo "=== STANDBY SETUP: configuring standby ==="

# Configure the standby for streaming replication with pg_failover_slots_rs
cat >> /var/lib/postgresql/data/postgresql.auto.conf <<EOF

# Streaming replication settings
primary_conninfo = 'host=primary port=5432 user=postgres password=testpass application_name=standby'
primary_slot_name = 'standby_slot'
hot_standby = on
hot_standby_feedback = on

# pg_failover_slots_rs settings (standby side)
shared_preload_libraries = 'pg_failover_slots_rs'
pg_failover_slots_rs.synchronize_slot_names = 'name_like:%%'
pg_failover_slots_rs.primary_dsn = 'host=primary port=5432 user=postgres password=testpass'
pg_failover_slots_rs.worker_nap_time = 5000
pg_failover_slots_rs.drop_extra_slots = true

# Must match or exceed the primary settings
max_worker_processes = 10
max_replication_slots = 10
max_wal_senders = 10

# Ensure wal_level is logical so that after promotion, logical
# replication can work on the new primary
wal_level = logical

# Logging
log_statement = 'all'
logging_collector = off
log_destination = 'stderr'
EOF

# Ensure standby.signal exists (pg_basebackup -R should create it, but be safe)
touch /var/lib/postgresql/data/standby.signal

# Fix permissions
chown -R postgres:postgres /var/lib/postgresql/data
chmod 700 /var/lib/postgresql/data

echo "=== STANDBY SETUP: starting PostgreSQL in recovery mode ==="

# Start PostgreSQL as the postgres user
exec gosu postgres postgres -D /var/lib/postgresql/data
