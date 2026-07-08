//! GUC (Grand Unified Configuration) parameters for pg_failover_slots_rs.
//!
//! Defines all configuration variables matching the original C extension:
//! - `pg_failover_slots_rs.version` — read-only version string
//! - `pg_failover_slots_rs.standby_slot_names` — physical slot names for ordering (primary)
//! - `pg_failover_slots_rs.standby_slots_min_confirmed` — how many must confirm
//! - `pg_failover_slots_rs.synchronize_slot_names` — slot filter patterns (standby)
//! - `pg_failover_slots_rs.drop_extra_slots` — drop local slots not on primary
//! - `pg_failover_slots_rs.primary_dsn` — connection string to primary
//! - `pg_failover_slots_rs.worker_nap_time` — sync interval in ms
//! - `pg_failover_slots_rs.maintenance_db` — database for primary connections

use pgrx::guc::*;
use pgrx::prelude::*;
use std::ffi::{CStr, CString};

// ---------------------------------------------------------------------------
// GUC static settings
// ---------------------------------------------------------------------------

/// Extension version, single source of truth = Cargo package version.
/// Built as a const `&CStr` from CARGO_PKG_VERSION at compile time.
const VERSION_CSTR: &CStr =
    match CStr::from_bytes_with_nul(concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes()) {
        Ok(s) => s,
        Err(_) => panic!("CARGO_PKG_VERSION contains an interior NUL byte"),
    };

/// Read-only version string.
pub static VERSION: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(Some(VERSION_CSTR));

/// Comma-separated list of physical slot names that must confirm before logical
/// data is sent.  Primary-side setting.
pub static STANDBY_SLOT_NAMES: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(Some(c""));

/// How many of the named standby slots must confirm.  -1 = all, 0 = disabled.
pub static STANDBY_SLOTS_MIN_CONFIRMED: GucSetting<i32> = GucSetting::<i32>::new(-1);

/// Slot filter for which slots to sync to standby.
/// Format: `key:value` pairs separated by commas.
/// Keys: `name`, `name_like`, `plugin`.
pub static SYNCHRONIZE_SLOT_NAMES: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(Some(c"name_like:%%"));

/// Whether to drop extra local logical slots not found on primary.
pub static DROP_EXTRA_SLOTS: GucSetting<bool> = GucSetting::<bool>::new(true);

/// DSN for connecting to primary.  Falls back to `primary_conninfo`.
pub static PRIMARY_DSN: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c""));

/// Worker nap time between sync cycles in milliseconds.
pub static WORKER_NAP_TIME: GucSetting<i32> = GucSetting::<i32>::new(60_000);

/// Database name to use for primary connections.
pub static MAINTENANCE_DB: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(Some(c"postgres"));

// ---------------------------------------------------------------------------
// Filter types used when parsing `synchronize_slot_names`
// ---------------------------------------------------------------------------

/// The kind of filter to apply when matching slots on the primary.
#[derive(Debug, Clone, PartialEq)]
pub enum FailoverSlotFilterKey {
    /// Exact slot name match.
    Name,
    /// SQL `LIKE` pattern match on slot name.
    NameLike,
    /// Exact output-plugin name match.
    Plugin,
}

/// A single parsed filter entry from the `synchronize_slot_names` GUC.
#[derive(Debug, Clone)]
pub struct FailoverSlotFilter {
    pub key: FailoverSlotFilterKey,
    pub val: String,
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Parse the `synchronize_slot_names` GUC value into a list of filters.
///
/// Input format (comma-separated):
///   `key1:val1, key2:val2, ...`
///
/// Recognised keys: `name`, `name_like`, `plugin`.
pub fn parse_synchronize_slot_names(raw: &str) -> Vec<FailoverSlotFilter> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut filters = Vec::new();

    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        if let Some((key_str, val_str)) = part.split_once(':') {
            let key_str = key_str.trim();
            let val_str = val_str.trim();

            let key = match key_str {
                "name" => FailoverSlotFilterKey::Name,
                "name_like" => FailoverSlotFilterKey::NameLike,
                "plugin" => FailoverSlotFilterKey::Plugin,
                other => {
                    warning!(
                        "pg_failover_slots_rs: unknown synchronize_slot_names key: {}",
                        other
                    );
                    continue;
                }
            };

            filters.push(FailoverSlotFilter {
                key,
                val: val_str.to_string(),
            });
        } else {
            // Bare word without ':' defaults to exact name match (matches C behavior)
            filters.push(FailoverSlotFilter {
                key: FailoverSlotFilterKey::Name,
                val: part.to_string(),
            });
        }
    }

    filters
}

/// Parse the `standby_slot_names` GUC value into a list of slot name strings.
pub fn parse_standby_slot_names(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// GUC value accessors (convenience wrappers)
// ---------------------------------------------------------------------------

/// Helper to extract a string from GucSetting<Option<CString>>.
fn get_guc_string(setting: &GucSetting<Option<CString>>) -> String {
    setting
        .get()
        .map(|c| c.to_str().unwrap_or("").to_string())
        .unwrap_or_default()
}

/// Get `standby_slot_names` as a parsed list of slot names.
pub fn get_standby_slot_names() -> Vec<String> {
    parse_standby_slot_names(&get_guc_string(&STANDBY_SLOT_NAMES))
}

/// Get `standby_slots_min_confirmed` value.
pub fn get_standby_slots_min_confirmed() -> i32 {
    STANDBY_SLOTS_MIN_CONFIRMED.get()
}

/// Get `synchronize_slot_names` as a parsed list of filters.
pub fn get_synchronize_slot_names() -> Vec<FailoverSlotFilter> {
    parse_synchronize_slot_names(&get_guc_string(&SYNCHRONIZE_SLOT_NAMES))
}

/// Whether to drop slots not found on the primary.
pub fn get_drop_extra_slots() -> bool {
    DROP_EXTRA_SLOTS.get()
}

/// Get the DSN for connecting to the primary.  May be empty.
pub fn get_primary_dsn() -> String {
    get_guc_string(&PRIMARY_DSN)
}

/// Get the worker nap time in milliseconds.
pub fn get_worker_nap_time() -> i32 {
    WORKER_NAP_TIME.get()
}

/// Get the maintenance database name.
pub fn get_maintenance_db() -> String {
    let val = get_guc_string(&MAINTENANCE_DB);
    if val.is_empty() {
        "postgres".to_string()
    } else {
        val
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all GUC variables.  Called from `_PG_init`.
pub fn register_gucs() {
    GucRegistry::define_string_guc(
        c"pg_failover_slots_rs.version",
        c"pg_failover_slots_rs extension version",
        c"Read-only version of the pg_failover_slots_rs extension.",
        &VERSION,
        GucContext::Internal,
        GucFlags::default(),
    );

    GucRegistry::define_string_guc(
        c"pg_failover_slots_rs.standby_slot_names",
        c"Physical replication slot names that must confirm before logical data is sent",
        c"Comma-separated list of physical slot names. Primary-side setting.",
        &STANDBY_SLOT_NAMES,
        GucContext::Sighup,
        GucFlags::default(),
    );

    GucRegistry::define_int_guc(
        c"pg_failover_slots_rs.standby_slots_min_confirmed",
        c"How many standby slots must confirm before logical data is sent",
        c"-1 means all slots must confirm, 0 disables the wait. Range: [-1, 100].",
        &STANDBY_SLOTS_MIN_CONFIRMED,
        -1,
        100,
        GucContext::Sighup,
        GucFlags::default(),
    );

    GucRegistry::define_string_guc(
        c"pg_failover_slots_rs.synchronize_slot_names",
        c"Slot filters for which slots to synchronize to standby",
        c"Comma-separated key:value pairs. Keys: name, name_like, plugin.",
        &SYNCHRONIZE_SLOT_NAMES,
        GucContext::Sighup,
        GucFlags::default(),
    );

    GucRegistry::define_bool_guc(
        c"pg_failover_slots_rs.drop_extra_slots",
        c"Whether to drop local logical slots not found on primary",
        c"When true, logical slots on the standby not on the primary will be dropped.",
        &DROP_EXTRA_SLOTS,
        GucContext::Sighup,
        GucFlags::default(),
    );

    GucRegistry::define_string_guc(
        c"pg_failover_slots_rs.primary_dsn",
        c"Connection string for connecting to the primary",
        c"If empty, falls back to primary_conninfo from the WAL receiver. Superuser-only.",
        &PRIMARY_DSN,
        GucContext::Sighup,
        GucFlags::SUPERUSER_ONLY,
    );

    GucRegistry::define_int_guc(
        c"pg_failover_slots_rs.worker_nap_time",
        c"Sleep time between synchronization cycles (ms)",
        c"How long the background worker sleeps between slot sync cycles, in milliseconds.",
        &WORKER_NAP_TIME,
        1_000,
        i32::MAX,
        GucContext::Sighup,
        GucFlags::SUPERUSER_ONLY | GucFlags::UNIT_MS,
    );

    GucRegistry::define_string_guc(
        c"pg_failover_slots_rs.maintenance_db",
        c"Database to use for connections to the primary",
        c"Name of the database to connect to when querying the primary.",
        &MAINTENANCE_DB,
        GucContext::Sighup,
        GucFlags::SUPERUSER_ONLY,
    );
}
