#!/bin/bash
# =============================================================================
# Integration test: physical-before-logical ordering (pg_failover_slots_rs)
#
# Run from the host in tests/integration/ (needs docker + docker compose).
# Verifies logical replication data is held back until the named physical
# standby slot confirms WAL receipt (pg_failover_slots_rs.standby_slot_names).
#
# Method: pause the standby container (freezes its walreceiver -> the
# 'standby_slot' physical slot stalls on the primary), insert a marker on the
# primary, assert the subscriber does NOT see it, then unpause and assert it
# arrives. The paired assertions prove the hold is due to ordering, not lag.
# =============================================================================
set -euo pipefail
cd "$(dirname "$0")"

COMPOSE="docker compose -f docker-compose.yml"
PASS=0; FAIL=0
pass(){ echo "[PASS] $1"; PASS=$((PASS + 1)); }
fail(){ echo "[FAIL] $1"; FAIL=$((FAIL + 1)); }
info(){ echo "[INFO] $1"; }

pexec(){ docker exec pfs_primary   psql -U postgres -d testdb -t -A "$@"; }
sexec(){ docker exec pfs_subscriber psql -U postgres -d testdb -t -A "$@"; }

cleanup(){ $COMPOSE down -v --remove-orphans >/dev/null 2>&1 || true; }
trap cleanup EXIT

info "Bringing up primary, standby, subscriber (building images)..."
$COMPOSE up -d --build primary standby subscriber

for c in pfs_primary pfs_standby pfs_subscriber; do
    info "Waiting for $c to be ready..."
    ready=0
    for _ in $(seq 1 90); do
        if docker exec "$c" pg_isready -U postgres -q 2>/dev/null; then ready=1; break; fi
        sleep 1
    done
    [ "$ready" = "1" ] || { fail "$c never became ready"; exit 1; }
done

# Confirm the ordering GUC is active on the primary
GUC=$(pexec -c "SHOW pg_failover_slots_rs.standby_slot_names;" 2>/dev/null || echo "")
if [ "$GUC" = "standby_slot" ]; then
    pass "primary standby_slot_names = standby_slot"
else
    fail "standby_slot_names not set on primary (got: '$GUC')"
fi

# Subscriber: schema + subscription to the primary's publication
info "Setting up subscriber schema and subscription..."
sexec -c "CREATE TABLE IF NOT EXISTS test_data (id SERIAL PRIMARY KEY, value TEXT NOT NULL, ts TIMESTAMPTZ DEFAULT now());" >/dev/null
sexec -c "CREATE SUBSCRIPTION pbl_sub CONNECTION 'host=primary port=5432 user=postgres password=testpass dbname=testdb' PUBLICATION test_pub;" >/dev/null 2>&1 || true

# Baseline: the 3 seed rows must replicate before we test the hold
info "Waiting for baseline logical replication..."
for _ in $(seq 1 30); do
    n=$(sexec -c "SELECT count(*) FROM test_data;" 2>/dev/null || echo 0)
    [ "$n" -ge 3 ] 2>/dev/null && break
    sleep 1
done
n=$(sexec -c "SELECT count(*) FROM test_data;" 2>/dev/null || echo 0)
if [ "$n" -ge 3 ] 2>/dev/null; then
    pass "baseline logical replication working ($n rows)"
else
    fail "baseline sync failed (got $n rows)"
fi

# Freeze the physical standby so its slot stalls on the primary
info "Pausing standby (freezes physical slot progress)..."
$COMPOSE pause standby

info "Inserting marker row on primary while standby is paused..."
pexec -c "INSERT INTO test_data (value) VALUES ('pbl_marker');" >/dev/null

# Held-back window must stay under wal_sender_timeout (default 60s)
info "Verifying marker is HELD BACK (physical-before-logical)..."
held=1
for _ in $(seq 1 8); do
    got=$(sexec -c "SELECT count(*) FROM test_data WHERE value='pbl_marker';" 2>/dev/null || echo 0)
    if [ "$got" != "0" ]; then held=0; break; fi
    sleep 1
done
if [ "$held" = "1" ]; then
    pass "marker correctly held back while standby was paused"
else
    fail "marker leaked to subscriber before standby confirmed"
fi

info "Unpausing standby..."
$COMPOSE unpause standby

info "Verifying marker is RELEASED after standby resumes..."
released=0
for _ in $(seq 1 30); do
    got=$(sexec -c "SELECT count(*) FROM test_data WHERE value='pbl_marker';" 2>/dev/null || echo 0)
    if [ "$got" = "1" ]; then released=1; break; fi
    sleep 1
done
if [ "$released" = "1" ]; then
    pass "marker released after standby confirmed WAL receipt"
else
    fail "marker never arrived after standby resumed"
fi

echo ""
echo "  Passed: $PASS   Failed: $FAIL"
if [ "$FAIL" -eq 0 ]; then
    echo "ALL PHYSICAL-BEFORE-LOGICAL TESTS PASSED"; exit 0
else
    echo "SOME TESTS FAILED"; exit 1
fi
