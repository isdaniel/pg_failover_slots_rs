//! Remote connection and slot information fetching via libpq.
//!
//! Provides:
//! - RAII wrappers for PGconn / PGresult using `libpq_sys` crate
//! - [`RemoteSlot`] data structure
//! - Connection string construction
//! - Functions to query the primary server for slot information

use crate::guc::{self, FailoverSlotFilter, FailoverSlotFilterKey};
use pgrx::prelude::*;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

// ---------------------------------------------------------------------------
// Safe RAII wrappers around libpq_sys types
// ---------------------------------------------------------------------------

/// RAII wrapper around a libpq `PGconn`.  Calls `PQfinish` on drop.
pub struct PgConn {
    conn: *mut libpq_sys::PGconn,
}

// SAFETY: PGconn is a C pointer managed by libpq.  We ensure single-threaded
// access through the PostgreSQL BGW model (one worker, no thread sharing).
unsafe impl Send for PgConn {}

impl PgConn {
    /// Raw pointer (for passing to libpq functions).
    #[allow(dead_code)]
    pub fn as_ptr(&self) -> *mut libpq_sys::PGconn {
        self.conn
    }

    /// Server version (e.g., 160000 for PG16).
    pub fn server_version(&self) -> i32 {
        unsafe { libpq_sys::PQserverVersion(self.conn) }
    }

    /// Execute a query, returning a `PgResult`.
    pub fn exec(&self, query: &str) -> PgResult {
        let cquery = CString::new(query).expect("query contained NUL byte");
        let res = unsafe { libpq_sys::PQexec(self.conn, cquery.as_ptr()) };
        PgResult { res }
    }

    /// Escape a string literal for use in SQL.  Returns the escaped string
    /// including surrounding single quotes.
    pub fn escape_literal(&self, s: &str) -> String {
        let cs = CString::new(s).expect("escape literal NUL");
        let ptr =
            unsafe { libpq_sys::PQescapeLiteral(self.conn, cs.as_ptr(), s.len()) };
        if ptr.is_null() {
            error!("pg_failover_slots_rs: PQescapeLiteral failed (out of memory)");
        }
        let escaped = unsafe { CStr::from_ptr(ptr) }
            .to_str()
            .unwrap_or("")
            .to_string();
        unsafe { libpq_sys::PQfreemem(ptr as *mut std::ffi::c_void) };
        escaped
    }
}

impl Drop for PgConn {
    fn drop(&mut self) {
        if !self.conn.is_null() {
            unsafe { libpq_sys::PQfinish(self.conn) };
        }
    }
}

/// RAII wrapper around a libpq `PGresult`.  Calls `PQclear` on drop.
pub struct PgResult {
    res: *mut libpq_sys::PGresult,
}

impl PgResult {
    pub fn status(&self) -> libpq_sys::ExecStatusType {
        unsafe { libpq_sys::PQresultStatus(self.res) }
    }

    pub fn error_message(&self) -> String {
        if self.res.is_null() {
            return "(null result)".to_string();
        }
        let ptr = unsafe { libpq_sys::PQresultErrorMessage(self.res) };
        if ptr.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(ptr) }
            .to_str()
            .unwrap_or("")
            .to_string()
    }

    pub fn ntuples(&self) -> i32 {
        unsafe { libpq_sys::PQntuples(self.res) }
    }

    pub fn get_value(&self, row: i32, col: i32) -> String {
        let ptr = unsafe { libpq_sys::PQgetvalue(self.res, row, col) };
        if ptr.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(ptr) }
            .to_str()
            .unwrap_or("")
            .to_string()
    }

    pub fn is_null(&self, row: i32, col: i32) -> bool {
        unsafe { libpq_sys::PQgetisnull(self.res, row, col) != 0 }
    }

    pub fn is_tuples_ok(&self) -> bool {
        self.status() == libpq_sys::ExecStatusType::PGRES_TUPLES_OK
    }
}

impl Drop for PgResult {
    fn drop(&mut self) {
        if !self.res.is_null() {
            unsafe { libpq_sys::PQclear(self.res) };
        }
    }
}

// ---------------------------------------------------------------------------
// RemoteSlot — mirrors the C `RemoteSlot` struct
// ---------------------------------------------------------------------------

/// Information about a replication slot on the primary server.
#[derive(Debug, Clone)]
pub struct RemoteSlot {
    pub name: String,
    pub plugin: String,
    pub database: String,
    pub two_phase: bool,
    pub catalog_xmin: pg_sys::TransactionId,
    pub restart_lsn: pg_sys::XLogRecPtr,
    pub confirmed_lsn: pg_sys::XLogRecPtr,
}

// ---------------------------------------------------------------------------
// Connection helpers
// ---------------------------------------------------------------------------

/// Build the DSN string for connecting to the primary.
///
/// If `pg_failover_slots_rs.primary_dsn` is set, use it (appending `dbname`).
/// Otherwise fall back to the WAL receiver's `conninfo`.
pub fn make_sync_failover_slots_dsn(db_name: &str) -> String {
    let dsn = guc::get_primary_dsn();
    if !dsn.is_empty() {
        format!("{dsn} dbname={db_name}")
    } else {
        // Fall back to WalRcv->conninfo
        let conninfo = unsafe {
            let wal_rcv = pg_sys::WalRcv;
            if wal_rcv.is_null() {
                error!("pg_failover_slots_rs: WalRcv is NULL, cannot build DSN");
            }
            CStr::from_ptr((*wal_rcv).conninfo.as_ptr())
                .to_str()
                .unwrap_or("")
                .to_string()
        };
        format!("{conninfo} dbname={db_name}")
    }
}

/// Connect to a remote PostgreSQL server via libpq.
///
/// Returns `None` if the connection fails (logs a warning instead of
/// crashing the BGW with `error!`).
pub fn remote_connect(connstr: &str, appname: &str) -> Option<PgConn> {
    let dbname_key = CString::new("dbname").unwrap();
    let appname_key = CString::new("application_name").unwrap();
    let timeout_key = CString::new("connect_timeout").unwrap();
    let keepalives_key = CString::new("keepalives").unwrap();
    let idle_key = CString::new("keepalives_idle").unwrap();
    let interval_key = CString::new("keepalives_interval").unwrap();
    let count_key = CString::new("keepalives_count").unwrap();

    let connstr_c = CString::new(connstr).unwrap();
    let appname_c = CString::new(appname).unwrap();
    let timeout_val = CString::new("30").unwrap();
    let one = CString::new("1").unwrap();
    let twenty = CString::new("20").unwrap();
    let five = CString::new("5").unwrap();

    let keys: [*const c_char; 8] = [
        dbname_key.as_ptr(),
        appname_key.as_ptr(),
        timeout_key.as_ptr(),
        keepalives_key.as_ptr(),
        idle_key.as_ptr(),
        interval_key.as_ptr(),
        count_key.as_ptr(),
        std::ptr::null(),
    ];
    let vals: [*const c_char; 8] = [
        connstr_c.as_ptr(),
        appname_c.as_ptr(),
        timeout_val.as_ptr(),
        one.as_ptr(),
        twenty.as_ptr(),
        twenty.as_ptr(),
        five.as_ptr(),
        std::ptr::null(),
    ];

    let conn = unsafe {
        libpq_sys::PQconnectdbParams(keys.as_ptr(), vals.as_ptr(), 1 /* expand_dbname */)
    };

    if conn.is_null()
        || unsafe { libpq_sys::PQstatus(conn) }
            != libpq_sys::ConnStatusType::CONNECTION_OK
    {
        let errmsg = if conn.is_null() {
            "null connection".to_string()
        } else {
            let ptr = unsafe { libpq_sys::PQerrorMessage(conn) };
            let msg = unsafe { CStr::from_ptr(ptr) }
                .to_str()
                .unwrap_or("unknown")
                .to_string();
            unsafe { libpq_sys::PQfinish(conn) };
            msg
        };
        warning!(
            "pg_failover_slots_rs: could not connect to the postgresql server: {}",
            errmsg
        );
        return None;
    }

    log!(
        "pg_failover_slots_rs: established connection to remote backend with pid {}",
        unsafe { libpq_sys::PQbackendPID(conn) }
    );

    Some(PgConn { conn })
}

// ---------------------------------------------------------------------------
// Filter query builder
// ---------------------------------------------------------------------------

/// Build a SQL query to fetch slot info from the primary, filtering by the
/// given list of slot filters.
pub fn build_slot_filter_query(
    conn: &PgConn,
    filters: &[FailoverSlotFilter],
) -> String {
    let sv = conn.server_version();
    let _ = sv; // server version available for future use; PG15+ always has two_phase
    let base =
        "SELECT slot_name, plugin, database, two_phase, catalog_xmin, \
         restart_lsn, confirmed_flush_lsn \
         FROM pg_catalog.pg_replication_slots \
         WHERE database IS NOT NULL AND (";

    let mut query = String::from(base);
    let mut op = "";

    for f in filters {
        let escaped = conn.escape_literal(&f.val);
        match f.key {
            FailoverSlotFilterKey::Name => {
                query.push_str(&format!(
                    " {op} slot_name OPERATOR(pg_catalog.=) {escaped}"
                ));
            }
            FailoverSlotFilterKey::NameLike => {
                query.push_str(&format!(" {op} slot_name LIKE {escaped}"));
            }
            FailoverSlotFilterKey::Plugin => {
                query.push_str(&format!(
                    " {op} plugin OPERATOR(pg_catalog.=) {escaped}"
                ));
            }
        }
        op = "OR";
    }

    query.push(')');
    query
}

// ---------------------------------------------------------------------------
// Remote queries
// ---------------------------------------------------------------------------

/// Fetch slot information from the primary server.
///
/// Returns an empty `Vec` if the query fails (logs a warning).
pub fn remote_get_primary_slot_info(
    conn: &PgConn,
    filters: &[FailoverSlotFilter],
) -> Vec<RemoteSlot> {
    if filters.is_empty() {
        return Vec::new();
    }

    let query = build_slot_filter_query(conn, filters);
    let res = conn.exec(&query);

    if !res.is_tuples_ok() {
        warning!(
            "pg_failover_slots_rs: could not fetch slot information from provider: {}",
            res.error_message()
        );
        return Vec::new();
    }

    let mut slots = Vec::new();
    for i in 0..res.ntuples() {
        let name = res.get_value(i, 0);
        let plugin = res.get_value(i, 1);
        let database = res.get_value(i, 2);
        let two_phase = res.get_value(i, 3) == "t";
        let catalog_xmin = if res.is_null(i, 4) {
            pg_sys::InvalidTransactionId
        } else {
            pg_sys::TransactionId::from_inner(
                res.get_value(i, 4).parse::<u32>().unwrap_or(0),
            )
        };
        let restart_lsn = if res.is_null(i, 5) {
            0u64
        } else {
            parse_lsn(&res.get_value(i, 5))
        };
        let confirmed_lsn = if res.is_null(i, 6) {
            0u64
        } else {
            parse_lsn(&res.get_value(i, 6))
        };

        slots.push(RemoteSlot {
            name,
            plugin,
            database,
            two_phase,
            catalog_xmin,
            restart_lsn,
            confirmed_lsn,
        });
    }

    slots
}

/// Fetch the restart_lsn of a physical replication slot on the primary.
///
/// Returns `INVALID_XLOG_REC_PTR` (0) if the slot is not found or the query
/// fails (logs a warning).
pub fn remote_get_physical_slot_lsn(conn: &PgConn, slot_name: &str) -> pg_sys::XLogRecPtr {
    let escaped = conn.escape_literal(slot_name);
    let query = format!(
        "SELECT restart_lsn FROM pg_catalog.pg_replication_slots \
         WHERE slot_name OPERATOR(pg_catalog.=) {escaped}"
    );

    let res = conn.exec(&query);

    if !res.is_tuples_ok() {
        warning!(
            "pg_failover_slots_rs: could not fetch physical slot LSN from provider: {}",
            res.error_message()
        );
        return 0u64;
    }

    if res.ntuples() != 1 {
        warning!(
            "pg_failover_slots_rs: physical slot {} not found on primary (got {} rows)",
            slot_name,
            res.ntuples()
        );
        return 0u64;
    }

    if res.is_null(0, 0) {
        0u64
    } else {
        parse_lsn(&res.get_value(0, 0))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse an LSN string like "0/16B3748" into an XLogRecPtr (u64).
pub fn parse_lsn(s: &str) -> pg_sys::XLogRecPtr {
    let s = s.trim();
    if s.is_empty() {
        return 0u64;
    }
    if let Some((hi_str, lo_str)) = s.split_once('/') {
        let hi = u64::from_str_radix(hi_str, 16).unwrap_or(0);
        let lo = u64::from_str_radix(lo_str, 16).unwrap_or(0);
        (hi << 32) | lo
    } else {
        0u64
    }
}
