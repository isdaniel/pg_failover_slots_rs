// Comprehensive unit/integration tests for pg_failover_slots_rs.
//
// Run with `cargo pgrx test pg16` (or pg15, pg17, pg18).
//
// NOTE: This file is `include!()`-d from lib.rs inside a `#[pgrx::pg_schema]`
// module, so items here live in the `crate::tests` namespace.

use pgrx::prelude::*;

// =========================================================================
// GUC default value tests
// =========================================================================

#[pg_test]
fn test_guc_defaults() {
    let filters = crate::guc::get_synchronize_slot_names();
    assert!(!filters.is_empty());
    assert_eq!(filters[0].key, crate::guc::FailoverSlotFilterKey::NameLike);

    assert!(crate::guc::get_drop_extra_slots());
    assert_eq!(crate::guc::get_standby_slots_min_confirmed(), -1);
    assert_eq!(crate::guc::get_worker_nap_time(), 60_000);
    assert_eq!(crate::guc::get_maintenance_db(), "postgres");
}

#[pg_test]
fn test_guc_primary_dsn_default_empty() {
    assert!(crate::guc::get_primary_dsn().is_empty());
}

#[pg_test]
fn test_guc_standby_slot_names_default_empty() {
    let names = crate::guc::get_standby_slot_names();
    assert!(names.is_empty());
}

// =========================================================================
// SQL-callable function tests
// =========================================================================

#[pg_test]
fn test_pg_failover_slots_rs_version_fn() {
    assert_eq!(crate::pg_failover_slots_rs_version(), env!("CARGO_PKG_VERSION"));
}

#[pg_test]
fn test_pg_failover_slots_rs_version_via_spi() {
    let result = Spi::get_one::<String>("SELECT pg_failover_slots_rs_version()");
    assert_eq!(result.unwrap(), Some(env!("CARGO_PKG_VERSION").to_string()));
}

#[pg_test]
fn test_version_guc_matches_cargo() {
    let guc_val = crate::guc::VERSION.get().expect("version GUC should be set");
    assert_eq!(guc_val.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
}

// =========================================================================
// LSN parsing tests
// =========================================================================

#[pg_test]
fn test_parse_lsn_round_trip() {
    let lsn = crate::remote::parse_lsn("A/BCDEF012");
    let hi = (lsn >> 32) as u32;
    let lo = lsn as u32;
    assert_eq!(hi, 0xA);
    assert_eq!(lo, 0xBCDEF012);
}

#[pg_test]
fn test_parse_lsn_empty() {
    assert_eq!(crate::remote::parse_lsn(""), 0u64);
}

#[pg_test]
fn test_parse_lsn_no_slash() {
    assert_eq!(crate::remote::parse_lsn("12345"), 0u64);
}

#[pg_test]
fn test_parse_lsn_zero() {
    assert_eq!(crate::remote::parse_lsn("0/0"), 0u64);
}

#[pg_test]
fn test_parse_lsn_valid() {
    let lsn = crate::remote::parse_lsn("0/16B3748");
    assert_eq!(lsn, 0x0000_0000_016B_3748);
}

#[pg_test]
fn test_parse_lsn_high_bits() {
    let lsn = crate::remote::parse_lsn("1/2A");
    assert_eq!(lsn, (1u64 << 32) | 0x2A);
}

#[pg_test]
fn test_parse_lsn_max_values() {
    let lsn = crate::remote::parse_lsn("FFFFFFFF/FFFFFFFF");
    assert_eq!(lsn, u64::MAX);
}

#[pg_test]
fn test_parse_lsn_whitespace() {
    let lsn = crate::remote::parse_lsn("  0/16B3748  ");
    assert_eq!(lsn, 0x0000_0000_016B_3748);
}

#[pg_test]
fn test_parse_lsn_lowercase() {
    let lsn = crate::remote::parse_lsn("a/bcdef012");
    assert_eq!((lsn >> 32) as u32, 0xA);
    assert_eq!(lsn as u32, 0xBCDEF012);
}

#[pg_test]
fn test_parse_lsn_only_slash() {
    assert_eq!(crate::remote::parse_lsn("/"), 0u64);
}

#[pg_test]
fn test_parse_lsn_missing_high() {
    let lsn = crate::remote::parse_lsn("/1234");
    assert_eq!(lsn, 0x1234);
}

#[pg_test]
fn test_parse_lsn_missing_low() {
    let lsn = crate::remote::parse_lsn("1/");
    assert_eq!(lsn, 1u64 << 32);
}

#[pg_test]
fn test_parse_lsn_mixed_case() {
    let lsn = crate::remote::parse_lsn("Ab/cDeF");
    assert_eq!((lsn >> 32) as u32, 0xAB);
    assert_eq!(lsn as u32, 0xCDEF);
}

#[pg_test]
fn test_parse_lsn_typical_pg_output() {
    // Typical pg_lsn output from a real system
    let lsn = crate::remote::parse_lsn("0/3000060");
    assert_eq!(lsn, 0x3000060u64);
}

#[pg_test]
fn test_parse_lsn_large_high() {
    let lsn = crate::remote::parse_lsn("FF/0");
    assert_eq!(lsn, 0xFF_0000_0000u64);
}

#[pg_test]
fn test_parse_lsn_both_parts_nonzero() {
    let lsn = crate::remote::parse_lsn("2/ABCD0000");
    assert_eq!((lsn >> 32) as u32, 2);
    assert_eq!(lsn as u32, 0xABCD0000);
}

// =========================================================================
// synchronize_slot_names parsing tests
// =========================================================================

#[pg_test]
fn test_parse_sync_slot_names() {
    let filters =
        crate::guc::parse_synchronize_slot_names("name:slot1, plugin:test_decoding");
    assert_eq!(filters.len(), 2);
    assert_eq!(filters[0].key, crate::guc::FailoverSlotFilterKey::Name);
    assert_eq!(filters[0].val, "slot1");
    assert_eq!(filters[1].key, crate::guc::FailoverSlotFilterKey::Plugin);
    assert_eq!(filters[1].val, "test_decoding");
}

#[pg_test]
fn test_parse_sync_bare_word() {
    let filters = crate::guc::parse_synchronize_slot_names("my_slot");
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].key, crate::guc::FailoverSlotFilterKey::Name);
    assert_eq!(filters[0].val, "my_slot");
}

#[pg_test]
fn test_parse_sync_default_all() {
    let filters = crate::guc::parse_synchronize_slot_names("name_like:%%");
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].key, crate::guc::FailoverSlotFilterKey::NameLike);
    assert_eq!(filters[0].val, "%%");
}

#[pg_test]
fn test_parse_sync_empty_string() {
    let filters = crate::guc::parse_synchronize_slot_names("");
    assert!(filters.is_empty());
}

#[pg_test]
fn test_parse_sync_whitespace_only() {
    let filters = crate::guc::parse_synchronize_slot_names("   ");
    assert!(filters.is_empty());
}

#[pg_test]
fn test_parse_sync_trailing_comma() {
    let filters = crate::guc::parse_synchronize_slot_names("name:slot1,");
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].val, "slot1");
}

#[pg_test]
fn test_parse_sync_leading_comma() {
    let filters = crate::guc::parse_synchronize_slot_names(",name:slot1");
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].val, "slot1");
}

#[pg_test]
fn test_parse_sync_multiple_bare_words() {
    let filters = crate::guc::parse_synchronize_slot_names("slot1, slot2, slot3");
    assert_eq!(filters.len(), 3);
    for f in &filters {
        assert_eq!(f.key, crate::guc::FailoverSlotFilterKey::Name);
    }
    assert_eq!(filters[0].val, "slot1");
    assert_eq!(filters[1].val, "slot2");
    assert_eq!(filters[2].val, "slot3");
}

#[pg_test]
fn test_parse_sync_all_filter_types() {
    let filters = crate::guc::parse_synchronize_slot_names(
        "name:exact_slot, name_like:prefix_%, plugin:pgoutput",
    );
    assert_eq!(filters.len(), 3);
    assert_eq!(filters[0].key, crate::guc::FailoverSlotFilterKey::Name);
    assert_eq!(filters[0].val, "exact_slot");
    assert_eq!(filters[1].key, crate::guc::FailoverSlotFilterKey::NameLike);
    assert_eq!(filters[1].val, "prefix_%");
    assert_eq!(filters[2].key, crate::guc::FailoverSlotFilterKey::Plugin);
    assert_eq!(filters[2].val, "pgoutput");
}

#[pg_test]
fn test_parse_sync_unknown_key_skipped() {
    // Unknown keys should be skipped with a warning (not panic)
    let filters = crate::guc::parse_synchronize_slot_names("bogus:val, name:good");
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].val, "good");
}

#[pg_test]
fn test_parse_sync_extra_whitespace() {
    let filters =
        crate::guc::parse_synchronize_slot_names("  name : slot1 , plugin : test_decoding  ");
    assert_eq!(filters.len(), 2);
    assert_eq!(filters[0].key, crate::guc::FailoverSlotFilterKey::Name);
    assert_eq!(filters[0].val, "slot1");
    assert_eq!(filters[1].key, crate::guc::FailoverSlotFilterKey::Plugin);
    assert_eq!(filters[1].val, "test_decoding");
}

#[pg_test]
fn test_parse_sync_name_like_with_percent() {
    let filters = crate::guc::parse_synchronize_slot_names("name_like:sub_%");
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].key, crate::guc::FailoverSlotFilterKey::NameLike);
    assert_eq!(filters[0].val, "sub_%");
}

#[pg_test]
fn test_parse_sync_name_like_with_underscore() {
    let filters = crate::guc::parse_synchronize_slot_names("name_like:slot_");
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].val, "slot_");
}

#[pg_test]
fn test_parse_sync_mixed_bare_and_keyed() {
    let filters = crate::guc::parse_synchronize_slot_names("my_slot, plugin:pgoutput, other_slot");
    assert_eq!(filters.len(), 3);
    assert_eq!(filters[0].key, crate::guc::FailoverSlotFilterKey::Name);
    assert_eq!(filters[0].val, "my_slot");
    assert_eq!(filters[1].key, crate::guc::FailoverSlotFilterKey::Plugin);
    assert_eq!(filters[1].val, "pgoutput");
    assert_eq!(filters[2].key, crate::guc::FailoverSlotFilterKey::Name);
    assert_eq!(filters[2].val, "other_slot");
}

#[pg_test]
fn test_parse_sync_multiple_unknown_keys() {
    let filters = crate::guc::parse_synchronize_slot_names("bad1:x, bad2:y, name:ok");
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].val, "ok");
}

#[pg_test]
fn test_parse_sync_colon_in_value() {
    // Value with colon: key is everything before first colon
    let filters = crate::guc::parse_synchronize_slot_names("name:host:port");
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].key, crate::guc::FailoverSlotFilterKey::Name);
    assert_eq!(filters[0].val, "host:port");
}

// =========================================================================
// standby_slot_names parsing tests
// =========================================================================

#[pg_test]
fn test_parse_standby_slot_names() {
    let names = crate::guc::parse_standby_slot_names("slot1, slot2, slot3");
    assert_eq!(names, vec!["slot1", "slot2", "slot3"]);
}

#[pg_test]
fn test_parse_standby_slot_names_empty() {
    let names = crate::guc::parse_standby_slot_names("");
    assert!(names.is_empty());
}

#[pg_test]
fn test_parse_standby_slot_names_single() {
    let names = crate::guc::parse_standby_slot_names("my_slot");
    assert_eq!(names, vec!["my_slot"]);
}

#[pg_test]
fn test_parse_standby_slot_names_whitespace() {
    let names = crate::guc::parse_standby_slot_names("  slot1 , slot2 ,  slot3  ");
    assert_eq!(names, vec!["slot1", "slot2", "slot3"]);
}

#[pg_test]
fn test_parse_standby_slot_names_trailing_comma() {
    let names = crate::guc::parse_standby_slot_names("slot1,slot2,");
    assert_eq!(names, vec!["slot1", "slot2"]);
}

#[pg_test]
fn test_parse_standby_slot_names_double_comma() {
    // Double comma should produce no empty entries
    let names = crate::guc::parse_standby_slot_names("slot1,,slot2");
    assert_eq!(names, vec!["slot1", "slot2"]);
}

#[pg_test]
fn test_parse_standby_slot_names_whitespace_only() {
    let names = crate::guc::parse_standby_slot_names("   ");
    assert!(names.is_empty());
}

#[pg_test]
fn test_parse_standby_slot_names_many_slots() {
    let names =
        crate::guc::parse_standby_slot_names("a, b, c, d, e, f, g, h, i, j");
    assert_eq!(names.len(), 10);
    assert_eq!(names[0], "a");
    assert_eq!(names[9], "j");
}

// =========================================================================
// RemoteSlot struct tests
// =========================================================================

#[pg_test]
fn test_remote_slot_struct() {
    let slot = crate::remote::RemoteSlot {
        name: "test_slot".to_string(),
        plugin: "test_decoding".to_string(),
        database: "testdb".to_string(),
        two_phase: false,
        catalog_xmin: pg_sys::InvalidTransactionId,
        restart_lsn: 0u64,
        confirmed_lsn: 0u64,
    };
    assert_eq!(slot.name, "test_slot");
    assert_eq!(slot.plugin, "test_decoding");
    assert_eq!(slot.database, "testdb");
    assert!(!slot.two_phase);
    assert_eq!(slot.restart_lsn, 0);
    assert_eq!(slot.confirmed_lsn, 0);
}

#[pg_test]
fn test_remote_slot_clone() {
    let slot = crate::remote::RemoteSlot {
        name: "original".to_string(),
        plugin: "pgoutput".to_string(),
        database: "mydb".to_string(),
        two_phase: true,
        catalog_xmin: pg_sys::InvalidTransactionId,
        restart_lsn: 0xA_0000_0001u64,
        confirmed_lsn: 0xA_0000_0002u64,
    };
    let cloned = slot.clone();
    assert_eq!(slot.name, cloned.name);
    assert_eq!(slot.plugin, cloned.plugin);
    assert_eq!(slot.database, cloned.database);
    assert_eq!(slot.restart_lsn, cloned.restart_lsn);
    assert_eq!(slot.confirmed_lsn, cloned.confirmed_lsn);
    assert_eq!(slot.two_phase, cloned.two_phase);
    assert_eq!(slot.catalog_xmin, cloned.catalog_xmin);
}

#[pg_test]
fn test_remote_slot_with_lsn_values() {
    let slot = crate::remote::RemoteSlot {
        name: "logical_slot".to_string(),
        plugin: "pgoutput".to_string(),
        database: "production".to_string(),
        two_phase: false,
        catalog_xmin: pg_sys::InvalidTransactionId,
        restart_lsn: 0x0000_0001_0000_0000u64, // 1/0
        confirmed_lsn: 0x0000_0001_0000_00F0u64, // 1/F0
    };
    assert!(slot.confirmed_lsn > slot.restart_lsn);
    assert_eq!((slot.restart_lsn >> 32) as u32, 1);
}

#[pg_test]
fn test_remote_slot_debug_format() {
    let slot = crate::remote::RemoteSlot {
        name: "dbg_slot".to_string(),
        plugin: "test_decoding".to_string(),
        database: "mydb".to_string(),
        two_phase: false,
        catalog_xmin: pg_sys::InvalidTransactionId,
        restart_lsn: 0u64,
        confirmed_lsn: 0u64,
    };
    let debug_str = format!("{:?}", slot);
    assert!(debug_str.contains("dbg_slot"));
    assert!(debug_str.contains("test_decoding"));
}

// =========================================================================
// pg_compat helper tests
// =========================================================================

#[pg_test]
fn test_invalid_xlog_rec_ptr() {
    assert_eq!(crate::pg_compat::INVALID_XLOG_REC_PTR, 0u64);
}

#[pg_test]
fn test_timestamp_tz_plus_milliseconds() {
    let ts: pg_sys::TimestampTz = 1_000_000; // 1 second in microseconds
    let result = crate::pg_compat::timestamp_tz_plus_milliseconds(ts, 500);
    assert_eq!(result, 1_500_000); // 1.5 seconds in microseconds
}

#[pg_test]
fn test_timestamp_tz_plus_milliseconds_zero() {
    let result = crate::pg_compat::timestamp_tz_plus_milliseconds(0, 0);
    assert_eq!(result, 0);
}

#[pg_test]
fn test_timestamp_tz_plus_milliseconds_large() {
    let ts: pg_sys::TimestampTz = 0;
    let result = crate::pg_compat::timestamp_tz_plus_milliseconds(ts, 60_000);
    assert_eq!(result, 60_000_000); // 60 seconds in microseconds
}

#[pg_test]
fn test_timestamp_tz_plus_negative() {
    // Negative milliseconds should subtract
    let ts: pg_sys::TimestampTz = 5_000_000; // 5 seconds
    let result = crate::pg_compat::timestamp_tz_plus_milliseconds(ts, -2_000);
    assert_eq!(result, 3_000_000); // 3 seconds
}

#[pg_test]
fn test_heap_tuple_is_valid_null() {
    assert!(!crate::pg_compat::heap_tuple_is_valid(std::ptr::null_mut()));
}

#[pg_test]
fn test_invalid_transaction_id_constant() {
    assert_eq!(crate::pg_compat::INVALID_TRANSACTION_ID, pg_sys::InvalidTransactionId);
}

// =========================================================================
// Extension constants
// =========================================================================

#[pg_test]
fn test_extension_name() {
    assert_eq!(crate::EXTENSION_NAME, "pg_failover_slots_rs");
}

#[pg_test]
fn test_worker_wait_feedback() {
    assert_eq!(crate::WORKER_WAIT_FEEDBACK, 10_000);
}

#[pg_test]
fn test_worker_wait_feedback_is_positive() {
    assert!(crate::WORKER_WAIT_FEEDBACK > 0);
}

// =========================================================================
// FailoverSlotFilter equality & Debug
// =========================================================================

#[pg_test]
fn test_filter_key_equality() {
    use crate::guc::FailoverSlotFilterKey;
    assert_eq!(FailoverSlotFilterKey::Name, FailoverSlotFilterKey::Name);
    assert_eq!(FailoverSlotFilterKey::NameLike, FailoverSlotFilterKey::NameLike);
    assert_eq!(FailoverSlotFilterKey::Plugin, FailoverSlotFilterKey::Plugin);
    assert_ne!(FailoverSlotFilterKey::Name, FailoverSlotFilterKey::NameLike);
    assert_ne!(FailoverSlotFilterKey::Name, FailoverSlotFilterKey::Plugin);
    assert_ne!(FailoverSlotFilterKey::NameLike, FailoverSlotFilterKey::Plugin);
}

#[pg_test]
fn test_filter_struct_debug() {
    let filter = crate::guc::FailoverSlotFilter {
        key: crate::guc::FailoverSlotFilterKey::NameLike,
        val: "sub_%".to_string(),
    };
    let debug_str = format!("{:?}", filter);
    assert!(debug_str.contains("NameLike"));
    assert!(debug_str.contains("sub_%"));
}

#[pg_test]
fn test_filter_clone() {
    let filter = crate::guc::FailoverSlotFilter {
        key: crate::guc::FailoverSlotFilterKey::Plugin,
        val: "pgoutput".to_string(),
    };
    let cloned = filter.clone();
    assert_eq!(filter.key, cloned.key);
    assert_eq!(filter.val, cloned.val);
}

// =========================================================================
// GUC helper function tests
// =========================================================================

#[pg_test]
fn test_get_maintenance_db_default() {
    let db = crate::guc::get_maintenance_db();
    assert_eq!(db, "postgres");
}

#[pg_test]
fn test_get_worker_nap_time_default() {
    assert_eq!(crate::guc::get_worker_nap_time(), 60_000);
}

#[pg_test]
fn test_get_drop_extra_slots_default_true() {
    assert!(crate::guc::get_drop_extra_slots());
}

#[pg_test]
fn test_get_standby_slots_min_confirmed_default() {
    assert_eq!(crate::guc::get_standby_slots_min_confirmed(), -1);
}

// =========================================================================
// Build-slot-filter-query structure tests
// (These test the SQL generation without needing a remote connection)
// =========================================================================

#[pg_test]
fn test_parse_sync_name_like_empty_pattern() {
    let filters = crate::guc::parse_synchronize_slot_names("name_like:");
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].key, crate::guc::FailoverSlotFilterKey::NameLike);
    assert_eq!(filters[0].val, "");
}

#[pg_test]
fn test_parse_sync_plugin_filter() {
    let filters = crate::guc::parse_synchronize_slot_names("plugin:pgoutput");
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].key, crate::guc::FailoverSlotFilterKey::Plugin);
    assert_eq!(filters[0].val, "pgoutput");
}

#[pg_test]
fn test_parse_sync_plugin_test_decoding() {
    let filters = crate::guc::parse_synchronize_slot_names("plugin:test_decoding");
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0].key, crate::guc::FailoverSlotFilterKey::Plugin);
    assert_eq!(filters[0].val, "test_decoding");
}

// =========================================================================
// Cross-module integration: verify parse_lsn is consistent with PG behavior
// =========================================================================

#[pg_test]
fn test_parse_lsn_pg_format_consistency() {
    // PostgreSQL formats LSNs as "X/Y" where X and Y are hex without leading zeros
    // Verify our parser handles the exact format PG produces
    let lsn1 = crate::remote::parse_lsn("0/1677F20");
    assert!(lsn1 > 0);

    let lsn2 = crate::remote::parse_lsn("0/1677F28");
    assert!(lsn2 > lsn1);

    // Both should have high=0
    assert_eq!((lsn1 >> 32), 0);
    assert_eq!((lsn2 >> 32), 0);
}

#[pg_test]
fn test_parse_lsn_ordering_preserved() {
    let lsn_a = crate::remote::parse_lsn("0/1000000");
    let lsn_b = crate::remote::parse_lsn("0/2000000");
    let lsn_c = crate::remote::parse_lsn("1/0");
    assert!(lsn_a < lsn_b);
    assert!(lsn_b < lsn_c);
}
