//! Physical-before-logical ordering via walsender hook.
//!
//! Intercepts the walsender communication methods to ensure physical standby
//! slots have confirmed WAL receipt before logical replication data is sent.

use crate::guc;
use crate::pg_compat::{self, INVALID_XLOG_REC_PTR};
use pgrx::prelude::*;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicPtr, Ordering};

// ---------------------------------------------------------------------------
// PQcommMethods FFI — not in pgrx pg_sys (server internal header libpq/libpq.h)
// ---------------------------------------------------------------------------

/// Mirror of PostgreSQL's `PQcommMethods` struct (PG14+).
#[repr(C)]
pub struct PQcommMethods {
    pub comm_reset: Option<unsafe extern "C" fn()>,
    pub flush: Option<unsafe extern "C" fn() -> c_int>,
    pub flush_if_writable: Option<unsafe extern "C" fn() -> c_int>,
    pub is_send_pending: Option<unsafe extern "C" fn() -> bool>,
    pub putmessage:
        Option<unsafe extern "C" fn(msgtype: c_char, s: *const c_char, len: usize) -> c_int>,
    pub putmessage_noblock:
        Option<unsafe extern "C" fn(msgtype: c_char, s: *const c_char, len: usize)>,
}

// SAFETY: Contains only function pointers which are Send + Sync.
unsafe impl Sync for PQcommMethods {}
unsafe impl Send for PQcommMethods {}

extern "C" {
    #[allow(improper_ctypes)]
    static mut PqCommMethods: *const PQcommMethods;
    static am_db_walsender: bool;
}

// ---------------------------------------------------------------------------
// ClientAuthentication_hook FFI — not always in pgrx pg_sys
// ---------------------------------------------------------------------------

/// Type of the ClientAuthentication_hook.
type ClientAuthHookFn = unsafe extern "C-unwind" fn(port: *mut pg_sys::Port, status: c_int);

extern "C" {
    #[allow(improper_ctypes)]
    static mut ClientAuthentication_hook: Option<ClientAuthHookFn>;
}

// ---------------------------------------------------------------------------
// Static storage
// ---------------------------------------------------------------------------

/// Old PqCommMethods pointer (before our replacement).
static OLD_PQ_COMM_METHODS: AtomicPtr<PQcommMethods> =
    AtomicPtr::new(std::ptr::null_mut());

/// Old ClientAuthentication_hook (if any).
static OLD_CLIENT_AUTH_HOOK: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

/// Cached oldest flush LSN across named standby slots.
static mut STANDBY_SLOT_NAMES_OLDEST_FLUSH_LSN: pg_sys::XLogRecPtr = 0;

// ---------------------------------------------------------------------------
// Shim PqCommMethods
// ---------------------------------------------------------------------------

unsafe extern "C" fn shim_comm_reset() {
    let old = OLD_PQ_COMM_METHODS.load(Ordering::Relaxed);
    if !old.is_null() {
        if let Some(f) = (*old).comm_reset { f(); }
    }
}

unsafe extern "C" fn shim_flush() -> c_int {
    let old = OLD_PQ_COMM_METHODS.load(Ordering::Relaxed);
    if !old.is_null() {
        if let Some(f) = (*old).flush { return f(); }
    }
    0
}

unsafe extern "C" fn shim_flush_if_writable() -> c_int {
    let old = OLD_PQ_COMM_METHODS.load(Ordering::Relaxed);
    if !old.is_null() {
        if let Some(f) = (*old).flush_if_writable { return f(); }
    }
    0
}

unsafe extern "C" fn shim_is_send_pending() -> bool {
    let old = OLD_PQ_COMM_METHODS.load(Ordering::Relaxed);
    if !old.is_null() {
        if let Some(f) = (*old).is_send_pending { return f(); }
    }
    false
}

unsafe extern "C" fn shim_putmessage(
    msgtype: c_char,
    s: *const c_char,
    len: usize,
) -> c_int {
    let old = OLD_PQ_COMM_METHODS.load(Ordering::Relaxed);
    if !old.is_null() {
        if let Some(f) = (*old).putmessage { return f(msgtype, s, len); }
    }
    0
}

/// Intercepts WAL data messages to enforce physical-before-logical ordering.
unsafe extern "C" fn shim_putmessage_noblock(
    msgtype: c_char,
    s: *const c_char,
    len: usize,
) {
    // WAL data messages: msgtype='d', s[0]='w', bytes 1..8 = LSN (big-endian)
    if msgtype == b'd' as c_char && len >= 17 {
        let sub_type = *s;
        if sub_type == b'w' as c_char {
            let mut lsn_bytes = [0u8; 8];
            std::ptr::copy_nonoverlapping(
                s.add(1) as *const u8,
                lsn_bytes.as_mut_ptr(),
                8,
            );
            let lsn = u64::from_be_bytes(lsn_bytes);
            wait_for_standby_confirmation(lsn);
        }
    }

    let old = OLD_PQ_COMM_METHODS.load(Ordering::Relaxed);
    if !old.is_null() {
        if let Some(f) = (*old).putmessage_noblock { f(msgtype, s, len); }
    }
}

/// Our replacement PqCommMethods table.
static SHIM_PQ_COMM_METHODS: PQcommMethods = PQcommMethods {
    comm_reset: Some(shim_comm_reset),
    flush: Some(shim_flush),
    flush_if_writable: Some(shim_flush_if_writable),
    is_send_pending: Some(shim_is_send_pending),
    putmessage: Some(shim_putmessage),
    putmessage_noblock: Some(shim_putmessage_noblock),
};

// ---------------------------------------------------------------------------
// Hook installation
// ---------------------------------------------------------------------------

/// Install the ClientAuthentication hook. Called from `_PG_init`.
pub unsafe fn install_client_auth_hook() {
    let original = ClientAuthentication_hook;
    let hook_ptr: *mut () = match original {
        Some(f) => f as *mut (),
        None => std::ptr::null_mut(),
    };
    OLD_CLIENT_AUTH_HOOK.store(hook_ptr, Ordering::SeqCst);
    ClientAuthentication_hook = Some(attach_to_walsender);
}

/// ClientAuthentication_hook callback.
///
/// For database walsenders, replaces PqCommMethods with our shim.
unsafe extern "C-unwind" fn attach_to_walsender(port: *mut pg_sys::Port, status: c_int) {
    let original_ptr = OLD_CLIENT_AUTH_HOOK.load(Ordering::SeqCst);
    if !original_ptr.is_null() {
        let original: ClientAuthHookFn = std::mem::transmute(original_ptr);
        original(port, status);
    }

    if am_db_walsender {
        OLD_PQ_COMM_METHODS.store(PqCommMethods as *mut PQcommMethods, Ordering::SeqCst);
        PqCommMethods = &SHIM_PQ_COMM_METHODS;
    }
}

// ---------------------------------------------------------------------------
// Standby confirmation logic
// ---------------------------------------------------------------------------

/// Check if we can skip waiting for standby slot confirmation.
fn skip_standby_slot_names(commit_lsn: pg_sys::XLogRecPtr) -> bool {
    let slot_names = guc::get_standby_slot_names();
    let min_confirmed = guc::get_standby_slots_min_confirmed();

    // Don't wait on our own slot
    unsafe {
        if !pg_sys::MyReplicationSlot.is_null() {
            let my_name = CStr::from_ptr(
                (*pg_sys::MyReplicationSlot).data.name.data.as_ptr(),
            )
            .to_str()
            .unwrap_or("");
            if slot_names.iter().any(|n| n == my_name) {
                return true;
            }
        }
    }

    let oldest = unsafe { STANDBY_SLOT_NAMES_OLDEST_FLUSH_LSN };
    if oldest >= commit_lsn || min_confirmed == 0 || slot_names.is_empty() {
        return true;
    }

    false
}

/// Wait until named standby slots have flushed past `commit_lsn`.
unsafe fn wait_for_standby_confirmation(commit_lsn: pg_sys::XLogRecPtr) {
    if skip_standby_slot_names(commit_lsn) {
        return;
    }

    let wait_start = pg_sys::GetCurrentTimestamp();

    loop {
        let slot_names = guc::get_standby_slot_names();
        let min_confirmed = guc::get_standby_slots_min_confirmed();

        let total_named = slot_names.len() as i32;
        let mut wait_slots_remaining = if min_confirmed == -1 {
            total_named
        } else {
            std::cmp::min(min_confirmed, total_named)
        };

        if wait_slots_remaining <= 0 {
            return;
        }

        let mut oldest_flush_pos: pg_sys::XLogRecPtr = INVALID_XLOG_REC_PTR;

        pg_sys::LWLockAcquire(
            pg_compat::replication_slot_control_lock(),
            pg_sys::LWLockMode::LW_SHARED,
        );

        let ctl = pg_sys::ReplicationSlotCtl;
        if !ctl.is_null() {
            for i in 0..pg_sys::max_replication_slots {
                let s_ptr = (*ctl).replication_slots.as_mut_ptr().add(i as usize);
                let s = &*s_ptr;

                if !s.in_use {
                    continue;
                }

                let slot_name = CStr::from_ptr(s.data.name.data.as_ptr())
                    .to_str()
                    .unwrap_or("");

                if !slot_names.iter().any(|n| n == slot_name) {
                    continue;
                }

                pg_compat::spin_lock_acquire(&mut (*s_ptr).mutex);

                let flush_pos = if s.data.database == pg_sys::InvalidOid {
                    s.data.restart_lsn
                } else {
                    s.data.confirmed_flush
                };

                pg_compat::spin_lock_release(&mut (*s_ptr).mutex);

                if oldest_flush_pos == INVALID_XLOG_REC_PTR || oldest_flush_pos > flush_pos {
                    oldest_flush_pos = flush_pos;
                }

                if flush_pos >= commit_lsn && wait_slots_remaining > 0 {
                    wait_slots_remaining -= 1;
                }
            }
        }

        pg_sys::LWLockRelease(pg_compat::replication_slot_control_lock());

        if wait_slots_remaining == 0 {
            if STANDBY_SLOT_NAMES_OLDEST_FLUSH_LSN < oldest_flush_pos {
                STANDBY_SLOT_NAMES_OLDEST_FLUSH_LSN = oldest_flush_pos;
            }
            return;
        }

        // Poll with brief sleep
        let rc = pg_sys::WaitLatch(
            pg_sys::MyLatch,
            (pg_sys::WL_LATCH_SET | pg_sys::WL_TIMEOUT | pg_sys::WL_POSTMASTER_DEATH) as _,
            100, // 100ms
            pg_sys::PG_WAIT_EXTENSION,
        );

        if (rc as u32) & pg_sys::WL_POSTMASTER_DEATH != 0 {
            pg_sys::proc_exit(1);
        }

        pg_sys::ResetLatch(pg_sys::MyLatch);

        pgrx::check_for_interrupts!();

        // Check replication timeout
        if pg_sys::wal_sender_timeout > 0 {
            let now = pg_sys::GetCurrentTimestamp();
            let deadline = pg_compat::timestamp_tz_plus_milliseconds(
                wait_start,
                pg_sys::wal_sender_timeout as i64,
            );
            if now > deadline {
                warning!(
                    "pg_failover_slots_rs: terminating walsender process due to \
                     pg_failover_slots_rs.standby_slot_names replication timeout"
                );
                pg_sys::proc_exit(0);
            }
        }

        // Reload config if needed
        if pg_compat::config_reload_pending() {
            pg_compat::clear_config_reload_pending();
            pg_sys::ProcessConfigFile(pg_sys::GucContext::PGC_SIGHUP);
            if skip_standby_slot_names(commit_lsn) {
                return;
            }
        }
    }
}
