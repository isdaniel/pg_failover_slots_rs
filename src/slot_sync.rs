//! Slot synchronization logic.
//!
//! Implements the core functionality of synchronizing logical replication
//! slots from the primary to the standby:
//!
//! - [`synchronize_failover_slots`] — main sync cycle called by the BGW
//! - [`synchronize_one_slot`] — syncs a single slot (create or update)
//! - [`wait_for_primary_slot_catchup`] — waits until the primary slot
//!   advances past our locally reserved position

use crate::guc::{self, FailoverSlotFilter, FailoverSlotFilterKey};
use crate::pg_compat::{self, INVALID_XLOG_REC_PTR};
use crate::remote::{self, RemoteSlot};
use crate::WORKER_WAIT_FEEDBACK;
use pgrx::prelude::*;
use std::ffi::{CStr, CString};

// ---------------------------------------------------------------------------
// Database OID lookup
// ---------------------------------------------------------------------------

/// Look up a database OID by name.  Works without a full database connection.
///
/// Uses `criticalSharedRelcachesBuilt` to determine whether the index can be
/// used for the scan, matching the original C implementation.
///
/// # Safety
/// Must be called within a PostgreSQL transaction context.
pub unsafe fn get_database_oid(dbname: &str) -> pg_sys::Oid {
    let dbname_c = CString::new(dbname).expect("dbname NUL");

    let mut key: pg_sys::ScanKeyData = std::mem::zeroed();
    pg_sys::ScanKeyInit(
        &mut key,
        pg_sys::Anum_pg_database_datname as pg_sys::AttrNumber,
        pg_sys::BTEqualStrategyNumber as pg_sys::StrategyNumber,
        pg_sys::Oid::from(pg_sys::F_NAMEEQ),
        pg_sys::Datum::from(dbname_c.as_ptr()),
    );

    let relation = pg_sys::table_open(
        pg_sys::DatabaseRelationId,
        pg_sys::AccessShareLock as _,
    );

    // Use index scan only if critical shared relcaches have been built.
    // This matches the C code: systable_beginscan(..., criticalSharedRelcachesBuilt, ...)
    let use_index = pg_sys::criticalSharedRelcachesBuilt;

    let scan = pg_sys::systable_beginscan(
        relation,
        pg_sys::Oid::from(pg_sys::DatabaseNameIndexId),
        use_index,
        std::ptr::null_mut(),
        1,
        &mut key,
    );

    let tuple = pg_sys::systable_getnext(scan);

    let oid = if pg_compat::heap_tuple_is_valid(tuple) {
        let datform = pg_sys::GETSTRUCT(tuple) as *mut pg_sys::FormData_pg_database;
        (*datform).oid
    } else {
        error!(
            "pg_failover_slots_rs: database \"{}\" does not exist",
            dbname
        );
    };

    pg_sys::systable_endscan(scan);
    pg_sys::table_close(relation, pg_sys::AccessShareLock as _);

    oid
}

// ---------------------------------------------------------------------------
// Wait for primary slot to catch up
// ---------------------------------------------------------------------------

/// Wait until the named remote slot on the primary has advanced past the
/// locally reserved position in `slot`.
///
/// Returns `true` if the remote slot caught up, `false` if interrupted.
///
/// # Safety
/// Must be called while holding `MyReplicationSlot`.
unsafe fn wait_for_primary_slot_catchup(remote_slot: &mut RemoteSlot) -> bool {
    log!(
        "pg_failover_slots_rs: waiting for remote slot {} lsn ({:X}/{:X}) and catalog xmin ({}) \
         to pass local slot lsn ({:X}/{:X}) and catalog xmin ({})",
        remote_slot.name,
        (remote_slot.restart_lsn >> 32) as u32,
        remote_slot.restart_lsn as u32,
        remote_slot.catalog_xmin,
        ((*pg_sys::MyReplicationSlot).data.restart_lsn >> 32) as u32,
        (*pg_sys::MyReplicationSlot).data.restart_lsn as u32,
        (*pg_sys::MyReplicationSlot).data.catalog_xmin,
    );

    let connstr = remote::make_sync_failover_slots_dsn(&remote_slot.database);
    let conn = match remote::remote_connect(&connstr, "pg_failover_slots_rs") {
        Some(c) => c,
        None => {
            warning!("pg_failover_slots_rs: cannot connect to primary for slot catchup wait");
            return false;
        }
    };

    let mut cb_wait_start: pg_sys::TimestampTz = 0;

    loop {
        pgrx::check_for_interrupts!();

        if !pg_sys::RecoveryInProgress() {
            warning!(
                "pg_failover_slots_rs: replication slot sync wait for slot {} interrupted by promotion",
                remote_slot.name
            );
            return false;
        }

        let filter = FailoverSlotFilter {
            key: FailoverSlotFilterKey::Name,
            val: remote_slot.name.clone(),
        };
        let slots = remote::remote_get_primary_slot_info(&conn, &[filter]);

        if slots.is_empty() {
            return false;
        }

        let mut new_slot = slots.into_iter().next().unwrap();

        let receive_ptr = pg_sys::GetWalRcvFlushRecPtr(std::ptr::null_mut(), std::ptr::null_mut());
        if new_slot.restart_lsn > receive_ptr {
            new_slot.restart_lsn = receive_ptr;
        }
        if new_slot.confirmed_lsn > receive_ptr {
            new_slot.confirmed_lsn = receive_ptr;
        }

        if new_slot.restart_lsn >= (*pg_sys::MyReplicationSlot).data.restart_lsn
            && pg_sys::TransactionIdFollowsOrEquals(
                new_slot.catalog_xmin,
                (*pg_sys::MyReplicationSlot).data.catalog_xmin,
            )
        {
            remote_slot.restart_lsn = new_slot.restart_lsn;
            remote_slot.confirmed_lsn = new_slot.confirmed_lsn;
            remote_slot.catalog_xmin = new_slot.catalog_xmin;
            return true;
        }

        let now = pg_sys::GetCurrentTimestamp();
        let retry_interval = pg_sys::wal_retrieve_retry_interval as i64;
        if cb_wait_start > 0
            && pg_sys::TimestampDifferenceExceeds(
                cb_wait_start,
                now,
                std::cmp::min(retry_interval * 5, 30_000) as i32,
            )
        {
            log!(
                "pg_failover_slots_rs: still waiting for remote slot {} lsn ({:X}/{:X}) \
                 and catalog xmin ({}) to pass local slot lsn ({:X}/{:X}) and catalog xmin ({})",
                remote_slot.name,
                (new_slot.restart_lsn >> 32) as u32,
                new_slot.restart_lsn as u32,
                new_slot.catalog_xmin,
                ((*pg_sys::MyReplicationSlot).data.restart_lsn >> 32) as u32,
                (*pg_sys::MyReplicationSlot).data.restart_lsn as u32,
                (*pg_sys::MyReplicationSlot).data.catalog_xmin,
            );
        }
        cb_wait_start = pg_sys::GetCurrentTimestamp();

        let rc = pg_sys::WaitLatch(
            pg_sys::MyLatch,
            (pg_sys::WL_LATCH_SET | pg_sys::WL_TIMEOUT | pg_sys::WL_POSTMASTER_DEATH) as _,
            retry_interval as _,
            pg_sys::PG_WAIT_EXTENSION,
        );

        if (rc as u32) & pg_sys::WL_POSTMASTER_DEATH != 0 {
            pg_sys::proc_exit(1);
        }

        pg_sys::ResetLatch(pg_sys::MyLatch);
    }
}

// ---------------------------------------------------------------------------
// Single slot synchronization
// ---------------------------------------------------------------------------

/// Synchronize a single logical replication slot from the primary to this
/// standby.
///
/// # Safety
/// Must be called during recovery with proper transaction context.
pub unsafe fn synchronize_one_slot(remote_slot: &mut RemoteSlot) {
    if !pg_sys::RecoveryInProgress() {
        warning!(
            "pg_failover_slots_rs: attempted to sync slot from master when not in recovery"
        );
        return;
    }

    pg_sys::SetCurrentStatementStartTimestamp();
    pg_sys::StartTransactionCommand();
    pg_sys::PushActiveSnapshot(pg_sys::GetTransactionSnapshot());

    // Search for the named slot locally
    let mut found = false;

    pg_sys::LWLockAcquire(
        pg_compat::replication_slot_control_lock(),
        pg_sys::LWLockMode::LW_SHARED,
    );

    let ctl = pg_sys::ReplicationSlotCtl;
    if !ctl.is_null() {
        for i in 0..pg_sys::max_replication_slots {
            let s = &*(*ctl).replication_slots.as_ptr().add(i as usize);
            if !s.in_use {
                continue;
            }
            let slot_name = CStr::from_ptr(s.data.name.data.as_ptr())
                .to_str()
                .unwrap_or("");
            if slot_name == remote_slot.name {
                found = true;
                break;
            }
        }
    }

    pg_sys::LWLockRelease(pg_compat::replication_slot_control_lock());

    if found {
        // Slot exists locally — acquire and update
        let name_c = CString::new(remote_slot.name.as_str()).unwrap();

        #[cfg(any(feature = "pg15", feature = "pg16", feature = "pg17"))]
        pg_sys::ReplicationSlotAcquire(name_c.as_ptr(), true);

        #[cfg(feature = "pg18")]
        pg_sys::ReplicationSlotAcquire(name_c.as_ptr(), true, true);

        if remote_slot.restart_lsn < (*pg_sys::MyReplicationSlot).data.restart_lsn
            || pg_sys::TransactionIdPrecedes(
                remote_slot.catalog_xmin,
                (*pg_sys::MyReplicationSlot).data.catalog_xmin,
            )
        {
            warning!(
                "pg_failover_slots_rs: not synchronizing slot {}; synchronization would move it backward",
                remote_slot.name
            );
            pg_sys::ReplicationSlotRelease();
            pg_sys::PopActiveSnapshot();
            pg_sys::CommitTransactionCommand();
            return;
        }

        pg_sys::LogicalConfirmReceivedLocation(remote_slot.confirmed_lsn);
        pg_sys::LogicalIncreaseXminForSlot(
            remote_slot.confirmed_lsn,
            remote_slot.catalog_xmin,
        );
        pg_sys::LogicalIncreaseRestartDecodingForSlot(
            remote_slot.confirmed_lsn,
            remote_slot.restart_lsn,
        );
        pg_sys::ReplicationSlotMarkDirty();
        pg_sys::ReplicationSlotSave();

        log!(
            "pg_failover_slots_rs: synchronized existing slot {} to lsn ({:X}/{:X}) and catalog xmin ({})",
            remote_slot.name,
            (remote_slot.restart_lsn >> 32) as u32,
            remote_slot.restart_lsn as u32,
            remote_slot.catalog_xmin,
        );
    } else {
        // Slot does not exist locally — create it
        let name_c = CString::new(remote_slot.name.as_str()).unwrap();

        #[cfg(any(feature = "pg15", feature = "pg16"))]
        pg_sys::ReplicationSlotCreate(
            name_c.as_ptr(),
            true,
            pg_sys::ReplicationSlotPersistency::RS_EPHEMERAL,
            remote_slot.two_phase,
        );

        #[cfg(any(feature = "pg17", feature = "pg18"))]
        pg_sys::ReplicationSlotCreate(
            name_c.as_ptr(),
            true,
            pg_sys::ReplicationSlotPersistency::RS_EPHEMERAL,
            remote_slot.two_phase,
            false,
            false,
        );

        let slot = pg_sys::MyReplicationSlot;

        // Set database and plugin
        pg_compat::spin_lock_acquire(&mut (*slot).mutex);
        (*slot).data.database = get_database_oid(&remote_slot.database);
        let plugin_c = CString::new(remote_slot.plugin.as_str()).unwrap();
        let src = plugin_c.as_bytes_with_nul();
        let dest = (*slot).data.plugin.data.as_mut_ptr() as *mut u8;
        let copy_len = std::cmp::min(src.len(), pg_sys::NAMEDATALEN as usize);
        std::ptr::copy_nonoverlapping(src.as_ptr(), dest, copy_len);
        pg_compat::spin_lock_release(&mut (*slot).mutex);

        // Reserve WAL
        pg_sys::ReplicationSlotReserveWal();

        // Compute xmin
        pg_sys::LWLockAcquire(
            pg_compat::proc_array_lock(),
            pg_sys::LWLockMode::LW_EXCLUSIVE,
        );
        let xmin_horizon = pg_sys::GetOldestSafeDecodingTransactionId(true);
        (*slot).effective_catalog_xmin = xmin_horizon;
        (*slot).data.catalog_xmin = xmin_horizon;
        pg_sys::ReplicationSlotsComputeRequiredXmin(true);
        pg_sys::LWLockRelease(pg_compat::proc_array_lock());

        // Check if we can satisfy the remote slot requirements
        if remote_slot.restart_lsn < (*pg_sys::MyReplicationSlot).data.restart_lsn
            || pg_sys::TransactionIdPrecedes(
                remote_slot.catalog_xmin,
                (*pg_sys::MyReplicationSlot).data.catalog_xmin,
            )
        {
            if !wait_for_primary_slot_catchup(remote_slot) {
                pg_sys::ReplicationSlotRelease();
                pg_sys::PopActiveSnapshot();
                pg_sys::CommitTransactionCommand();
                return;
            }
        }

        pg_sys::LogicalConfirmReceivedLocation(remote_slot.confirmed_lsn);
        pg_sys::LogicalIncreaseXminForSlot(
            remote_slot.confirmed_lsn,
            remote_slot.catalog_xmin,
        );
        pg_sys::LogicalIncreaseRestartDecodingForSlot(
            remote_slot.confirmed_lsn,
            remote_slot.restart_lsn,
        );
        pg_sys::ReplicationSlotMarkDirty();
        pg_sys::ReplicationSlotPersist();

        log!(
            "pg_failover_slots_rs: synchronized new slot {} to lsn ({:X}/{:X}) and catalog xmin ({})",
            remote_slot.name,
            (remote_slot.restart_lsn >> 32) as u32,
            remote_slot.restart_lsn as u32,
            remote_slot.catalog_xmin,
        );
    }

    pg_sys::ReplicationSlotRelease();
    pg_sys::PopActiveSnapshot();
    pg_sys::CommitTransactionCommand();
}

// ---------------------------------------------------------------------------
// Main sync cycle
// ---------------------------------------------------------------------------

/// Synchronize all failover slots from the primary.
///
/// Returns the suggested sleep time (in ms) for the next cycle.
///
/// Uses a static `was_lsn_safe` flag (matching C) to emit the "now active"
/// log message only once when transitioning from unsafe → safe.
///
/// # Safety
/// Must be called from the background worker during recovery.
pub unsafe fn synchronize_failover_slots(sleep_time: i64) -> i64 {
    // Static flag: tracks whether the previous cycle had safe LSN positions.
    // Matches C's `static bool was_lsn_safe = false`.
    static mut WAS_LSN_SAFE: bool = false;

    let filters = guc::get_synchronize_slot_names();

    let wal_rcv = pg_sys::WalRcv;
    if wal_rcv.is_null() || filters.is_empty() {
        // Log diagnostic info at LOG level (not every iteration - use static flag)
        static mut LOGGED_SKIP: bool = false;
        if !LOGGED_SKIP {
            log!(
                "pg_failover_slots_rs: sync skipped: wal_rcv_null={}, filters_count={}",
                wal_rcv.is_null(),
                filters.len()
            );
            LOGGED_SKIP = true;
        }
        return sleep_time;
    }

    if !pg_sys::hot_standby_feedback {
        warning!(
            "pg_failover_slots_rs: cannot synchronize replication slot positions \
             because hot_standby_feedback is off"
        );
        return sleep_time;
    }
    if (*wal_rcv).slotname[0] == 0 {
        warning!(
            "pg_failover_slots_rs: cannot synchronize replication slot positions \
             because primary_slot_name is not set"
        );
        return sleep_time;
    }

    log!("pg_failover_slots_rs: starting replication slot synchronization from primary");

    let maintenance_db = guc::get_maintenance_db();
    let connstr = remote::make_sync_failover_slots_dsn(&maintenance_db);
    let conn = match remote::remote_connect(&connstr, "pg_failover_slots_rs") {
        Some(c) => c,
        None => {
            // Connection failed (warning already logged by remote_connect)
            return sleep_time;
        }
    };

    let mut slots = remote::remote_get_primary_slot_info(&conn, &filters);

    let slotname = CStr::from_ptr((*wal_rcv).slotname.as_ptr())
        .to_str()
        .unwrap_or("");
    let safe_lsn = remote::remote_get_physical_slot_lsn(&conn, slotname);

    // Connection is no longer needed after fetching remote info; dropping it
    // now matches the C code's explicit PQfinish(conn) before slot iteration.
    drop(conn);

    log!(
        "pg_failover_slots_rs: fetched {} logical slot(s) from primary, physical slot '{}' lsn={:X}/{:X}",
        slots.len(),
        slotname,
        (safe_lsn >> 32) as u32,
        safe_lsn as u32,
    );

    if guc::get_drop_extra_slots() {
        drop_extra_local_slots(&slots);
    }

    if slots.is_empty() {
        return sleep_time;
    }

    // Find oldest restart_lsn
    let mut lsn: pg_sys::XLogRecPtr = INVALID_XLOG_REC_PTR;
    for rs in &slots {
        if lsn == INVALID_XLOG_REC_PTR || rs.restart_lsn < lsn {
            lsn = rs.restart_lsn;
        }
    }

    if safe_lsn == INVALID_XLOG_REC_PTR
        || (*wal_rcv).latestWalEnd == INVALID_XLOG_REC_PTR
    {
        warning!(
            "pg_failover_slots_rs: cannot synchronize replication slot positions yet \
             because feedback was not sent yet"
        );
        WAS_LSN_SAFE = false;
        return std::cmp::min(sleep_time, WORKER_WAIT_FEEDBACK);
    }

    if (*wal_rcv).latestWalEnd < lsn {
        warning!(
            "pg_failover_slots_rs: requested slot synchronization point {:X}/{:X} is ahead of \
             the standby position {:X}/{:X}, not synchronizing slots",
            (lsn >> 32) as u32,
            lsn as u32,
            ((*wal_rcv).latestWalEnd >> 32) as u32,
            (*wal_rcv).latestWalEnd as u32,
        );
        WAS_LSN_SAFE = false;
        return std::cmp::min(sleep_time, WORKER_WAIT_FEEDBACK);
    }

    for rs in &mut slots {
        let receive_ptr =
            pg_sys::GetWalRcvFlushRecPtr(std::ptr::null_mut(), std::ptr::null_mut());

        if rs.confirmed_lsn > receive_ptr {
            rs.confirmed_lsn = receive_ptr;
        }
        if rs.restart_lsn > lsn {
            rs.restart_lsn = lsn;
        }

        synchronize_one_slot(rs);
    }

    // Log transition from unsafe → safe (matches C's was_lsn_safe/is_lsn_safe)
    if !WAS_LSN_SAFE {
        log!("pg_failover_slots_rs: slot synchronization from primary now active");
    }
    WAS_LSN_SAFE = true;

    sleep_time
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Drop local logical slots that are not present on the primary.
unsafe fn drop_extra_local_slots(remote_slots: &[RemoteSlot]) {
    loop {
        let mut dropslot: Option<String> = None;

        pg_sys::LWLockAcquire(
            pg_compat::replication_slot_control_lock(),
            pg_sys::LWLockMode::LW_SHARED,
        );

        let ctl = pg_sys::ReplicationSlotCtl;
        if !ctl.is_null() {
            for i in 0..pg_sys::max_replication_slots {
                let s = &*(*ctl).replication_slots.as_ptr().add(i as usize);

                if !s.in_use || s.active_pid != 0 {
                    continue;
                }

                // Only consider logical slots (skip physical)
                if s.data.database == pg_sys::InvalidOid {
                    continue;
                }

                let slot_name = CStr::from_ptr(s.data.name.data.as_ptr())
                    .to_str()
                    .unwrap_or("");

                let found = remote_slots.iter().any(|rs| rs.name == slot_name);
                if !found {
                    dropslot = Some(slot_name.to_string());
                    break;
                }
            }
        }

        pg_sys::LWLockRelease(pg_compat::replication_slot_control_lock());

        if let Some(name) = dropslot {
            warning!(
                "pg_failover_slots_rs: dropping replication slot \"{}\"",
                name
            );
            let name_c = CString::new(name).unwrap();
            pg_sys::ReplicationSlotDrop(name_c.as_ptr(), false);
        } else {
            break;
        }
    }
}
