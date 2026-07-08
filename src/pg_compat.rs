//! PostgreSQL compatibility helpers.
//!
//! Provides wrappers for C macros and inline functions that are not exposed
//! through `pg_sys` bindings (they are preprocessor constructs that bindgen
//! cannot resolve).

use pgrx::pg_sys;
use std::os::raw::c_int;

// ---------------------------------------------------------------------------
// LWLock helpers — the named locks are C macros that offset into MainLWLockArray
// ---------------------------------------------------------------------------

/// PG16 LWLock index for ProcArrayLock (from lwlocknames.h).
const PROC_ARRAY_LOCK_IDX: usize = 4;

/// PG16 LWLock index for ReplicationSlotControlLock (from lwlocknames.h).
const REPLICATION_SLOT_CONTROL_LOCK_IDX: usize = 37;

/// Get a pointer to the ProcArrayLock LWLock.
///
/// Equivalent to the C macro `ProcArrayLock`.
#[inline]
pub unsafe fn proc_array_lock() -> *mut pg_sys::LWLock {
    &mut (*pg_sys::MainLWLockArray.add(PROC_ARRAY_LOCK_IDX)).lock
}

/// Get a pointer to the ReplicationSlotControlLock LWLock.
///
/// Equivalent to the C macro `ReplicationSlotControlLock`.
#[inline]
pub unsafe fn replication_slot_control_lock() -> *mut pg_sys::LWLock {
    &mut (*pg_sys::MainLWLockArray.add(REPLICATION_SLOT_CONTROL_LOCK_IDX)).lock
}

// ---------------------------------------------------------------------------
// SpinLock helpers — SpinLockAcquire/Release are C macros
// ---------------------------------------------------------------------------

/// Acquire a spinlock.
///
/// Equivalent to the C macro `SpinLockAcquire(lock)`.
#[inline]
pub unsafe fn spin_lock_acquire(lock: *mut pg_sys::slock_t) {
    // Use test-and-set loop (matches PostgreSQL's S_LOCK)
    while pg_sys::tas(lock) != 0 {
        // Spin until we acquire the lock
        std::hint::spin_loop();
    }
}

/// Release a spinlock.
///
/// Equivalent to the C macro `SpinLockRelease(lock)`.
#[inline]
pub unsafe fn spin_lock_release(lock: *mut pg_sys::slock_t) {
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::Release);
    std::ptr::write_volatile(lock, 0);
}

// ---------------------------------------------------------------------------
// Miscellaneous macro equivalents
// ---------------------------------------------------------------------------

/// Check if a HeapTuple is valid.
///
/// Equivalent to the C macro `HeapTupleIsValid(tuple)`.
#[inline]
pub fn heap_tuple_is_valid(tuple: pg_sys::HeapTuple) -> bool {
    !tuple.is_null()
}

/// `HotStandbyActive()` — checks if hot standby is active.
///
/// NOTE: This checks `standbyState` which is only set in the startup process.
/// In BGW processes, use `RecoveryInProgress()` + `WalRcv != null` instead.
#[inline]
#[allow(dead_code)]
pub unsafe fn hot_standby_active() -> bool {
    pg_sys::standbyState >= pg_sys::HotStandbyState::STANDBY_SNAPSHOT_READY
}

/// `TimestampTzPlusMilliseconds(tz, ms)` — add milliseconds to a TimestampTz.
///
/// In PostgreSQL, TimestampTz is microseconds since 2000-01-01.
#[inline]
pub fn timestamp_tz_plus_milliseconds(tz: pg_sys::TimestampTz, ms: i64) -> pg_sys::TimestampTz {
    tz + ms * 1000
}

/// XLogRecPtr constants — the C `InvalidXLogRecPtr` is 0 but typed as u32 in
/// bindings while XLogRecPtr is u64.
pub const INVALID_XLOG_REC_PTR: pg_sys::XLogRecPtr = 0u64;

/// Invalid TransactionId.
#[allow(dead_code)]
pub const INVALID_TRANSACTION_ID: pg_sys::TransactionId = pg_sys::InvalidTransactionId;

/// Check if `ConfigReloadPending` is true (it's `sig_atomic_t` = `c_int`).
#[inline]
pub unsafe fn config_reload_pending() -> bool {
    pg_sys::ConfigReloadPending != 0
}

/// Clear `ConfigReloadPending`.
#[inline]
pub unsafe fn clear_config_reload_pending() {
    pg_sys::ConfigReloadPending = 0;
}

/// Cast a Rust `unsafe fn(c_int)` to a `pqsigfunc` for use with `pqsignal`.
///
/// This is needed because pgrx generates `pqsigfunc` as
/// `Option<unsafe extern "C-unwind" fn(c_int)>` but the signal handler
/// functions are plain `unsafe fn(c_int)`.
#[inline]
pub unsafe fn as_pqsigfunc(f: unsafe fn(c_int)) -> pg_sys::pqsigfunc {
    // The functions have the same ABI; the distinction is only in Rust's type system.
    Some(std::mem::transmute::<
        unsafe fn(c_int),
        unsafe extern "C-unwind" fn(c_int),
    >(f))
}
