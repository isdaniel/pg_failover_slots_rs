//! # pg_failover_slots_rs — Rust/pgrx rewrite
//!
//! This extension synchronizes logical replication slots from a primary server
//! to a physical standby, making them survive failover. It also ensures
//! physical-before-logical ordering via the `standby_slot_names` mechanism.
//!
//! Original C implementation: <https://github.com/EnterpriseDB/pg_failover_slots>

mod guc;
mod pg_compat;
mod remote;
mod slot_sync;
mod standby_wait;
mod worker;

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    include!("tests.rs");
}

use pgrx::prelude::*;

/// Extension name constant
pub const EXTENSION_NAME: &str = "pg_failover_slots_rs";

/// Short sleep time when waiting for feedback (10 seconds in ms)
pub const WORKER_WAIT_FEEDBACK: i64 = 10_000;

pgrx::pg_module_magic!();

// ---------------------------------------------------------------------------
// SQL-callable diagnostic functions
// ---------------------------------------------------------------------------

/// Return the pg_failover_slots_rs extension version.
#[pg_extern]
fn pg_failover_slots_rs_version() -> &'static str {
    "1.0.0"
}

/// Extension initialization — called when PostgreSQL loads the shared library.
///
/// This function:
/// 1. Verifies loading via `shared_preload_libraries`
/// 2. Registers all GUC configuration variables
/// 3. Registers the background worker for slot synchronization
/// 4. Installs the ClientAuthentication hook for physical-before-logical ordering
#[pg_guard]
pub unsafe extern "C-unwind" fn _PG_init() {
    // Must be loaded via shared_preload_libraries
    if !pg_sys::process_shared_preload_libraries_in_progress {
        pgrx::error!("pg_failover_slots_rs is not in shared_preload_libraries");
    }

    // Register all GUC variables
    guc::register_gucs();

    // Don't register worker/hooks during binary upgrade
    if pg_sys::IsBinaryUpgrade {
        return;
    }

    // Register the background worker
    worker::register_background_worker();

    // Install the ClientAuthentication hook for physical-before-logical ordering
    standby_wait::install_client_auth_hook();
}

/// This module is required by `cargo pgrx test` invocations.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {
        // perform one-off initialization when the pg_test framework starts
    }

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries = 'pg_failover_slots_rs'"]
    }
}
