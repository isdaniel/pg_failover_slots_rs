#!/bin/bash
# =============================================================================
# Integration test: REAL HA failover with logical slot transfer
# (pg_failover_slots_rs)
#
# Run from the host in tests/integration/ (needs docker + docker compose).
#
# Unlike run_failover_test.sh (which runs inside a compose service and only
# simulates the outage by terminating backends), this test performs a REAL HA
# failover: it STOPS the primary container, promotes the physical standby, and
# verifies the logical replication slot survived the transfer and that logical
# replication continues against the new primary.
#
# Topology (from docker-compose.yml):
#   primary  --physical streaming (slot standby_slot)-->  standby
#   primary  --logical replication (slot ha_sub)-------->  subscriber
# pg_failover_slots_rs syncs the logical slot to the standby so it survives
# promotion.
# =============================================================================
set -euo pipefail
cd "$(dirname "$0")"

COMPOSE="docker compose -f docker-compose.yml"
PASS=0; FAIL=0
pass(){ echo "[PASS] $1"; PASS=$((PASS + 1)); }
fail(){ echo "[FAIL] $1"; FAIL=$((FAIL + 1)); }
info(){ echo "[INFO] $1"; }

pexec(){ docker exec pfs_primary    psql -U postgres -d testdb -t -A "$@"; }
nexec(){ docker exec pfs_standby    psql -U postgres -d testdb -t -A "$@"; }  # new primary after promotion
sexec(){ docker exec pfs_subscriber psql -U postgres -d testdb -t -A "$@"; }

cleanup(){ $COMPOSE down -v --remove-orphans >/dev/null 2>&1 || true; }
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Phase 1: bring the cluster up
# ---------------------------------------------------------------------------
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

# Standby must be in recovery to start with
rec=$(nexec -c "SELECT pg_is_in_recovery();" 2>/dev/null || echo "")
[ "$rec" = "t" ] && pass "standby is in recovery" || fail "standby not in recovery (got: '$rec')"

# ---------------------------------------------------------------------------
# Phase 2: set up logical replication and baseline sync
# ---------------------------------------------------------------------------
info "Setting up subscriber schema + subscription..."
sexec -c "CREATE TABLE IF NOT EXISTS test_data (id SERIAL PRIMARY KEY, value TEXT NOT NULL, ts TIMESTAMPTZ DEFAULT now());" >/dev/null
sexec -c "CREATE SUBSCRIPTION ha_sub CONNECTION 'host=primary port=5432 user=postgres password=testpass dbname=testdb' PUBLICATION test_pub;" >/dev/null 2>&1 || true

info "Waiting for baseline logical replication (3 seed rows)..."
for _ in $(seq 1 30); do
    n=$(sexec -c "SELECT count(*) FROM test_data;" 2>/dev/null || echo 0)
    [ "$n" -ge 3 ] 2>/dev/null && break
    sleep 1
done
n=$(sexec -c "SELECT count(*) FROM test_data;" 2>/dev/null || echo 0)
[ "$n" -ge 3 ] 2>/dev/null && pass "baseline logical replication working ($n rows)" \
                           || fail "baseline sync failed (got $n rows)"

# ---------------------------------------------------------------------------
# Phase 3: wait for the logical slot to be synced AND persistent on the standby
# ---------------------------------------------------------------------------
info "Waiting for logical slot 'ha_sub' to be synced+persistent on the standby..."
synced="f"
for i in $(seq 1 40); do
    lsn=$(nexec -c "SELECT confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name='ha_sub' AND slot_type='logical' AND confirmed_flush_lsn IS NOT NULL;" 2>/dev/null || echo "")
    if [ -n "$lsn" ]; then synced="t"; break; fi
    # Drive primary slot activity so its position advances and the slot persists
    pexec -c "INSERT INTO test_data (value) VALUES ('warmup_${i}');" >/dev/null 2>&1 || true
    sleep 2
done
if [ "$synced" = "t" ]; then
    pass "logical slot synced to standby and persistent (confirmed_flush_lsn=$lsn)"
else
    fail "logical slot never became persistent on standby"
    info "standby slots:"; nexec -c "SELECT slot_name, slot_type, confirmed_flush_lsn FROM pg_replication_slots;" 2>/dev/null || true
fi

# Preserve the plugin so we can confirm it survives the transfer
plugin_before=$(nexec -c "SELECT plugin FROM pg_replication_slots WHERE slot_name='ha_sub';" 2>/dev/null || echo "")
pre_rows=$(sexec -c "SELECT count(*) FROM test_data;" 2>/dev/null || echo 0)
info "Pre-failover subscriber row count: $pre_rows (slot plugin: '$plugin_before')"

# ---------------------------------------------------------------------------
# Phase 4: REAL failover — stop the primary container, promote the standby
# ---------------------------------------------------------------------------
info "Disabling subscription before the outage..."
sexec -c "ALTER SUBSCRIPTION ha_sub DISABLE;" >/dev/null 2>&1 || true

info "STOPPING the primary container (real HA outage)..."
$COMPOSE stop primary

# Confirm the old primary is actually down
if docker exec pfs_primary pg_isready -U postgres -q 2>/dev/null; then
    fail "primary still reachable after stop"
else
    pass "primary is down (real outage)"
fi

info "Promoting the standby to primary..."
nexec -c "SELECT pg_promote(true, 60);" >/dev/null 2>&1 || true

info "Waiting for promotion to complete..."
promoted="f"
for _ in $(seq 1 60); do
    rec=$(nexec -c "SELECT pg_is_in_recovery();" 2>/dev/null || echo "t")
    if [ "$rec" = "f" ]; then promoted="t"; break; fi
    sleep 1
done
[ "$promoted" = "t" ] && pass "standby promoted to new primary" \
                      || fail "standby still in recovery after promotion"

# ---------------------------------------------------------------------------
# Phase 5: verify the logical slot SURVIVED / transferred to the new primary
# ---------------------------------------------------------------------------
slot_name=$(nexec -c "SELECT slot_name FROM pg_replication_slots WHERE slot_type='logical' AND slot_name='ha_sub';" 2>/dev/null || echo "")
if [ "$slot_name" = "ha_sub" ]; then
    pass "logical slot 'ha_sub' SURVIVED failover on the new primary"
else
    fail "logical slot did NOT survive failover"
    nexec -c "SELECT * FROM pg_replication_slots;" 2>/dev/null || true
fi

new_lsn=$(nexec -c "SELECT confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name='ha_sub';" 2>/dev/null || echo "")
[ -n "$new_lsn" ] && pass "transferred slot has a valid LSN ($new_lsn)" \
                  || fail "transferred slot has NULL confirmed_flush_lsn"

plugin_after=$(nexec -c "SELECT plugin FROM pg_replication_slots WHERE slot_name='ha_sub';" 2>/dev/null || echo "")
[ -n "$plugin_after" ] && [ "$plugin_after" = "$plugin_before" ] \
    && pass "slot output plugin preserved across failover ($plugin_after)" \
    || fail "slot plugin changed/lost (before='$plugin_before' after='$plugin_after')"

# ---------------------------------------------------------------------------
# Phase 6: verify logical replication continues against the new primary
# ---------------------------------------------------------------------------
info "Repointing subscriber to the new primary and re-enabling..."
sexec -c "ALTER SUBSCRIPTION ha_sub SET (slot_name = 'ha_sub');" >/dev/null 2>&1 || true
sexec -c "ALTER SUBSCRIPTION ha_sub CONNECTION 'host=standby port=5432 user=postgres password=testpass dbname=testdb';" >/dev/null 2>&1 || true
sexec -c "ALTER SUBSCRIPTION ha_sub ENABLE;" >/dev/null 2>&1 || true

info "Inserting new rows on the NEW primary..."
nexec -c "INSERT INTO test_data (value) VALUES ('after_failover_1'), ('after_failover_2');" >/dev/null

info "Waiting for post-failover rows to replicate to the subscriber..."
got_post=0
for _ in $(seq 1 30); do
    got_post=$(sexec -c "SELECT count(*) FROM test_data WHERE value LIKE 'after_failover%';" 2>/dev/null || echo 0)
    [ "$got_post" = "2" ] 2>/dev/null && break
    sleep 2
done
[ "$got_post" = "2" ] 2>/dev/null \
    && pass "logical replication CONTINUES against new primary (post-failover rows replicated)" \
    || fail "post-failover replication failed (expected 2 rows, got $got_post)"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "  Passed: $PASS   Failed: $FAIL"
if [ "$FAIL" -eq 0 ]; then
    echo "ALL HA FAILOVER TESTS PASSED"; exit 0
else
    echo "SOME TESTS FAILED"; exit 1
fi
