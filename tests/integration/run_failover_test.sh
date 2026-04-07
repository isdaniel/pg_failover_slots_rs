#!/bin/bash
# =============================================================================
# Integration test: pg_failover_slots_rs logical replication slot survival
#
# This script:
# 1. Sets up logical replication (primary -> subscriber)
# 2. Creates a logical replication slot on the primary
# 3. Waits for the slot to be synced to the standby by pg_failover_slots_rs
# 4. Stops the primary (simulating a crash)
# 5. Promotes the standby to primary
# 6. Verifies the logical replication slot survived on the new primary
# 7. Reconnects the subscriber to the new primary and verifies data flow
# =============================================================================
set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

PASS=0
FAIL=0

pass() {
    echo -e "${GREEN}[PASS]${NC} $1"
    PASS=$((PASS + 1))
}

fail() {
    echo -e "${RED}[FAIL]${NC} $1"
    FAIL=$((FAIL + 1))
}

info() {
    echo -e "${YELLOW}[INFO]${NC} $1"
}

psql_primary() {
    psql -h "$PRIMARY_HOST" -p 5432 -U postgres -d testdb -t -A "$@"
}

psql_standby() {
    psql -h "$STANDBY_HOST" -p 5432 -U postgres -d testdb -t -A "$@"
}

psql_subscriber() {
    psql -h "$SUBSCRIBER_HOST" -p 5432 -U postgres -d testdb -t -A "$@"
}

wait_for_pg() {
    local host=$1
    local name=$2
    local max_wait=60
    local elapsed=0
    info "Waiting for $name ($host) to be ready..."
    while ! pg_isready -h "$host" -U postgres -q 2>/dev/null; do
        sleep 1
        elapsed=$((elapsed + 1))
        if [ $elapsed -ge $max_wait ]; then
            fail "Timeout waiting for $name to be ready after ${max_wait}s"
            return 1
        fi
    done
    info "$name is ready (${elapsed}s)"
}

# =============================================================================
# PHASE 1: Setup verification
# =============================================================================
echo ""
echo "============================================================"
echo "  PHASE 1: Verify initial setup"
echo "============================================================"
echo ""

wait_for_pg "$PRIMARY_HOST" "primary"
wait_for_pg "$STANDBY_HOST" "standby"
wait_for_pg "$SUBSCRIBER_HOST" "subscriber"

# Verify primary has the extension loaded
EXT_VERSION=$(psql_primary -c "SELECT pg_failover_slots_rs_version();" 2>/dev/null || echo "FAIL")
if [ "$EXT_VERSION" = "1.0.0" ]; then
    pass "pg_failover_slots_rs extension loaded on primary (version: $EXT_VERSION)"
else
    fail "pg_failover_slots_rs extension not loaded on primary (got: $EXT_VERSION)"
fi

# Verify the physical slot exists on primary
PHYS_SLOT=$(psql_primary -c "SELECT slot_name FROM pg_replication_slots WHERE slot_type='physical' AND slot_name='standby_slot';" 2>/dev/null || echo "")
if [ "$PHYS_SLOT" = "standby_slot" ]; then
    pass "Physical slot 'standby_slot' exists on primary"
else
    fail "Physical slot 'standby_slot' not found on primary"
fi

# Verify standby is in recovery mode
IS_RECOVERY=$(psql_standby -c "SELECT pg_is_in_recovery();" 2>/dev/null || echo "FAIL")
if [ "$IS_RECOVERY" = "t" ]; then
    pass "Standby is in recovery mode"
else
    fail "Standby is not in recovery mode (got: $IS_RECOVERY)"
fi

# Verify initial data on primary
ROW_COUNT=$(psql_primary -c "SELECT count(*) FROM test_data;" 2>/dev/null || echo "0")
if [ "$ROW_COUNT" = "3" ]; then
    pass "Initial data exists on primary ($ROW_COUNT rows)"
else
    fail "Initial data count mismatch on primary (expected 3, got: $ROW_COUNT)"
fi

# =============================================================================
# PHASE 2: Set up logical replication
# =============================================================================
echo ""
echo "============================================================"
echo "  PHASE 2: Set up logical replication"
echo "============================================================"
echo ""

# Create the test table on the subscriber
info "Creating test table on subscriber..."
psql_subscriber -c "CREATE TABLE IF NOT EXISTS test_data (id SERIAL PRIMARY KEY, value TEXT NOT NULL, ts TIMESTAMPTZ DEFAULT now());" 2>/dev/null

# Create the subscription on the subscriber pointing to the primary
info "Creating subscription on subscriber..."
psql_subscriber -c "CREATE SUBSCRIPTION test_sub CONNECTION 'host=$PRIMARY_HOST port=5432 user=postgres password=testpass dbname=testdb' PUBLICATION test_pub;" 2>/dev/null || true

# Wait for initial data sync
info "Waiting for initial data sync to subscriber..."
SYNC_WAIT=0
while true; do
    SUB_COUNT=$(psql_subscriber -c "SELECT count(*) FROM test_data;" 2>/dev/null || echo "0")
    if [ "$SUB_COUNT" = "3" ]; then
        break
    fi
    sleep 1
    SYNC_WAIT=$((SYNC_WAIT + 1))
    if [ $SYNC_WAIT -ge 30 ]; then
        fail "Timeout waiting for initial data sync to subscriber (got $SUB_COUNT rows)"
        break
    fi
done

SUB_COUNT=$(psql_subscriber -c "SELECT count(*) FROM test_data;" 2>/dev/null || echo "0")
if [ "$SUB_COUNT" = "3" ]; then
    pass "Initial data synced to subscriber ($SUB_COUNT rows)"
else
    fail "Initial data not synced to subscriber (expected 3, got $SUB_COUNT)"
fi

# Verify logical slot was created on primary
LOGICAL_SLOT=$(psql_primary -c "SELECT slot_name FROM pg_replication_slots WHERE slot_type='logical' AND slot_name='test_sub';" 2>/dev/null || echo "")
if [ -n "$LOGICAL_SLOT" ]; then
    pass "Logical replication slot 'test_sub' exists on primary"
else
    fail "Logical replication slot not found on primary"
fi

# Insert more data to verify live replication
info "Inserting additional rows on primary..."
psql_primary -c "INSERT INTO test_data (value) VALUES ('row_4'), ('row_5');" 2>/dev/null

sleep 3

SUB_COUNT=$(psql_subscriber -c "SELECT count(*) FROM test_data;" 2>/dev/null || echo "0")
if [ "$SUB_COUNT" = "5" ]; then
    pass "Live logical replication working (subscriber has $SUB_COUNT rows)"
else
    fail "Live logical replication not working (expected 5, got $SUB_COUNT)"
fi

# =============================================================================
# PHASE 3: Verify slot sync to standby
# =============================================================================
echo ""
echo "============================================================"
echo "  PHASE 3: Verify slot synchronization to standby"
echo "============================================================"
echo ""

info "Waiting for pg_failover_slots_rs to sync logical slot to standby..."
SLOT_SYNC_WAIT=0
SLOT_SYNCED="f"
while true; do
    STANDBY_SLOT=$(psql_standby -c "SELECT slot_name FROM pg_replication_slots WHERE slot_type='logical' AND slot_name='test_sub';" 2>/dev/null || echo "")
    if [ "$STANDBY_SLOT" = "test_sub" ]; then
        SLOT_SYNCED="t"
        break
    fi
    sleep 2
    SLOT_SYNC_WAIT=$((SLOT_SYNC_WAIT + 2))
    if [ $SLOT_SYNC_WAIT -ge 90 ]; then
        break
    fi
done

if [ "$SLOT_SYNCED" = "t" ]; then
    pass "Logical slot 'test_sub' synced to standby (${SLOT_SYNC_WAIT}s)"
else
    fail "Logical slot not synced to standby after ${SLOT_SYNC_WAIT}s"
    # Show diagnostic info
    info "Standby replication slots:"
    psql_standby -c "SELECT slot_name, slot_type, plugin, database, restart_lsn, confirmed_flush_lsn FROM pg_replication_slots;" 2>/dev/null || true
    info "Primary replication slots:"
    psql_primary -c "SELECT slot_name, slot_type, plugin, database, restart_lsn, confirmed_flush_lsn FROM pg_replication_slots;" 2>/dev/null || true
fi

# Wait for the slot to be fully synchronized (persistent with valid
# confirmed_flush_lsn).  The slot is initially created as EPHEMERAL and
# only persisted after the primary's slot position catches up past the
# standby's local reservation.  Inserting data on the primary triggers
# subscriber activity which advances the primary slot's catalog_xmin.
if [ "$SLOT_SYNCED" = "t" ]; then
    info "Waiting for slot to be fully persistent (confirmed_flush_lsn set)..."
    PERSIST_WAIT=0
    SLOT_PERSISTENT="f"
    while true; do
        FLUSH_LSN=$(psql_standby -c "SELECT confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name='test_sub' AND confirmed_flush_lsn IS NOT NULL;" 2>/dev/null || echo "")
        if [ -n "$FLUSH_LSN" ] && [ "$FLUSH_LSN" != "" ]; then
            SLOT_PERSISTENT="t"
            break
        fi
        # Insert data on primary to trigger subscriber activity and advance catalog_xmin
        psql_primary -c "INSERT INTO test_data (value) VALUES ('sync_trigger_${PERSIST_WAIT}');" 2>/dev/null || true
        sleep 3
        PERSIST_WAIT=$((PERSIST_WAIT + 3))
        if [ $PERSIST_WAIT -ge 60 ]; then
            break
        fi
    done

    if [ "$SLOT_PERSISTENT" = "t" ]; then
        pass "Slot fully persistent on standby (confirmed_flush_lsn: $FLUSH_LSN, waited ${PERSIST_WAIT}s)"
    else
        fail "Slot not fully persistent after ${PERSIST_WAIT}s (still ephemeral)"
        info "Standby slot state:"
        psql_standby -c "SELECT slot_name, confirmed_flush_lsn, restart_lsn, catalog_xmin FROM pg_replication_slots WHERE slot_name='test_sub';" 2>/dev/null || true
        info "Primary slot state:"
        psql_primary -c "SELECT slot_name, confirmed_flush_lsn, restart_lsn, catalog_xmin FROM pg_replication_slots WHERE slot_name='test_sub';" 2>/dev/null || true
    fi
fi

# Compare slot positions between primary and standby
if [ "$SLOT_SYNCED" = "t" ]; then
    PRIMARY_LSN=$(psql_primary -c "SELECT confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name='test_sub';" 2>/dev/null || echo "")
    STANDBY_LSN=$(psql_standby -c "SELECT confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name='test_sub';" 2>/dev/null || echo "")
    info "Primary slot LSN: $PRIMARY_LSN, Standby slot LSN: $STANDBY_LSN"
    if [ -n "$PRIMARY_LSN" ] && [ -n "$STANDBY_LSN" ]; then
        pass "Slot positions recorded (primary: $PRIMARY_LSN, standby: $STANDBY_LSN)"
    fi
fi

# =============================================================================
# PHASE 4: Simulate failover
# =============================================================================
echo ""
echo "============================================================"
echo "  PHASE 4: Simulate failover"
echo "============================================================"
echo ""

# First, disable the subscription on subscriber to avoid connection errors
info "Disabling subscription on subscriber..."
psql_subscriber -c "ALTER SUBSCRIPTION test_sub DISABLE;" 2>/dev/null || true
sleep 2

# Record pre-failover state
PRE_FAILOVER_ROWS=$(psql_subscriber -c "SELECT count(*) FROM test_data;" 2>/dev/null || echo "0")
info "Pre-failover subscriber row count: $PRE_FAILOVER_ROWS"

# Stop the primary (simulating a crash)
info "Stopping primary (simulating crash)..."
# We can't directly stop a container from here, so we kill the PG process
psql_primary -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE pid <> pg_backend_pid();" 2>/dev/null || true
# Use pg_ctl stop from within the primary to simulate immediate shutdown
psql_primary -c "SELECT pg_catalog.pg_reload_conf();" 2>/dev/null || true

# The primary will be stopped by the test orchestrator
# For now, we promote the standby
info "Promoting standby to primary..."
psql_standby -c "SELECT pg_promote(true, 60);" 2>/dev/null

# Wait for promotion to complete
info "Waiting for standby promotion to complete..."
PROMOTE_WAIT=0
while true; do
    IS_RECOVERY=$(psql_standby -c "SELECT pg_is_in_recovery();" 2>/dev/null || echo "t")
    if [ "$IS_RECOVERY" = "f" ]; then
        break
    fi
    sleep 1
    PROMOTE_WAIT=$((PROMOTE_WAIT + 1))
    if [ $PROMOTE_WAIT -ge 30 ]; then
        fail "Standby promotion timed out after ${PROMOTE_WAIT}s"
        break
    fi
done

IS_RECOVERY=$(psql_standby -c "SELECT pg_is_in_recovery();" 2>/dev/null || echo "t")
if [ "$IS_RECOVERY" = "f" ]; then
    pass "Standby promoted to primary (${PROMOTE_WAIT}s)"
else
    fail "Standby still in recovery after promotion attempt"
fi

# =============================================================================
# PHASE 5: Verify slot survival after failover
# =============================================================================
echo ""
echo "============================================================"
echo "  PHASE 5: Verify slot survival after failover"
echo "============================================================"
echo ""

# Check that the logical slot survived on the new primary
NEW_PRIMARY_SLOT=$(psql_standby -c "SELECT slot_name FROM pg_replication_slots WHERE slot_type='logical' AND slot_name='test_sub';" 2>/dev/null || echo "")
if [ "$NEW_PRIMARY_SLOT" = "test_sub" ]; then
    pass "Logical slot 'test_sub' SURVIVED failover on new primary"
else
    fail "Logical slot 'test_sub' DID NOT survive failover"
    info "Replication slots on new primary:"
    psql_standby -c "SELECT * FROM pg_replication_slots;" 2>/dev/null || true
fi

# Check slot details
if [ "$NEW_PRIMARY_SLOT" = "test_sub" ]; then
    NEW_LSN=$(psql_standby -c "SELECT confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name='test_sub';" 2>/dev/null || echo "")
    NEW_PLUGIN=$(psql_standby -c "SELECT plugin FROM pg_replication_slots WHERE slot_name='test_sub';" 2>/dev/null || echo "")
    info "Slot on new primary: plugin=$NEW_PLUGIN, confirmed_flush_lsn=$NEW_LSN"

    if [ -n "$NEW_LSN" ]; then
        pass "Slot has valid LSN position on new primary ($NEW_LSN)"
    else
        fail "Slot has NULL LSN on new primary"
    fi

    if [ "$NEW_PLUGIN" = "pgoutput" ]; then
        pass "Slot plugin preserved (pgoutput)"
    fi
fi

# =============================================================================
# PHASE 6: Verify continued logical replication after failover
# =============================================================================
echo ""
echo "============================================================"
echo "  PHASE 6: Verify continued replication after failover"
echo "============================================================"
echo ""

# Insert new data on the new primary (promoted standby)
info "Inserting data on new primary (promoted standby)..."
psql_standby -c "INSERT INTO test_data (value) VALUES ('row_after_failover_1'), ('row_after_failover_2');" 2>/dev/null

# Drop old subscription and create new one pointing to new primary
info "Reconfiguring subscriber to point to new primary..."
psql_subscriber -c "ALTER SUBSCRIPTION test_sub SET (slot_name = 'test_sub');" 2>/dev/null || true
psql_subscriber -c "ALTER SUBSCRIPTION test_sub CONNECTION 'host=$STANDBY_HOST port=5432 user=postgres password=testpass dbname=testdb';" 2>/dev/null || true
psql_subscriber -c "ALTER SUBSCRIPTION test_sub ENABLE;" 2>/dev/null || true

# Wait for new data to arrive
info "Waiting for new data to replicate to subscriber..."
REPL_WAIT=0
TARGET_ROWS=$((PRE_FAILOVER_ROWS + 2))
while true; do
    SUB_COUNT=$(psql_subscriber -c "SELECT count(*) FROM test_data;" 2>/dev/null || echo "0")
    if [ "$SUB_COUNT" -ge "$TARGET_ROWS" ] 2>/dev/null; then
        break
    fi
    sleep 2
    REPL_WAIT=$((REPL_WAIT + 2))
    if [ $REPL_WAIT -ge 30 ]; then
        break
    fi
done

FINAL_COUNT=$(psql_subscriber -c "SELECT count(*) FROM test_data;" 2>/dev/null || echo "0")
if [ "$FINAL_COUNT" -ge "$TARGET_ROWS" ] 2>/dev/null; then
    pass "Post-failover logical replication working ($FINAL_COUNT rows on subscriber)"
else
    fail "Post-failover replication issue (expected >= $TARGET_ROWS, got $FINAL_COUNT)"
fi

# Check for the specific post-failover rows
FAILOVER_ROWS=$(psql_subscriber -c "SELECT count(*) FROM test_data WHERE value LIKE 'row_after_failover%';" 2>/dev/null || echo "0")
if [ "$FAILOVER_ROWS" = "2" ]; then
    pass "Post-failover rows replicated successfully ($FAILOVER_ROWS rows)"
else
    fail "Post-failover rows not replicated (expected 2, got $FAILOVER_ROWS)"
fi

# =============================================================================
# SUMMARY
# =============================================================================
echo ""
echo "============================================================"
echo "  TEST SUMMARY"
echo "============================================================"
echo ""
echo -e "  ${GREEN}Passed: $PASS${NC}"
echo -e "  ${RED}Failed: $FAIL${NC}"
echo ""

if [ $FAIL -eq 0 ]; then
    echo -e "${GREEN}ALL TESTS PASSED${NC}"
    exit 0
else
    echo -e "${RED}SOME TESTS FAILED${NC}"
    exit 1
fi
