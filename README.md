# pg_failover_slots_rs (Rust/pgrx)

A PostgreSQL extension that synchronizes logical replication slots from a
primary server to a physical streaming standby so that the slots survive
failover. It also enforces physical-before-logical ordering, ensuring
physical standbys have confirmed WAL receipt before logical replication data
is sent downstream.

This is a Rust rewrite of the original C extension
[EnterpriseDB/pg_failover_slots](https://github.com/EnterpriseDB/pg_failover_slots),
built with the [pgrx](https://github.com/pgcentralfoundation/pgrx) framework
(v0.19.1).

## Supported PostgreSQL Versions

- PostgreSQL 14
- PostgreSQL 15
- PostgreSQL 16
- PostgreSQL 17
- PostgreSQL 18

## How It Works

### Slot Synchronization (Standby Side)

A background worker starts on the standby during recovery. Each cycle it:

1. Connects to the primary via libpq using the configured DSN (or falls back
   to the WAL receiver's `primary_conninfo`).
2. Queries `pg_catalog.pg_replication_slots` for logical slots matching the
   configured filter.
3. Fetches the physical replication slot's `restart_lsn` for safety checks.
4. For each remote logical slot:
   - If the slot already exists locally, acquires it and updates its
     `confirmed_flush_lsn`, `restart_lsn`, and `catalog_xmin`.
   - If the slot does not exist, creates it as ephemeral, waits for the
     primary's slot to advance past the local WAL reservation, then persists
     the slot.
5. Optionally drops local logical slots that no longer exist on the primary
   `pg_failover_slots_rs.drop_extra_slots`).

### Physical-Before-Logical Ordering (Primary Side)

On the primary, a `ClientAuthentication_hook` intercepts walsender connections
for logical replication. It replaces the `PqCommMethods` communication table
with a shim that inspects outgoing WAL data messages. Before forwarding each
message, the shim checks that the named physical standby slots (configured via
`pg_failover_slots_rs.standby_slot_names`) have flushed past the message's LSN.
This guarantees the standby has received all WAL before the logical subscriber
sees it.

## Relationship to PostgreSQL 17+ Native Slot Synchronization

PostgreSQL 17 introduced native logical slot failover: the `failover` slot
option, the `sync_replication_slots` GUC and `pg_sync_replication_slots()`
function (standby-side sync), and `synchronized_standby_slots` (the primary-side
physical-before-logical guarantee). Where those cover your needs on PG17+, the
native mechanism is the first choice.

This extension remains useful when you need a **single mechanism that works
uniformly across PostgreSQL 15–18** (including versions without native slot
sync), or want its filtering (`synchronize_slot_names`) and
physical-before-logical enforcement (`standby_slot_names`) with consistent
behavior across a mixed-version fleet. Do not enable both this extension's
synchronization and the native `sync_replication_slots` for the same slots on
the same standby — pick one authority per slot to avoid conflicting updates.

## Building

### Prerequisites

- Rust toolchain (1.96+)
- [cargo-pgrx](https://github.com/pgcentralfoundation/pgrx) v0.19.1
- PostgreSQL development headers (`postgresql-server-dev-XX`)
- libpq development files (`libpq-dev`)
- `pkg-config`, `libclang-dev`, `clang`

This repository pins the Rust toolchain to 1.96.0 via `rust-toolchain.toml`,
so `cargo` auto-selects the correct version when you build.

### Initialize pgrx

```sh
cargo install --locked cargo-pgrx --version "=0.19.1"
cargo pgrx init --pg16 $(which pg_config)
```

Replace `--pg16` with the target version (`--pg15`, `--pg17`, `--pg18`).

### Package for Installation

```sh
cargo pgrx package \
  --features pg16 \
  --no-default-features \
  --pg-config /usr/lib/postgresql/16/bin/pg_config
```

This produces installable artifacts under `target/release/pg_failover_slots_rs-pg16/`.

## Installation

After building, copy the extension files to the PostgreSQL installation:

```sh
# From the package output directory:
cp usr/lib/postgresql/16/lib/pg_failover_slots_rs.so \
   /usr/lib/postgresql/16/lib/

cp usr/share/postgresql/16/extension/pg_failover_slots_rs* \
   /usr/share/postgresql/16/extension/
```

Add to `postgresql.conf` on both primary and standby:

```
shared_preload_libraries = 'pg_failover_slots_rs'
```

Restart PostgreSQL, then create the extension:

```sql
CREATE EXTENSION pg_failover_slots_rs;
```

## Configuration

All parameters are set via `postgresql.conf` or `ALTER SYSTEM` and take effect
after a configuration reload (SIGHUP) unless noted otherwise.

### Primary-Side Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `pg_failover_slots_rs.standby_slot_names` | string | `""` | Comma-separated physical slot names that must confirm WAL receipt before logical data is sent. |
| `pg_failover_slots_rs.standby_slots_min_confirmed` | integer | `-1` | How many named standby slots must confirm. `-1` means all, `0` disables the wait. Range: -1 to 100. |

### Standby-Side Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `pg_failover_slots_rs.synchronize_slot_names` | string | `"name_like:%%"` | Comma-separated `key:value` filters for which slots to sync. Keys: `name` (exact), `name_like` (SQL LIKE pattern), `plugin` (output plugin). Bare words match as exact names. Default syncs all logical slots. |
| `pg_failover_slots_rs.primary_dsn` | string | `""` | Connection string for the primary. If empty, falls back to the WAL receiver's `primary_conninfo`. Superuser only. |
| `pg_failover_slots_rs.worker_nap_time` | integer (ms) | `60000` | Sleep time between synchronization cycles. Minimum: 1000 ms. Superuser only. |
| `pg_failover_slots_rs.maintenance_db` | string | `"postgres"` | Database name used when connecting to the primary. Superuser only. |
| `pg_failover_slots_rs.drop_extra_slots` | boolean | `true` | Drop local logical slots not found on the primary. |

### Read-Only

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `pg_failover_slots_rs.version` | string | `"0.1.0"` | Extension version (internal, read-only). |

## Example Setup

### Primary (`postgresql.conf`)

```
shared_preload_libraries = 'pg_failover_slots_rs'
wal_level = logical
max_replication_slots = 10
max_wal_senders = 10
hot_standby = on
hot_standby_feedback = on

pg_failover_slots_rs.standby_slot_names = 'standby_slot'
```

### Standby (`postgresql.auto.conf` or `postgresql.conf`)

```
primary_conninfo = 'host=primary port=5432 user=postgres password=...'
primary_slot_name = 'standby_slot'
hot_standby = on
hot_standby_feedback = on

shared_preload_libraries = 'pg_failover_slots_rs'
pg_failover_slots_rs.synchronize_slot_names = 'name_like:%%'
pg_failover_slots_rs.primary_dsn = 'host=primary port=5432 user=postgres password=...'
pg_failover_slots_rs.worker_nap_time = 10000
pg_failover_slots_rs.drop_extra_slots = true
```

The test runs six phases:

1. **Verify initial setup** -- extension loaded, physical slot exists, standby
   in recovery, initial data present.
2. **Set up logical replication** -- create subscription, verify initial data
   sync and live replication.
3. **Verify slot synchronization** -- wait for the logical slot to appear on
   the standby and become fully persistent (non-null `confirmed_flush_lsn`).
4. **Simulate failover** -- terminate primary backends, promote the standby.
5. **Verify slot survival** -- confirm the logical slot exists on the new
   primary with valid LSN and preserved plugin.
6. **Verify continued replication** -- insert data on the new primary,
   reconfigure the subscriber, verify data replicates.

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| [pgrx](https://crates.io/crates/pgrx) | 0.19.1 | PostgreSQL extension framework |
| [libpq-sys](https://crates.io/crates/libpq-sys) | 0.8.0 | FFI bindings for libpq (client connections to primary) |
| [pgrx-tests](https://crates.io/crates/pgrx-tests) | 0.19.1 | Test framework (dev dependency) |

The `libpq-sys` crate handles library discovery automatically via `pkg-config`
or `pg_config`. No `build.rs` is needed.

## License

See the original [pg_failover_slots](https://github.com/EnterpriseDB/pg_failover_slots)
repository for license information.
