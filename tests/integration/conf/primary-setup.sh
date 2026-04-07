#!/bin/bash
# =============================================================================
# Primary setup script — runs during initdb via docker-entrypoint-initdb.d
#
# 1. Configure replication access in pg_hba.conf
# 2. Create a replication user and physical replication slot for the standby
# 3. Create a test table and publication for logical replication
# =============================================================================
set -e

echo "=== PRIMARY SETUP: configuring replication ==="

# Allow replication connections from the Docker network
cat >> "$PGDATA/pg_hba.conf" <<'EOF'
# Allow replication from any host in the Docker network
host    replication     postgres    0.0.0.0/0    md5
host    all             postgres    0.0.0.0/0    md5
EOF

# Create the physical replication slot for the standby
psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" <<-'EOSQL'
    -- Install the pg_failover_slots_rs extension (makes SQL functions available)
    CREATE EXTENSION IF NOT EXISTS pg_failover_slots_rs;

    -- Physical slot for the standby
    SELECT pg_create_physical_replication_slot('standby_slot');

    -- Test table for logical replication
    CREATE TABLE test_data (
        id    SERIAL PRIMARY KEY,
        value TEXT NOT NULL,
        ts    TIMESTAMPTZ DEFAULT now()
    );

    -- Insert some initial data
    INSERT INTO test_data (value) VALUES ('row_1'), ('row_2'), ('row_3');

    -- Publication for logical replication
    CREATE PUBLICATION test_pub FOR TABLE test_data;
EOSQL

echo "=== PRIMARY SETUP: complete ==="
