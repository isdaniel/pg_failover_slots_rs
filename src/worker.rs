//! Background worker registration and main loop.

use crate::pg_compat;
use crate::slot_sync;
use crate::EXTENSION_NAME;
use pgrx::prelude::*;
use std::ffi::CString;

// ---------------------------------------------------------------------------
// Background worker registration
// ---------------------------------------------------------------------------

/// Register the pg_failover_slots_rs background worker.
///
/// # Safety
/// Must be called during shared_preload_libraries initialization.
pub unsafe fn register_background_worker() {
    let mut bgw: pg_sys::BackgroundWorker = std::mem::zeroed();

    bgw.bgw_flags = (pg_sys::BGWORKER_SHMEM_ACCESS
        | pg_sys::BGWORKER_BACKEND_DATABASE_CONNECTION) as _;
    bgw.bgw_start_time = pg_sys::BgWorkerStartTime::BgWorkerStart_ConsistentState;
    bgw.bgw_restart_time = 10;

    copy_to_carray(&mut bgw.bgw_library_name, EXTENSION_NAME);
    copy_to_carray(&mut bgw.bgw_function_name, "pg_failover_slots_rs_main");
    copy_to_carray(&mut bgw.bgw_name, "pg_failover_slots_rs worker");

    pg_sys::RegisterBackgroundWorker(&mut bgw);
}

/// Copy a Rust string into a fixed-size C character array.
fn copy_to_carray(dest: &mut [std::os::raw::c_char], src: &str) {
    let bytes = src.as_bytes();
    let len = std::cmp::min(bytes.len(), dest.len() - 1);
    for (i, &b) in bytes[..len].iter().enumerate() {
        dest[i] = b as std::os::raw::c_char;
    }
    dest[len] = 0;
}

// ---------------------------------------------------------------------------
// Background worker entry point
// ---------------------------------------------------------------------------

/// Main entry point for the pg_failover_slots_rs background worker.
#[pg_guard]
#[no_mangle]
pub unsafe extern "C-unwind" fn pg_failover_slots_rs_main(_main_arg: pg_sys::Datum) {
    // Set up signal handlers — transmute Rust fn items to extern "C-unwind" fn ptrs.
    // In PG18, pqsignal is a macro that expands to pqsignal_be; pgrx only binds
    // the expanded name.
    #[cfg(not(feature = "pg18"))]
    {
        pg_sys::pqsignal(
            pg_sys::SIGUSR1 as _,
            pg_compat::as_pqsigfunc(pg_sys::procsignal_sigusr1_handler),
        );
        pg_sys::pqsignal(
            pg_sys::SIGTERM as _,
            pg_compat::as_pqsigfunc(pg_sys::die),
        );
        pg_sys::pqsignal(
            pg_sys::SIGHUP as _,
            pg_compat::as_pqsigfunc(pg_sys::SignalHandlerForConfigReload),
        );
    }
    #[cfg(feature = "pg18")]
    {
        pg_sys::pqsignal_be(
            pg_sys::SIGUSR1 as _,
            pg_compat::as_pqsigfunc(pg_sys::procsignal_sigusr1_handler),
        );
        pg_sys::pqsignal_be(
            pg_sys::SIGTERM as _,
            pg_compat::as_pqsigfunc(pg_sys::die),
        );
        pg_sys::pqsignal_be(
            pg_sys::SIGHUP as _,
            pg_compat::as_pqsigfunc(pg_sys::SignalHandlerForConfigReload),
        );
    }
    pg_sys::BackgroundWorkerUnblockSignals();

    // Identify ourselves in pg_stat_activity
    let appname_raw = std::ffi::CStr::from_ptr((*pg_sys::MyBgworkerEntry).bgw_name.as_ptr());
    let appname = CString::new(appname_raw.to_str().unwrap_or("pg_failover_slots_rs worker")).unwrap();
    let option = CString::new("application_name").unwrap();
    pg_sys::SetConfigOption(
        option.as_ptr(),
        appname.as_ptr(),
        pg_sys::GucContext::PGC_SU_BACKEND,
        pg_sys::GucSource::PGC_S_OVERRIDE,
    );

    log!("pg_failover_slots_rs: starting pg_failover_slots_rs replica worker");

    // Initialize connection to pinned catalogs
    pg_sys::BackgroundWorkerInitializeConnection(
        std::ptr::null(),
        std::ptr::null(),
        0,
    );

    // Main wait loop
    loop {
        pgrx::check_for_interrupts!();

        let nap_time = crate::guc::get_worker_nap_time() as i64;

        let sleep_time = if pg_sys::RecoveryInProgress() {
            slot_sync::synchronize_failover_slots(nap_time)
        } else {
            nap_time * 10
        };

        let rc = pg_sys::WaitLatch(
            pg_sys::MyLatch,
            (pg_sys::WL_LATCH_SET | pg_sys::WL_TIMEOUT | pg_sys::WL_POSTMASTER_DEATH) as _,
            sleep_time as _,
            pg_sys::PG_WAIT_EXTENSION,
        );

        pg_sys::ResetLatch(pg_sys::MyLatch);

        if (rc as u32) & pg_sys::WL_POSTMASTER_DEATH != 0 {
            pg_sys::proc_exit(1);
        }

        if pg_compat::config_reload_pending() {
            pg_compat::clear_config_reload_pending();
            pg_sys::ProcessConfigFile(pg_sys::GucContext::PGC_SIGHUP);
        }
    }
}
