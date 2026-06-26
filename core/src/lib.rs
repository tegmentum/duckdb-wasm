use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::mem::MaybeUninit;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr::{self, copy_nonoverlapping};
use std::slice;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Diagnostic logging that never aborts the component. The core runs as a
/// reactor component whose host may not wire `wasi:cli/stderr`; the std
/// `eprintln!` macro panics on a failed write, which would abort DuckDB
/// mid-query. This swallows write errors instead.
macro_rules! clog {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

pub(crate) use clog;

mod bindings;
mod extension_loader;
mod tvm_spill;

use bindings::duckdb::component::extension_loader_hooks;
use bindings::duckdb::extension::callback_dispatch;
use bindings::duckdb::extension::pragma_host;
use bindings::duckdb::extension::types::{
    Complexvalue, Configerror, Decimalvalue, Duckvalue, Funcflags, Intervalvalue, Logfield,
    Logicaltype, Loglevel, Uuidvalue,
};
use bindings::exports::duckdb::component::database as exported_database;
use bindings::exports::duckdb::extension::{
    config as config_exports, logging as logging_exports, runtime as runtime_exports,
};
use bindings::wasi::cli::environment;
use bindings::wasi::clocks::wall_clock::Datetime;
use bindings::wasi::filesystem::types::{
    Descriptor, DescriptorFlags, DescriptorStat, DescriptorType, DirectoryEntryStream, ErrorCode,
    OpenFlags,
};
use exported_database::{
    Capabilitykind, Columndef, Connection, ConnectionBorrow, Duckerror, ExtensionInfo, QueryResult,
    Row,
};
use extension_loader::record_extension_registration;
use libc;
use libduckdb_sys as duckdb;
use thiserror::Error;

const DUCKDB_SUCCESS: duckdb::duckdb_state = 0;

static NEXT_SCALAR_FUNCTION_ID: AtomicU32 = AtomicU32::new(1);
static SCALAR_FUNCTION_DEFINITIONS: OnceLock<Mutex<Vec<Arc<ScalarFunctionDefinition>>>> =
    OnceLock::new();
static ACTIVE_CONNECTIONS: OnceLock<Mutex<Vec<ConnectionHandle>>> = OnceLock::new();
static NEXT_TABLE_FUNCTION_ID: AtomicU32 = AtomicU32::new(1);
static TABLE_FUNCTION_DEFINITIONS: OnceLock<Mutex<Vec<Arc<TableFunctionDefinition>>>> =
    OnceLock::new();
static NEXT_AGGREGATE_FUNCTION_ID: AtomicU32 = AtomicU32::new(1);
static AGGREGATE_FUNCTION_DEFINITIONS: OnceLock<Mutex<Vec<Arc<AggregateFunctionDefinition>>>> =
    OnceLock::new();
/// ATTACH `TYPE` names for which a wasm StorageExtension has already been
/// registered on a given database (avoids re-registering on every query). Keyed
/// loosely by type-name across all databases; the C++ side also guards via
/// `StorageExtension::Find`, so a stale entry is harmless.
static STORAGE_REGISTERED_TYPES: OnceLock<Mutex<std::collections::HashSet<String>>> =
    OnceLock::new();
/// Collation names for which a wasm collation has already been registered (avoids
/// re-registering on every query). The C++ side also guards via IGNORE_ON_CONFLICT.
static COLLATION_REGISTERED_NAMES: OnceLock<Mutex<std::collections::HashSet<String>>> =
    OnceLock::new();
/// Item 4: pragmas components have declared (via `runtime.pragma-registry.register-call`),
/// pulled through the `pragma-host.pragma-list` import. Maps the PRAGMA name the
/// user types to the callback-dispatch handle for `callback-dispatch.call-pragma`.
/// The DuckDB C API has no pragma-creation function, so the core intercepts
/// `PRAGMA <name>(...)` in its query path: it dispatches the pragma (the component
/// RETURNS a SQL script as text -- no mid-callback re-entry into SQL) and then runs
/// that script on the same connection.
static DECLARED_PRAGMAS: OnceLock<Mutex<std::collections::HashMap<String, u32>>> = OnceLock::new();
/// Registered replacement scans (file extension -> table function name).
static REPLACEMENT_SCANS: OnceLock<Mutex<Vec<ReplacementScanSpec>>> = OnceLock::new();
/// Databases that already have the global replacement-scan callback installed.
static REPLACEMENT_SCAN_DATABASES: OnceLock<Mutex<Vec<DatabaseHandle>>> = OnceLock::new();

// M1: implemented in core/cpp/wasm_storage.cpp. Registers a stub
// StorageExtension for a given ATTACH `TYPE` (currently hardcoded "sqlitewasm")
// on the freshly-opened database. Defined in C++ so it can subclass DuckDB's
// internal StorageExtension and link against the prebuilt libduckdb-wasi.a.
extern "C" {
    fn wasm_register_storage_extension(db: duckdb::duckdb_database, type_name: *const c_char);
}

// httpfs M1 (de-risk): implemented in core/cpp/wasm_files.cpp. Registers a stub
// FileSystem subsystem on the database's VirtualFileSystem that claims
// http:// / https:// paths and throws a recognizable IOException from OpenFile.
// Idempotent on the C++ side (process-wide once-guard + dup-name try/catch), so
// always-register per query is safe; this M1 path proves the mechanism. A
// dynamic gate on a files-capable component is M2.
extern "C" {
    fn wasm_register_file_system(db: duckdb::duckdb_database);
}

// Item 3 / M1 (custom-index de-risk): implemented in core/cpp/wasm_index.cpp.
// Registers a custom INDEX TYPE on the database's index-type set so
// `CREATE INDEX ... USING <type>` routes to a WasmBoundIndex stub. Idempotent on
// the C++ side (process-wide once-guard + FindByName dup-check), so always-
// registering per query before any CREATE INDEX binds is safe. M1 fixes the type
// name to "wasm_hnsw"; the dynamic per-component pull is M2.
extern "C" {
    fn wasm_register_index_type(db: duckdb::duckdb_database, type_name: *const c_char);
}

// Item 2: implemented in core/cpp/wasm_collation.cpp. Wraps an already-registered
// sort-key scalar (`transform_scalar`, text -> sort-key text) in a DuckDB
// collation named `name`, so `ORDER BY x COLLATE name` resolves to it. The C++
// side is idempotent (CreateCollation with IGNORE_ON_CONFLICT), and we also
// dedup-guard already-registered collations here. Registration is mid-session
// safe: the binder reads collations from the system catalog at bind time.
extern "C" {
    fn wasm_register_collation(
        db: duckdb::duckdb_database,
        name: *const c_char,
        transform_scalar: *const c_char,
        combinable: bool,
    );
}

// M2a: read-only foreign-catalog bridge. The C++ WasmCatalog / WasmSchemaEntry /
// WasmTableEntry call these extern-C fns; each routes to the host-provided
// `duckdb:extension/storage-host` import, which the host forwards to the
// sqlitewasm component's `storage-dispatch` export (mirroring how call-table is
// routed via callback-dispatch).
//
// C ABI choices (kept simple + leak-free; wasm is single-threaded):
//   * attach returns a u32 component-side catalog handle (0 == error).
//   * list-tables / table-columns return a malloc'd, NUL-terminated string the
//     caller MUST free with `wasm_storage_free`. Tables are '\n'-joined names.
//     Columns are '\n'-joined `name\t<duckdb_type_code>` lines (the type code is
//     a `duckdb_type` enum value: BOOLEAN=1, BIGINT=5, UBIGINT=9, DOUBLE=11,
//     VARCHAR=17, BLOB=18). A NULL return signals an error.
//   * on error, the C++ side reads `wasm_storage_last_error()` for a message.

static STORAGE_LAST_ERROR: OnceLock<Mutex<Option<CString>>> = OnceLock::new();

fn storage_set_last_error(msg: String) {
    let cell = STORAGE_LAST_ERROR.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = cell.lock() {
        *guard = CString::new(msg).ok();
    }
}

/// Maps a storage-host `Duckerror` into a single human-readable string.
fn storage_format_error(err: &bindings::duckdb::extension::storage_host::Duckerror) -> String {
    use bindings::duckdb::extension::storage_host::Duckerror as E;
    match err {
        E::Invalidargument(m) => format!("invalid argument: {m}"),
        E::Unsupported(m) => format!("unsupported: {m}"),
        E::Invalidstate(m) => format!("invalid state: {m}"),
        E::Io(m) => format!("io error: {m}"),
        E::Internal(m) => format!("internal error: {m}"),
    }
}

/// Maps a `types::Logicaltype` to the corresponding `duckdb_type` enum code that
/// the C++ side turns back into a `LogicalType`.
fn storage_logicaltype_to_code(ty: &Logicaltype) -> u32 {
    match ty {
        Logicaltype::Boolean => 1,  // DUCKDB_TYPE_BOOLEAN
        Logicaltype::Int64 => 5,    // DUCKDB_TYPE_BIGINT
        Logicaltype::Uint64 => 9,   // DUCKDB_TYPE_UBIGINT
        Logicaltype::Float64 => 11, // DUCKDB_TYPE_DOUBLE
        Logicaltype::Text => 17,    // DUCKDB_TYPE_VARCHAR
        Logicaltype::Blob => 18,    // DUCKDB_TYPE_BLOB
        Logicaltype::Int32 => 4,    // DUCKDB_TYPE_INTEGER
        Logicaltype::Timestamp => 12, // DUCKDB_TYPE_TIMESTAMP
        Logicaltype::Int8 => 2,     // DUCKDB_TYPE_TINYINT
        Logicaltype::Int16 => 3,    // DUCKDB_TYPE_SMALLINT
        Logicaltype::Uint8 => 6,    // DUCKDB_TYPE_UTINYINT
        Logicaltype::Uint16 => 7,   // DUCKDB_TYPE_USMALLINT
        Logicaltype::Uint32 => 8,   // DUCKDB_TYPE_UINTEGER
        Logicaltype::Float32 => 10, // DUCKDB_TYPE_FLOAT
        Logicaltype::Date => 13,    // DUCKDB_TYPE_DATE
        Logicaltype::Time => 14,    // DUCKDB_TYPE_TIME
        Logicaltype::Timestamptz => 31, // DUCKDB_TYPE_TIMESTAMP_TZ
        Logicaltype::Decimal => 19, // DUCKDB_TYPE_DECIMAL
        Logicaltype::Interval => 15, // DUCKDB_TYPE_INTERVAL
        Logicaltype::Uuid => 27,    // DUCKDB_TYPE_UUID
        // ESCAPE-HATCH: storage scan-column codes can't carry a nested type
        // expression; report LIST (the common nested case) as a best-effort code.
        Logicaltype::Complex(_) => 24, // DUCKDB_TYPE_LIST
    }
}

/// Returns the most recent storage-bridge error message (or an empty C string),
/// owned by the core; the pointer stays valid until the next bridge call.
#[no_mangle]
pub extern "C" fn wasm_storage_last_error() -> *const c_char {
    let cell = STORAGE_LAST_ERROR.get_or_init(|| Mutex::new(None));
    match cell.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(s) => s.as_ptr(),
            None => b"\0".as_ptr() as *const c_char,
        },
        Err(_) => b"\0".as_ptr() as *const c_char,
    }
}

/// Frees a string previously returned by a `wasm_storage_*` enumeration fn.
#[no_mangle]
pub unsafe extern "C" fn wasm_storage_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(CString::from_raw(ptr));
    }
}

/// Opens the foreign catalog named by `dsn` (an ATTACH path). Returns a
/// component-side catalog handle, or 0 on error (message in `wasm_storage_last_error`).
#[no_mangle]
pub unsafe extern "C" fn wasm_storage_attach(dsn: *const c_char) -> u32 {
    if dsn.is_null() {
        storage_set_last_error("wasm_storage_attach: null dsn".to_string());
        return 0;
    }
    let dsn_str = match CStr::from_ptr(dsn).to_str() {
        Ok(s) => s,
        Err(_) => {
            storage_set_last_error("wasm_storage_attach: dsn is not valid UTF-8".to_string());
            return 0;
        }
    };
    match bindings::duckdb::extension::storage_host::storage_attach(dsn_str) {
        Ok(handle) => handle,
        Err(err) => {
            storage_set_last_error(storage_format_error(&err));
            0
        }
    }
}

/// Enumerates `catalog`'s base tables as a '\n'-joined, NUL-terminated string.
/// Caller frees with `wasm_storage_free`. NULL on error.
#[no_mangle]
pub extern "C" fn wasm_storage_list_tables(catalog: u32) -> *mut c_char {
    match bindings::duckdb::extension::storage_host::storage_list_tables(catalog) {
        Ok(tables) => {
            let joined = tables.join("\n");
            match CString::new(joined) {
                Ok(c) => c.into_raw(),
                Err(_) => {
                    storage_set_last_error(
                        "wasm_storage_list_tables: table name contains NUL".to_string(),
                    );
                    ptr::null_mut()
                }
            }
        }
        Err(err) => {
            storage_set_last_error(storage_format_error(&err));
            ptr::null_mut()
        }
    }
}

/// Enumerates `table`'s columns as '\n'-joined `name\t<typecode>` lines.
/// Caller frees with `wasm_storage_free`. NULL on error.
#[no_mangle]
pub unsafe extern "C" fn wasm_storage_table_columns(
    catalog: u32,
    table: *const c_char,
) -> *mut c_char {
    if table.is_null() {
        storage_set_last_error("wasm_storage_table_columns: null table".to_string());
        return ptr::null_mut();
    }
    let table_str = match CStr::from_ptr(table).to_str() {
        Ok(s) => s,
        Err(_) => {
            storage_set_last_error(
                "wasm_storage_table_columns: table name is not valid UTF-8".to_string(),
            );
            return ptr::null_mut();
        }
    };
    match bindings::duckdb::extension::storage_host::storage_table_columns(catalog, table_str) {
        Ok(columns) => {
            let mut lines: Vec<String> = Vec::with_capacity(columns.len());
            for col in columns {
                if col.name.contains('\n') || col.name.contains('\t') {
                    storage_set_last_error(
                        "wasm_storage_table_columns: column name contains separator".to_string(),
                    );
                    return ptr::null_mut();
                }
                lines.push(format!(
                    "{}\t{}",
                    col.name,
                    storage_logicaltype_to_code(&col.logical)
                ));
            }
            let joined = lines.join("\n");
            match CString::new(joined) {
                Ok(c) => c.into_raw(),
                Err(_) => {
                    storage_set_last_error(
                        "wasm_storage_table_columns: column name contains NUL".to_string(),
                    );
                    ptr::null_mut()
                }
            }
        }
        Err(err) => {
            storage_set_last_error(storage_format_error(&err));
            ptr::null_mut()
        }
    }
}

//===----------------------------------------------------------------------===//
// Item 3 / M2a: custom-index build bridge.
//
// The C++ WasmBoundIndex / WasmCreateIndexPlan call these extern-C fns; each
// routes to the host-provided `duckdb:extension/index-host` import, which the
// host forwards to the index component's `index-dispatch` export. The build is
// driven create -> append -> build; explicit search is exposed component-side as
// a table function (hnsw_search), so the core does not call index-search.
//
// C ABI: create returns a component-side index-handle (0 == error); append/build
// return 0 on success, -1 on error (message in wasm_index_last_error). Vectors
// cross the ABI flattened: `vectors_flat` is n_rows*dims contiguous f32.
//===----------------------------------------------------------------------===//

static INDEX_LAST_ERROR: OnceLock<Mutex<Option<CString>>> = OnceLock::new();

fn index_set_last_error(msg: String) {
    let cell = INDEX_LAST_ERROR.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = cell.lock() {
        *guard = CString::new(msg).ok();
    }
}

fn index_format_error(err: &bindings::duckdb::extension::index_host::Duckerror) -> String {
    use bindings::duckdb::extension::index_host::Duckerror as E;
    match err {
        E::Invalidargument(m) => format!("invalid argument: {m}"),
        E::Unsupported(m) => format!("unsupported: {m}"),
        E::Invalidstate(m) => format!("invalid state: {m}"),
        E::Io(m) => format!("io error: {m}"),
        E::Internal(m) => format!("internal error: {m}"),
    }
}

/// Returns the most recent index-bridge error message (or an empty C string),
/// owned by the core; valid until the next bridge call.
#[no_mangle]
pub extern "C" fn wasm_index_last_error() -> *const c_char {
    let cell = INDEX_LAST_ERROR.get_or_init(|| Mutex::new(None));
    match cell.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(s) => s.as_ptr(),
            None => b"\0".as_ptr() as *const c_char,
        },
        Err(_) => b"\0".as_ptr() as *const c_char,
    }
}

/// Allocate an empty index builder for `(type_name, index_name)` over a
/// FLOAT[dims] key. Returns the component-side index-handle, 0 on error.
#[no_mangle]
pub unsafe extern "C" fn wasm_index_create(
    type_name: *const c_char,
    index_name: *const c_char,
    dims: u32,
) -> u32 {
    if type_name.is_null() || index_name.is_null() {
        index_set_last_error("wasm_index_create: null argument".to_string());
        return 0;
    }
    let type_str = match CStr::from_ptr(type_name).to_str() {
        Ok(s) => s,
        Err(_) => {
            index_set_last_error("wasm_index_create: type not UTF-8".to_string());
            return 0;
        }
    };
    let name_str = match CStr::from_ptr(index_name).to_str() {
        Ok(s) => s,
        Err(_) => {
            index_set_last_error("wasm_index_create: name not UTF-8".to_string());
            return 0;
        }
    };
    match bindings::duckdb::extension::index_host::index_create(type_str, name_str, dims) {
        Ok(handle) => handle,
        Err(err) => {
            index_set_last_error(index_format_error(&err));
            0
        }
    }
}

/// Append `n_rows` rows: `rowids` is n_rows i64; `vectors_flat` is n_rows*dims
/// contiguous f32. Returns 0 on success, -1 on error.
#[no_mangle]
pub unsafe extern "C" fn wasm_index_append(
    handle: u32,
    rowids: *const i64,
    n_rows: u32,
    vectors_flat: *const f32,
    dims: u32,
) -> i32 {
    if (n_rows > 0) && (rowids.is_null() || vectors_flat.is_null()) {
        index_set_last_error("wasm_index_append: null buffer".to_string());
        return -1;
    }
    let n = n_rows as usize;
    let d = dims as usize;
    let rowid_slice = if n == 0 { &[][..] } else { slice::from_raw_parts(rowids, n) };
    let flat = if n == 0 { &[][..] } else { slice::from_raw_parts(vectors_flat, n * d) };
    let rowids_vec: Vec<i64> = rowid_slice.to_vec();
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(n);
    for i in 0..n {
        vectors.push(flat[i * d..(i + 1) * d].to_vec());
    }
    match bindings::duckdb::extension::index_host::index_append(handle, &rowids_vec, &vectors) {
        Ok(()) => 0,
        Err(err) => {
            index_set_last_error(index_format_error(&err));
            -1
        }
    }
}

/// Finalize the index build. Returns 0 on success, -1 on error.
#[no_mangle]
pub extern "C" fn wasm_index_build(handle: u32) -> i32 {
    match bindings::duckdb::extension::index_host::index_build(handle) {
        Ok(()) => 0,
        Err(err) => {
            index_set_last_error(index_format_error(&err));
            -1
        }
    }
}

/// Drop the index. Returns 0 on success, -1 on error.
#[no_mangle]
pub extern "C" fn wasm_index_drop(handle: u32) -> i32 {
    match bindings::duckdb::extension::index_host::index_drop(handle) {
        Ok(()) => 0,
        Err(err) => {
            index_set_last_error(index_format_error(&err));
            -1
        }
    }
}

/// Item 3 / M2b: kNN search bridge used by the OPTIMIZER rule (not the build
/// pipeline). The C++ `wasm_index_optimizer.cpp` rewrites
/// `ORDER BY array_distance(col, const) LIMIT k` into an index scan; at optimize
/// time it calls this to ask the component for the k nearest rowids of `query`,
/// then constrains the table scan to exactly those rowids. Routes to the
/// host-provided `index-host.index-search` import (handle -> built map in the
/// component), mirroring how the explicit `hnsw_search` table function reaches
/// the SAME built index.
///
/// `query` is `dims` contiguous f32. On success, writes up to `k` rowids into
/// the caller-provided `out_rowids` buffer (length >= k) and returns the count
/// written (0..=k). Returns -1 on error (message in `wasm_index_last_error`).
#[no_mangle]
pub unsafe extern "C" fn wasm_index_search(
    handle: u32,
    query: *const f32,
    dims: u32,
    k: u32,
    out_rowids: *mut i64,
) -> i32 {
    if query.is_null() || out_rowids.is_null() {
        index_set_last_error("wasm_index_search: null buffer".to_string());
        return -1;
    }
    let q = slice::from_raw_parts(query, dims as usize).to_vec();
    match bindings::duckdb::extension::index_host::index_search(handle, &q, k) {
        Ok(hits) => {
            let n = std::cmp::min(hits.len(), k as usize);
            let out = slice::from_raw_parts_mut(out_rowids, n);
            for (i, hit) in hits.iter().take(n).enumerate() {
                out[i] = hit.rowid;
            }
            n as i32
        }
        Err(err) => {
            index_set_last_error(index_format_error(&err));
            -1
        }
    }
}

//===----------------------------------------------------------------------===//
// M2b scan bridge: projection + filter pushdown.
//
// The C++ TableFunction owns a scan handle (returned by `wasm_storage_scan_open`)
// and pulls rows with `wasm_storage_scan_fill` into a DataChunk until EOF. The
// component returns rows in PROJECTION order (each row = one Duckvalue per
// projected column); to write them into the output vectors we need the projected
// columns' logical types, which we capture at open by re-enumerating the table's
// columns and selecting the projected indices.
//===----------------------------------------------------------------------===//

/// Per-scan state: the projected columns' logical types (emit order), used to
/// drive `write_duckvalue_to_vector` for each output column.
struct WasmScanState {
    column_types: Vec<Logicaltype>,
}

static STORAGE_SCANS: OnceLock<Mutex<HashMap<u32, WasmScanState>>> = OnceLock::new();

fn storage_scans() -> &'static Mutex<HashMap<u32, WasmScanState>> {
    STORAGE_SCANS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Maps a C-ABI compare-op code (WASM_SCAN_OP_*) to the storage-host CompareOp.
fn storage_scan_op_from_code(
    op: u8,
) -> Option<bindings::duckdb::extension::storage_host::CompareOp> {
    use bindings::duckdb::extension::storage_host::CompareOp as Op;
    Some(match op {
        0 => Op::Eq,
        1 => Op::Ne,
        2 => Op::Lt,
        3 => Op::Le,
        4 => Op::Gt,
        5 => Op::Ge,
        6 => Op::IsNull,
        7 => Op::IsNotNull,
        _ => return None,
    })
}

/// Builds a storage-host Duckvalue from the tagged C-ABI filter fields.
unsafe fn storage_scan_value_from_filter(
    value_type: u8,
    i64v: i64,
    f64v: f64,
    text: *const c_char,
) -> Duckvalue {
    match value_type {
        1 => Duckvalue::Boolean(i64v != 0), // WASM_SCAN_VAL_BOOLEAN
        2 => Duckvalue::Int64(i64v),        // WASM_SCAN_VAL_INT64
        3 => Duckvalue::Float64(f64v),      // WASM_SCAN_VAL_FLOAT64
        4 => {
            // WASM_SCAN_VAL_TEXT
            if text.is_null() {
                Duckvalue::Text(String::new())
            } else {
                Duckvalue::Text(CStr::from_ptr(text).to_string_lossy().into_owned())
            }
        }
        _ => Duckvalue::Null, // WASM_SCAN_VAL_NONE (is-null / is-not-null)
    }
}

/// C-ABI mirror of `WasmScanFilter` in wasm_storage_bridge.h.
#[repr(C)]
pub struct WasmScanFilter {
    column: u32,
    op: u8,
    value_type: u8,
    i64: i64,
    f64: f64,
    text: *const c_char,
}

/// Open a scan cursor honoring `projection` (real table column indices, emit
/// order; nproj==0 => all) and `filters`. Returns a scan handle, 0 on error.
#[no_mangle]
pub unsafe extern "C" fn wasm_storage_scan_open(
    catalog: u32,
    table: *const c_char,
    projection: *const u32,
    nproj: u32,
    filters: *const WasmScanFilter,
    nfilt: u32,
    limit: i64,
) -> u32 {
    use bindings::duckdb::extension::storage_host as sh;

    if table.is_null() {
        storage_set_last_error("wasm_storage_scan_open: null table".to_string());
        return 0;
    }
    let table_str = match CStr::from_ptr(table).to_str() {
        Ok(s) => s.to_owned(),
        Err(_) => {
            storage_set_last_error("wasm_storage_scan_open: table not UTF-8".to_string());
            return 0;
        }
    };

    // Enumerate the full column list so we can (a) record the projected column
    // types for scan-fill and (b) validate indices.
    let all_cols = match sh::storage_table_columns(catalog, &table_str) {
        Ok(cols) => cols,
        Err(err) => {
            storage_set_last_error(storage_format_error(&err));
            return 0;
        }
    };

    let projection_slice: &[u32] = if projection.is_null() || nproj == 0 {
        &[]
    } else {
        slice::from_raw_parts(projection, nproj as usize)
    };

    // Projected column types in emit order (empty projection => natural order).
    let column_types: Vec<Logicaltype> = if projection_slice.is_empty() {
        all_cols.iter().map(|c| c.logical.clone()).collect()
    } else {
        let mut out = Vec::with_capacity(projection_slice.len());
        for &idx in projection_slice {
            match all_cols.get(idx as usize) {
                Some(c) => out.push(c.logical.clone()),
                None => {
                    storage_set_last_error(format!(
                        "wasm_storage_scan_open: projection index {idx} out of range ({} columns)",
                        all_cols.len()
                    ));
                    return 0;
                }
            }
        }
        out
    };

    let filters_slice: &[WasmScanFilter] = if filters.is_null() || nfilt == 0 {
        &[]
    } else {
        slice::from_raw_parts(filters, nfilt as usize)
    };

    let mut scan_filters: Vec<sh::ScanFilter> = Vec::with_capacity(filters_slice.len());
    for f in filters_slice {
        let op = match storage_scan_op_from_code(f.op) {
            Some(op) => op,
            None => continue, // unknown op: skip (best-effort)
        };
        let value = storage_scan_value_from_filter(f.value_type, f.i64, f.f64, f.text);
        scan_filters.push(sh::ScanFilter {
            column: f.column,
            op,
            value,
        });
    }

    let request = sh::ScanRequest {
        table: table_str,
        projection: projection_slice.to_vec(),
        filters: scan_filters,
        limit: if limit < 0 { None } else { Some(limit as u64) },
    };

    clog!(
        "[storage-scan] core scan-open catalog={} projection={:?} nfilt={}",
        catalog,
        request.projection,
        request.filters.len()
    );

    match sh::storage_scan_open(catalog, &request) {
        Ok(scan) => {
            storage_scans()
                .lock()
                .map(|mut m| m.insert(scan, WasmScanState { column_types }))
                .ok();
            scan
        }
        Err(err) => {
            storage_set_last_error(storage_format_error(&err));
            0
        }
    }
}

/// Pull the next batch into `chunk` (a `duckdb_data_chunk` raw handle). Returns
/// true if rows were written, false at EOF (or on error, with last-error set).
#[no_mangle]
pub unsafe extern "C" fn wasm_storage_scan_fill(scan: u32, chunk: *mut c_void) -> bool {
    use bindings::duckdb::extension::storage_host as sh;

    let output = chunk as duckdb::duckdb_data_chunk;
    if output.is_null() {
        storage_set_last_error("wasm_storage_scan_fill: null chunk".to_string());
        return false;
    }

    // Snapshot the projected column types (avoid holding the lock across the
    // host call + vector writes).
    let column_types: Vec<Logicaltype> = match storage_scans().lock() {
        Ok(m) => match m.get(&scan) {
            Some(state) => state.column_types.clone(),
            None => {
                storage_set_last_error(format!("wasm_storage_scan_fill: unknown scan {scan}"));
                return false;
            }
        },
        Err(_) => {
            storage_set_last_error("wasm_storage_scan_fill: scan map poisoned".to_string());
            return false;
        }
    };

    // Pull up to one standard vector worth of rows.
    let rows = match sh::storage_scan_next(scan, 2048) {
        Ok(rows) => rows,
        Err(err) => {
            storage_set_last_error(storage_format_error(&err));
            return false;
        }
    };

    if rows.is_empty() {
        duckdb::duckdb_data_chunk_set_size(output, 0);
        return false;
    }

    duckdb::duckdb_data_chunk_set_size(output, rows.len() as duckdb::idx_t);
    let ncols = column_types.len();
    for col_idx in 0..ncols {
        let vector = duckdb::duckdb_data_chunk_get_vector(output, col_idx as duckdb::idx_t);
        let logical = &column_types[col_idx];
        for (row, row_values) in rows.iter().enumerate() {
            if row_values.len() != ncols {
                storage_set_last_error(format!(
                    "wasm_storage_scan_fill: row {row} has {} cols, expected {ncols}",
                    row_values.len()
                ));
                return false;
            }
            let value = row_values[col_idx].clone();
            if let Err(err) = write_duckvalue_to_vector(vector, logical, row as duckdb::idx_t, value)
            {
                storage_set_last_error(format_duckerror(&err));
                return false;
            }
        }
    }
    true
}

/// Close + free a scan cursor.
#[no_mangle]
pub extern "C" fn wasm_storage_scan_close(scan: u32) {
    use bindings::duckdb::extension::storage_host as sh;
    let _ = sh::storage_scan_close(scan);
    if let Ok(mut m) = storage_scans().lock() {
        m.remove(&scan);
    }
}

//===----------------------------------------------------------------------===//
// httpfs M2 files bridge.
//
// The C++ WasmFileSystem (cpp/wasm_files.cpp) calls these extern-C fns; each
// routes to the host-provided `duckdb:extension/files-host` import, which the
// host forwards to the registered files component's `file-dispatch` export
// (webfs fetches the resource over wasi:sockets, caches it, serves ranges).
// Mirrors the wasm_storage_* bridge. The files-host error channel is plain
// strings (not Duckerror), so we surface them directly.
//===----------------------------------------------------------------------===//

static FILE_LAST_ERROR: OnceLock<Mutex<Option<CString>>> = OnceLock::new();

fn file_set_last_error(msg: String) {
    let cell = FILE_LAST_ERROR.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = cell.lock() {
        *guard = CString::new(msg).ok();
    }
}

/// Returns the most recent files-bridge error message (or an empty C string),
/// owned by the core; the pointer stays valid until the next bridge call.
#[no_mangle]
pub extern "C" fn wasm_file_last_error() -> *const c_char {
    let cell = FILE_LAST_ERROR.get_or_init(|| Mutex::new(None));
    match cell.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(s) => s.as_ptr(),
            None => b"\0".as_ptr() as *const c_char,
        },
        Err(_) => b"\0".as_ptr() as *const c_char,
    }
}

/// Fetch + cache the resource at `url` via the files backend. On success writes
/// the component-side handle + total size and returns true; on error returns
/// false (message in `wasm_file_last_error`).
#[no_mangle]
pub unsafe extern "C" fn wasm_file_open(
    url: *const c_char,
    out_handle: *mut u32,
    out_size: *mut u64,
) -> bool {
    if url.is_null() || out_handle.is_null() || out_size.is_null() {
        file_set_last_error("wasm_file_open: null argument".to_string());
        return false;
    }
    let url_str = match CStr::from_ptr(url).to_str() {
        Ok(s) => s,
        Err(_) => {
            file_set_last_error("wasm_file_open: url is not valid UTF-8".to_string());
            return false;
        }
    };
    match bindings::duckdb::extension::files_host::file_open(url_str) {
        Ok(res) => {
            *out_handle = res.handle;
            *out_size = res.size;
            true
        }
        Err(msg) => {
            file_set_last_error(msg);
            false
        }
    }
}

/// Copy up to `len` bytes from the cached resource at `offset` into `buf`.
/// Returns the count copied (<= len; may be short at EOF), or -1 on error.
#[no_mangle]
pub unsafe extern "C" fn wasm_file_read(
    handle: u32,
    offset: u64,
    len: u32,
    buf: *mut u8,
) -> i64 {
    if buf.is_null() {
        file_set_last_error("wasm_file_read: null buffer".to_string());
        return -1;
    }
    if len == 0 {
        return 0;
    }
    match bindings::duckdb::extension::files_host::file_read(handle, offset, len) {
        Ok(bytes) => {
            let n = std::cmp::min(bytes.len(), len as usize);
            copy_nonoverlapping(bytes.as_ptr(), buf, n);
            n as i64
        }
        Err(msg) => {
            file_set_last_error(msg);
            -1
        }
    }
}

/// Drop the component-side cache entry for `handle` (best-effort).
#[no_mangle]
pub extern "C" fn wasm_file_close(handle: u32) {
    let _ = bindings::duckdb::extension::files_host::file_close(handle);
}

#[derive(Clone, Copy)]
struct ConnectionHandle(duckdb::duckdb_connection, duckdb::duckdb_database);

unsafe impl Send for ConnectionHandle {}
unsafe impl Sync for ConnectionHandle {}

#[derive(Clone, Copy)]
struct DatabaseHandle(duckdb::duckdb_database);

unsafe impl Send for DatabaseHandle {}
unsafe impl Sync for DatabaseHandle {}

#[derive(Clone)]
struct ReplacementScanSpec {
    extensions: Vec<String>,
    function_name: String,
}

#[no_mangle]
pub extern "C" fn _Znwm(size: usize) -> *mut u8 {
    unsafe {
        let layout = Layout::from_size_align(size.max(1), std::mem::align_of::<usize>())
            .unwrap_or_else(|_| std::process::abort());
        let ptr = alloc(layout);
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        ptr
    }
}

#[no_mangle]
pub extern "C" fn _Znam(size: usize) -> *mut u8 {
    _Znwm(size)
}

#[no_mangle]
pub extern "C" fn _ZdlPv(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let layout = Layout::from_size_align(1, std::mem::align_of::<usize>())
            .unwrap_or_else(|_| std::process::abort());
        dealloc(ptr, layout);
    }
}

#[no_mangle]
pub extern "C" fn _ZdaPv(ptr: *mut u8) {
    _ZdlPv(ptr);
}

static GETENV_CACHE: OnceLock<Mutex<HashMap<String, Arc<CString>>>> = OnceLock::new();

fn lookup_env_variable(name: &str) -> Option<String> {
    environment::get_environment()
        .into_iter()
        .find_map(|(key, value)| if key == name { Some(value) } else { None })
}

mod wasi_fs {
    use super::*;
    use bindings::wasi::filesystem::types::{Descriptor, DescriptorFlags, OpenFlags, PathFlags};
    static PREOPEN_DIRS: OnceLock<Vec<(Descriptor, String)>> = OnceLock::new();
    static CURRENT_DIR: OnceLock<Mutex<String>> = OnceLock::new();

    /// Retrieve the list of directories made available to this component.
    /// The WIT bindings return ownership of the descriptor resources, so we
    /// memoise them here to keep the handles alive for the lifetime of the process.
    pub(crate) fn preopened_directories() -> &'static [(Descriptor, String)] {
        PREOPEN_DIRS.get_or_init(|| bindings::wasi::filesystem::preopens::get_directories())
    }

    fn cwd_lock() -> &'static Mutex<String> {
        CURRENT_DIR.get_or_init(|| Mutex::new(".".to_string()))
    }

    pub(crate) fn current_working_directory() -> String {
        cwd_lock()
            .lock()
            .map(|path| path.clone())
            .unwrap_or_else(|_| ".".to_string())
    }

    pub(crate) fn set_current_working_directory(new_path: String) {
        let normalized = if new_path.is_empty() {
            ".".to_string()
        } else {
            new_path
        };
        if let Ok(mut guard) = cwd_lock().lock() {
            *guard = normalized;
        }
    }

    #[derive(Debug)]
    pub(crate) struct OpenRequest {
        pub(crate) descriptor_flags: DescriptorFlags,
        pub(crate) open_flags: OpenFlags,
        pub(crate) append: bool,
        pub(crate) follow_symlinks: bool,
    }

    #[derive(Debug)]
    pub(crate) struct ResolvedPath<'a> {
        pub(crate) descriptor: &'a Descriptor,
        pub(crate) relative_path: String,
        pub(crate) normalized_path: String,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ResolveError {
        /// The requested path escapes the sandbox (e.g. via `..` components).
        EscapeSandbox,
        /// The path does not map to any of the preopened directories.
        NotFound,
    }

    /// Attempt to resolve a POSIX-style path string to a preopened directory
    /// and a relative sub-path.
    pub(crate) fn resolve_path(path: &str) -> Result<ResolvedPath<'static>, ResolveError> {
        let mut attempts = Vec::new();

        let direct = normalize_path(path).ok_or(ResolveError::EscapeSandbox)?;
        clog!("[wasi-fs] resolve start path='{path}' normalized='{direct}'");
        if direct.starts_with(':') {
            return Err(ResolveError::NotFound);
        }
        attempts.push(direct);

        if !path.starts_with('/') && !path.starts_with(':') {
            let cwd = current_working_directory();
            if cwd != "." {
                let candidate = if path == "." {
                    cwd.clone()
                } else {
                    format!("{cwd}/{path}")
                };
                if let Some(normalized) = normalize_path(&candidate) {
                    if !attempts.iter().any(|existing| existing == &normalized) {
                        attempts.insert(0, normalized);
                    }
                }
            }
        }

        let mut best_match: Option<(&Descriptor, String, usize, String)> = None;

        for normalized in attempts {
            for (descriptor, root) in preopened_directories() {
                if let Some((relative, score)) = match_preopen(&normalized, root) {
                    let replace = match best_match
                        .as_ref()
                        .map(|(_, _, current_score, _)| *current_score)
                    {
                        Some(current_score) => score > current_score,
                        None => true,
                    };
                    if replace {
                        best_match = Some((descriptor, relative, score, normalized.clone()));
                    }
                }
            }
        }

        if let Some((descriptor, relative, _, normalized)) = best_match {
            clog!("[wasi-fs] resolved path '{path}' -> preopen relative='{relative}' normalized='{normalized}'");
            Ok(ResolvedPath {
                descriptor,
                relative_path: relative,
                normalized_path: normalized,
            })
        } else {
            Err(ResolveError::NotFound)
        }
    }

    fn match_preopen(normalized_path: &str, root: &str) -> Option<(String, usize)> {
        if root == "." {
            return Some((normalized_path.to_string(), 0));
        }

        let normalized_root = normalize_preopen_root(root)?;
        if normalized_root == "." {
            return Some((normalized_path.to_string(), 0));
        }

        if normalized_path == normalized_root {
            return Some((".".to_string(), normalized_root.len()));
        }

        if let Some(stripped) = normalized_path.strip_prefix(&normalized_root) {
            if stripped.is_empty() {
                return Some((".".to_string(), normalized_root.len()));
            }
            if stripped.starts_with('/') {
                let trimmed = stripped.trim_start_matches('/');
                return Some((
                    if trimmed.is_empty() {
                        ".".to_string()
                    } else {
                        trimmed.to_string()
                    },
                    normalized_root.len(),
                ));
            }
        }

        None
    }

    fn normalize_preopen_root(root: &str) -> Option<String> {
        if root == "." {
            return Some(".".to_string());
        }
        normalize_path(root)
    }

    /// Normalize a filesystem path by removing redundant separators, handling
    /// `.` and `..` segments, and converting Windows-style separators to POSIX.
    fn normalize_path(path: &str) -> Option<String> {
        if path.is_empty() {
            return Some(".".to_string());
        }
        // Treat special in-memory handles separately; callers decide how to handle them.
        if path.starts_with(':') {
            return Some(path.to_string());
        }
        let mut canonical = path.replace('\\', "/");
        let is_absolute = canonical.starts_with('/');
        canonical = canonical.trim_start_matches("./").to_string();
        canonical = canonical.trim_start_matches('/').to_string();

        let mut parts = Vec::new();
        for segment in canonical.split('/') {
            match segment {
                "" | "." => continue,
                ".." => {
                    if parts.pop().is_none() {
                        // Attempted to traverse above the sandbox root.
                        return None;
                    }
                }
                other => parts.push(other),
            }
        }
        if is_absolute && preopened_directories().is_empty() {
            return None;
        }

        let normalized = parts.join("/");
        if canonical.is_empty() {
            Some(".".to_string())
        } else if normalized.is_empty() {
            Some(".".to_string())
        } else {
            Some(normalized)
        }
    }

    pub(crate) fn translate_open_flags(flags: c_int) -> Result<OpenRequest, i32> {
        let mut descriptor_flags = DescriptorFlags::empty();
        let mut open_flags = OpenFlags::empty();
        let follow_symlinks = true;

        let access_mode = flags & libc::O_ACCMODE;
        match access_mode {
            x if x == libc::O_RDONLY => {
                descriptor_flags.insert(DescriptorFlags::READ);
            }
            x if x == libc::O_WRONLY => {
                descriptor_flags.insert(DescriptorFlags::WRITE);
            }
            x if x == libc::O_RDWR => {
                descriptor_flags.insert(DescriptorFlags::READ | DescriptorFlags::WRITE);
            }
            _ => return Err(libc::EINVAL),
        }

        let mut append = false;

        if flags & libc::O_CREAT != 0 {
            open_flags.insert(OpenFlags::CREATE);
            descriptor_flags.insert(DescriptorFlags::WRITE);
        }
        if flags & libc::O_EXCL != 0 {
            open_flags.insert(OpenFlags::EXCLUSIVE);
        }
        if flags & libc::O_TRUNC != 0 {
            open_flags.insert(OpenFlags::TRUNCATE);
            descriptor_flags.insert(DescriptorFlags::WRITE);
        }
        if flags & libc::O_APPEND != 0 {
            append = true;
            descriptor_flags.insert(DescriptorFlags::WRITE);
        }
        if flags & libc::O_DIRECTORY != 0 {
            open_flags.insert(OpenFlags::DIRECTORY);
            descriptor_flags.remove(DescriptorFlags::WRITE);
            append = false;
        }
        if descriptor_flags.is_empty() {
            // Should not happen, but guard against creating handles without capabilities.
            descriptor_flags.insert(DescriptorFlags::READ);
        }

        Ok(OpenRequest {
            descriptor_flags,
            open_flags,
            append,
            follow_symlinks,
        })
    }

    pub(crate) fn path_flags(follow_symlinks: bool) -> PathFlags {
        if follow_symlinks {
            PathFlags::SYMLINK_FOLLOW
        } else {
            PathFlags::empty()
        }
    }
}

#[derive(Debug)]
struct FileEntry {
    descriptor: Descriptor,
    offset: u64,
    append: bool,
    is_directory: bool,
}

#[derive(Default)]
struct FileTable {
    next_fd: i32,
    entries: HashMap<i32, FileEntry>,
}

impl FileTable {
    fn new() -> Self {
        FileTable {
            next_fd: 4,
            entries: HashMap::new(),
        }
    }

    fn allocate_fd(&mut self, entry: FileEntry) -> i32 {
        let fd = self.next_fd;
        self.next_fd += 1;
        self.entries.insert(fd, entry);
        fd
    }

    fn get_mut(&mut self, fd: i32) -> Option<&mut FileEntry> {
        self.entries.get_mut(&fd)
    }

    fn get(&self, fd: i32) -> Option<&FileEntry> {
        self.entries.get(&fd)
    }

    fn remove(&mut self, fd: i32) -> Option<FileEntry> {
        self.entries.remove(&fd)
    }
}

fn file_table() -> &'static Mutex<FileTable> {
    static FILE_TABLE: OnceLock<Mutex<FileTable>> = OnceLock::new();
    FILE_TABLE.get_or_init(|| Mutex::new(FileTable::new()))
}

// On wasm `libc::dirent.d_name` is a zero-length flexible-array member, so the
// struct alone has no room for the name. Back the dirent with an over-sized byte
// buffer (header + name storage) and write the name at the d_name offset.
const DIRENT_NAME_CAP: usize = 256;

#[repr(C)]
struct WasiDir {
    descriptor: Descriptor,
    stream: DirectoryEntryStream,
    entry: [MaybeUninit<u8>; std::mem::size_of::<libc::dirent>() + DIRENT_NAME_CAP],
    finished: bool,
}

impl WasiDir {
    fn new(descriptor: Descriptor) -> Result<Self, ErrorCode> {
        let stream = descriptor.read_directory()?;
        Ok(Self {
            descriptor,
            stream,
            entry: [MaybeUninit::uninit(); std::mem::size_of::<libc::dirent>() + DIRENT_NAME_CAP],
            finished: false,
        })
    }

    fn dirent_ptr(&mut self) -> *mut libc::dirent {
        self.entry.as_mut_ptr().cast::<libc::dirent>()
    }
}

// postgres_scanner: Rust trampolines for the getaddrinfo wrapper. --wrap=getaddrinfo
// (build.rs, when cfg pg_net is set) redirects libpq's getaddrinfo/freeaddrinfo
// here; they must live in the root crate (not an on-demand archive) for --wrap to
// bind them, mirroring the fs-shims __wrap_open. They forward to the C impl
// (cmake/postgres-wasi/getaddrinfo_wrap.c), which parses numeric IPs locally
// (wasi's getaddrinfo rejects them) and delegates hostnames to __real_getaddrinfo.
#[cfg(pg_net)]
mod pg_net_overrides {
    use core::ffi::{c_char, c_int, c_void};
    extern "C" {
        fn pg_wasi_getaddrinfo(
            node: *const c_char,
            service: *const c_char,
            hints: *const c_void,
            res: *mut *mut c_void,
        ) -> c_int;
        fn pg_wasi_freeaddrinfo(ai: *mut c_void);
    }
    #[no_mangle]
    pub unsafe extern "C" fn __wrap_getaddrinfo(
        node: *const c_char,
        service: *const c_char,
        hints: *const c_void,
        res: *mut *mut c_void,
    ) -> c_int {
        pg_wasi_getaddrinfo(node, service, hints, res)
    }
    #[no_mangle]
    pub unsafe extern "C" fn __wrap_freeaddrinfo(ai: *mut c_void) {
        pg_wasi_freeaddrinfo(ai)
    }
}

#[cfg(feature = "fs_shims")]
mod libc_overrides {
    use super::*;
    use std::slice;

    // --wrap renames the real libc fns to __real_*. For fds NOT in our file
    // table (sockets -- e.g. openssl's socket BIO uses read()/write()), fall
    // through to these so socket I/O reaches the wasip2 socket layer instead of
    // failing with EBADF. (libpq used recv/send and bypassed the wrap entirely.)
    extern "C" {
        fn __real_read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
        fn __real_write(fd: c_int, buf: *const c_void, count: usize) -> isize;
        fn __real_close(fd: c_int) -> c_int;
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_open(path: *const c_char, flags: c_int, _mode: c_int) -> c_int {
        clear_errno();

        if path.is_null() {
            set_errno(libc::EINVAL);
            return -1;
        }

        let c_path = CStr::from_ptr(path);
        let path_str = match c_path.to_str() {
            Ok(p) => p,
            Err(_) => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };

        clog!("[wasi-fs] open path='{path_str}' flags=0x{flags:x}");

        clog!("[wasi-fs] open path={path_str} flags={flags:#x}");

        let open_request = match wasi_fs::translate_open_flags(flags) {
            Ok(req) => req,
            Err(errno) => {
                set_errno(errno);
                return -1;
            }
        };

        let resolved = match wasi_fs::resolve_path(path_str) {
            Ok(result) => result,
            Err(wasi_fs::ResolveError::EscapeSandbox) => {
                set_errno(libc::EPERM);
                return -1;
            }
            Err(wasi_fs::ResolveError::NotFound) => {
                set_errno(libc::ENOENT);
                return -1;
            }
        };

        let directory = resolved.descriptor;
        let relative_path = resolved.relative_path;
        let relative = if relative_path.is_empty() || relative_path == "." {
            "."
        } else {
            relative_path.as_str()
        };

        match directory.open_at(
            wasi_fs::path_flags(open_request.follow_symlinks),
            relative,
            open_request.open_flags,
            open_request.descriptor_flags,
        ) {
            Ok(descriptor) => {
                let metadata = match descriptor.stat() {
                    Ok(stat) => stat,
                    Err(code) => {
                        set_errno(map_error_code(code));
                        return -1;
                    }
                };

                let entry = FileEntry {
                    descriptor,
                    offset: 0,
                    append: open_request.append,
                    is_directory: metadata.type_ == DescriptorType::Directory,
                };

                let fd = {
                    let mut table = file_table().lock().expect("file table mutex poisoned");
                    table.allocate_fd(entry)
                };

                fd
            }
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_close(fd: c_int) -> c_int {
        clear_errno();

        if fd < 0 {
            set_errno(libc::EBADF);
            return -1;
        }

        let mut table = file_table().lock().expect("file table mutex poisoned");
        if table.remove(fd as i32).is_some() {
            0
        } else {
            drop(table);
            __real_close(fd)
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_read(fd: c_int, buf: *mut c_void, count: usize) -> isize {
        clear_errno();

        let mut table = file_table().lock().expect("file table mutex poisoned");
        let entry = match table.get_mut(fd as i32) {
            Some(entry) => entry,
            None => {
                // not a managed file fd (e.g. a socket) -> real read
                return __real_read(fd, buf, count);
            }
        };

        if count == 0 {
            return 0;
        }
        if buf.is_null() {
            set_errno(libc::EFAULT);
            return -1;
        }
        if entry.is_directory {
            set_errno(libc::EISDIR);
            return -1;
        }

        match entry.descriptor.read(count as u64, entry.offset) {
            Ok((data, _eof)) => {
                let bytes_read = std::cmp::min(data.len(), count);
                if bytes_read > 0 {
                    copy_nonoverlapping(data.as_ptr(), buf as *mut u8, bytes_read);
                    entry.offset = entry.offset.saturating_add(bytes_read as u64);
                }
                bytes_read as isize
            }
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_pread(
        fd: c_int,
        buf: *mut c_void,
        count: usize,
        offset: libc::off_t,
    ) -> isize {
        clear_errno();

        if offset < 0 {
            set_errno(libc::EINVAL);
            return -1;
        }

        let table = file_table().lock().expect("file table mutex poisoned");
        let entry = match table.get(fd as i32) {
            Some(entry) => entry,
            None => {
                set_errno(libc::EBADF);
                return -1;
            }
        };

        if count == 0 {
            return 0;
        }
        if buf.is_null() {
            set_errno(libc::EFAULT);
            return -1;
        }
        if entry.is_directory {
            set_errno(libc::EISDIR);
            return -1;
        }

        match entry.descriptor.read(count as u64, offset as u64) {
            Ok((data, _eof)) => {
                let bytes_read = std::cmp::min(data.len(), count);
                if bytes_read > 0 {
                    copy_nonoverlapping(data.as_ptr(), buf as *mut u8, bytes_read);
                }
                bytes_read as isize
            }
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_write(fd: c_int, buf: *const c_void, count: usize) -> isize {
        clear_errno();

        let mut table = file_table().lock().expect("file table mutex poisoned");
        let entry = match table.get_mut(fd as i32) {
            Some(entry) => entry,
            None => {
                // not a managed file fd (e.g. a socket) -> real write
                return __real_write(fd, buf, count);
            }
        };

        if count == 0 {
            return 0;
        }
        if buf.is_null() {
            set_errno(libc::EFAULT);
            return -1;
        }
        if entry.is_directory {
            set_errno(libc::EISDIR);
            return -1;
        }

        let data = slice::from_raw_parts(buf as *const u8, count);

        let start_offset = if entry.append {
            match entry.descriptor.stat() {
                Ok(stat) => stat.size,
                Err(code) => {
                    set_errno(map_error_code(code));
                    return -1;
                }
            }
        } else {
            entry.offset
        };

        match entry.descriptor.write(data, start_offset) {
            Ok(written) => {
                let bytes_written = std::cmp::min(written, count as u64);
                let new_offset = start_offset.saturating_add(bytes_written);
                entry.offset = new_offset;
                bytes_written as isize
            }
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_pwrite(
        fd: c_int,
        buf: *const c_void,
        count: usize,
        offset: libc::off_t,
    ) -> isize {
        clear_errno();

        if offset < 0 {
            set_errno(libc::EINVAL);
            return -1;
        }

        let table = file_table().lock().expect("file table mutex poisoned");
        let entry = match table.get(fd as i32) {
            Some(entry) => entry,
            None => {
                set_errno(libc::EBADF);
                return -1;
            }
        };

        if count == 0 {
            return 0;
        }
        if buf.is_null() {
            set_errno(libc::EFAULT);
            return -1;
        }
        if entry.is_directory {
            set_errno(libc::EISDIR);
            return -1;
        }

        let data = slice::from_raw_parts(buf as *const u8, count);

        match entry.descriptor.write(data, offset as u64) {
            Ok(written) => {
                let bytes_written = std::cmp::min(written, count as u64);
                bytes_written as isize
            }
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_lseek(
        fd: c_int,
        offset: libc::off_t,
        whence: c_int,
    ) -> libc::off_t {
        clear_errno();

        let mut table = file_table().lock().expect("file table mutex poisoned");
        let entry = match table.get_mut(fd as i32) {
            Some(entry) => entry,
            None => {
                set_errno(libc::EBADF);
                return -1;
            }
        };

        let base: i128 = match whence {
            x if x == libc::SEEK_SET => 0,
            x if x == libc::SEEK_CUR => entry.offset as i128,
            x if x == libc::SEEK_END => match entry.descriptor.stat() {
                Ok(stat) => stat.size as i128,
                Err(code) => {
                    set_errno(map_error_code(code));
                    return -1;
                }
            },
            _ => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };

        let target = match base.checked_add(offset as i128) {
            Some(value) => value,
            None => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };
        if target < 0 {
            set_errno(libc::EINVAL);
            return -1;
        }
        if target > i128::from(i64::MAX) {
            set_errno(libc::EINVAL);
            return -1;
        }

        let target_u64 = target as u64;
        entry.offset = target_u64;
        target as libc::off_t
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_fsync(fd: c_int) -> c_int {
        clear_errno();

        let table = file_table().lock().expect("file table mutex poisoned");
        let entry = match table.get(fd as i32) {
            Some(entry) => entry,
            None => {
                set_errno(libc::EBADF);
                return -1;
            }
        };

        match entry.descriptor.sync() {
            Ok(()) => 0,
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_fdatasync(fd: c_int) -> c_int {
        clear_errno();

        let table = file_table().lock().expect("file table mutex poisoned");
        let entry = match table.get(fd as i32) {
            Some(entry) => entry,
            None => {
                set_errno(libc::EBADF);
                return -1;
            }
        };

        match entry.descriptor.sync_data() {
            Ok(()) => 0,
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_ftruncate(fd: c_int, length: libc::off_t) -> c_int {
        clear_errno();

        if length < 0 {
            set_errno(libc::EINVAL);
            return -1;
        }

        let mut table = file_table().lock().expect("file table mutex poisoned");
        let entry = match table.get_mut(fd as i32) {
            Some(entry) => entry,
            None => {
                set_errno(libc::EBADF);
                return -1;
            }
        };

        if entry.is_directory {
            set_errno(libc::EINVAL);
            return -1;
        }

        let new_size = length as u64;
        match entry.descriptor.set_size(new_size) {
            Ok(()) => {
                if entry.offset > new_size {
                    entry.offset = new_size;
                }
                0
            }
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    fn map_error_code(code: ErrorCode) -> i32 {
        match code {
            ErrorCode::Access => libc::EACCES,
            ErrorCode::WouldBlock => libc::EAGAIN,
            ErrorCode::Already => libc::EALREADY,
            ErrorCode::BadDescriptor => libc::EBADF,
            ErrorCode::Busy => libc::EBUSY,
            ErrorCode::Deadlock => libc::EDEADLK,
            ErrorCode::Quota => libc::EDQUOT,
            ErrorCode::Exist => libc::EEXIST,
            ErrorCode::FileTooLarge => libc::EFBIG,
            ErrorCode::IllegalByteSequence => libc::EILSEQ,
            ErrorCode::InProgress => libc::EINPROGRESS,
            ErrorCode::Interrupted => libc::EINTR,
            ErrorCode::Invalid => libc::EINVAL,
            ErrorCode::Io => libc::EIO,
            ErrorCode::IsDirectory => libc::EISDIR,
            ErrorCode::Loop => libc::ELOOP,
            ErrorCode::TooManyLinks => libc::EMLINK,
            ErrorCode::MessageSize => libc::EMSGSIZE,
            ErrorCode::NameTooLong => libc::ENAMETOOLONG,
            ErrorCode::NoDevice => libc::ENODEV,
            ErrorCode::NoEntry => libc::ENOENT,
            ErrorCode::NoLock => libc::ENOLCK,
            ErrorCode::InsufficientMemory => libc::ENOMEM,
            ErrorCode::InsufficientSpace => libc::ENOSPC,
            ErrorCode::NotDirectory => libc::ENOTDIR,
            ErrorCode::NotEmpty => libc::ENOTEMPTY,
            ErrorCode::NotRecoverable => libc::ENOTRECOVERABLE,
            ErrorCode::Unsupported => libc::ENOTSUP,
            ErrorCode::NoTty => libc::ENOTTY,
            ErrorCode::NoSuchDevice => libc::ENXIO,
            ErrorCode::Overflow => libc::EOVERFLOW,
            ErrorCode::NotPermitted => libc::EPERM,
            ErrorCode::Pipe => libc::EPIPE,
            ErrorCode::ReadOnly => libc::EROFS,
            ErrorCode::InvalidSeek => libc::ESPIPE,
            ErrorCode::TextFileBusy => libc::ETXTBSY,
            ErrorCode::CrossDevice => libc::EXDEV,
        }
    }

    fn descriptor_type_to_mode(ty: DescriptorType) -> libc::mode_t {
        match ty {
            DescriptorType::BlockDevice => libc::S_IFBLK,
            DescriptorType::CharacterDevice => libc::S_IFCHR,
            DescriptorType::Directory => libc::S_IFDIR,
            DescriptorType::Fifo => libc::S_IFIFO,
            DescriptorType::SymbolicLink => libc::S_IFLNK,
            DescriptorType::RegularFile => libc::S_IFREG,
            DescriptorType::Socket => libc::S_IFSOCK,
            DescriptorType::Unknown => 0,
        }
    }

    fn default_permissions(ty: DescriptorType) -> libc::mode_t {
        match ty {
            DescriptorType::Directory => 0o755,
            DescriptorType::SymbolicLink => 0o777,
            DescriptorType::RegularFile => 0o644,
            _ => 0o644,
        }
    }

    fn datetime_to_timespec(dt: Option<Datetime>) -> libc::timespec {
        match dt {
            Some(datetime) => libc::timespec {
                tv_sec: {
                    if datetime.seconds > i64::MAX as u64 {
                        i64::MAX as libc::time_t
                    } else {
                        datetime.seconds as libc::time_t
                    }
                },
                tv_nsec: datetime.nanoseconds as libc::c_long,
            },
            None => libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        }
    }

    fn clamp_u64_to_off_t(value: u64) -> libc::off_t {
        if value > i64::MAX as u64 {
            i64::MAX as libc::off_t
        } else {
            value as libc::off_t
        }
    }

    fn clamp_u64_to_nlink(value: u64) -> libc::nlink_t {
        if std::mem::size_of::<libc::nlink_t>() >= 8 {
            value as libc::nlink_t
        } else {
            value.min(u32::MAX as u64) as libc::nlink_t
        }
    }

    fn descriptor_stat_to_libc(stat: &DescriptorStat) -> libc::stat {
        let mut out: libc::stat = unsafe { std::mem::zeroed() };
        out.st_mode = descriptor_type_to_mode(stat.type_) | default_permissions(stat.type_);
        out.st_nlink = clamp_u64_to_nlink(stat.link_count);
        out.st_size = clamp_u64_to_off_t(stat.size);
        set_stat_timestamps(&mut out, stat);
        out
    }

    fn set_stat_timestamps(out: &mut libc::stat, stat: &DescriptorStat) {
        let access = datetime_to_timespec(stat.data_access_timestamp);
        let modification = datetime_to_timespec(stat.data_modification_timestamp);
        let status_change = datetime_to_timespec(stat.status_change_timestamp);

        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
            target_os = "solaris",
            target_os = "emscripten",
            target_os = "wasi"
        ))]
        {
            out.st_atim = access;
            out.st_mtim = modification;
            out.st_ctim = status_change;
        }

        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            out.st_atime = access.tv_sec as libc::time_t;
            out.st_mtime = modification.tv_sec as libc::time_t;
            out.st_ctime = status_change.tv_sec as libc::time_t;
            #[cfg(target_os = "macos")]
            {
                out.st_atime_nsec = access.tv_nsec as libc::c_long;
                out.st_mtime_nsec = modification.tv_nsec as libc::c_long;
                out.st_ctime_nsec = status_change.tv_nsec as libc::c_long;
            }
            #[cfg(target_os = "ios")]
            {
                out.st_atime_nsec = access.tv_nsec as libc::c_long;
                out.st_mtime_nsec = modification.tv_nsec as libc::c_long;
                out.st_ctime_nsec = status_change.tv_nsec as libc::c_long;
            }
        }

        #[cfg(target_os = "windows")]
        {
            out.st_atime = access.tv_sec as libc::time_t;
            out.st_mtime = modification.tv_sec as libc::time_t;
            out.st_ctime = status_change.tv_sec as libc::time_t;
        }
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "solaris",
        target_os = "emscripten"
    ))]
    fn descriptor_type_to_dirent_type(ty: DescriptorType) -> u8 {
        match ty {
            DescriptorType::BlockDevice => libc::DT_BLK as u8,
            DescriptorType::CharacterDevice => libc::DT_CHR as u8,
            DescriptorType::Directory => libc::DT_DIR as u8,
            DescriptorType::Fifo => libc::DT_FIFO as u8,
            DescriptorType::SymbolicLink => libc::DT_LNK as u8,
            DescriptorType::RegularFile => libc::DT_REG as u8,
            DescriptorType::Socket => libc::DT_SOCK as u8,
            DescriptorType::Unknown => libc::DT_UNKNOWN as u8,
        }
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "solaris",
        target_os = "emscripten",
        target_os = "wasi"
    )))]
    fn descriptor_type_to_dirent_type(_: DescriptorType) -> u8 {
        0
    }

    #[cfg(all(target_family = "wasm", not(target_os = "emscripten")))]
    fn descriptor_type_to_dirent_type(ty: DescriptorType) -> u8 {
        match ty {
            DescriptorType::Unknown => 0,
            DescriptorType::BlockDevice => 6,
            DescriptorType::CharacterDevice => 2,
            DescriptorType::Directory => 4,
            DescriptorType::Fifo => 1,
            DescriptorType::SymbolicLink => 10,
            DescriptorType::RegularFile => 8,
            DescriptorType::Socket => 12,
        }
    }

    unsafe fn stat_like(path: *const c_char, buf: *mut libc::stat, follow_symlinks: bool) -> c_int {
        clear_errno();

        if path.is_null() {
            set_errno(libc::EINVAL);
            return -1;
        }
        if buf.is_null() {
            set_errno(libc::EFAULT);
            return -1;
        }

        let path_str = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };

        let resolved = match wasi_fs::resolve_path(path_str) {
            Ok(tuple) => tuple,
            Err(wasi_fs::ResolveError::EscapeSandbox) => {
                set_errno(libc::EPERM);
                return -1;
            }
            Err(wasi_fs::ResolveError::NotFound) => {
                set_errno(libc::ENOENT);
                return -1;
            }
        };

        let relative = if resolved.relative_path == "." {
            "."
        } else {
            resolved.relative_path.as_str()
        };

        match resolved
            .descriptor
            .stat_at(wasi_fs::path_flags(follow_symlinks), relative)
        {
            Ok(info) => {
                *buf = descriptor_stat_to_libc(&info);
                0
            }
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_stat(path: *const c_char, buf: *mut libc::stat) -> c_int {
        stat_like(path, buf, true)
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_lstat(path: *const c_char, buf: *mut libc::stat) -> c_int {
        stat_like(path, buf, false)
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_fstat(fd: c_int, buf: *mut libc::stat) -> c_int {
        clear_errno();

        if buf.is_null() {
            set_errno(libc::EFAULT);
            return -1;
        }

        let table = file_table().lock().expect("file table mutex poisoned");
        let entry = match table.get(fd as i32) {
            Some(entry) => entry,
            None => {
                set_errno(libc::EBADF);
                return -1;
            }
        };

        match entry.descriptor.stat() {
            Ok(info) => {
                *buf = descriptor_stat_to_libc(&info);
                0
            }
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    fn resolve_relative_path(
        path: &str,
    ) -> Result<wasi_fs::ResolvedPath<'static>, wasi_fs::ResolveError> {
        wasi_fs::resolve_path(path)
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_mkdir(path: *const c_char, _mode: libc::mode_t) -> c_int {
        clear_errno();

        if path.is_null() {
            set_errno(libc::EINVAL);
            return -1;
        }

        let path_str = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };

        let resolved = match resolve_relative_path(path_str) {
            Ok(result) => result,
            Err(wasi_fs::ResolveError::EscapeSandbox) => {
                set_errno(libc::EPERM);
                return -1;
            }
            Err(wasi_fs::ResolveError::NotFound) => {
                set_errno(libc::ENOENT);
                return -1;
            }
        };

        if resolved.relative_path == "." {
            set_errno(libc::EEXIST);
            return -1;
        }

        match resolved
            .descriptor
            .create_directory_at(resolved.relative_path.as_str())
        {
            Ok(()) => 0,
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_rmdir(path: *const c_char) -> c_int {
        clear_errno();

        if path.is_null() {
            set_errno(libc::EINVAL);
            return -1;
        }

        let path_str = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };

        let resolved = match resolve_relative_path(path_str) {
            Ok(result) => result,
            Err(wasi_fs::ResolveError::EscapeSandbox) => {
                set_errno(libc::EPERM);
                return -1;
            }
            Err(wasi_fs::ResolveError::NotFound) => {
                set_errno(libc::ENOENT);
                return -1;
            }
        };

        if resolved.relative_path == "." {
            set_errno(libc::EINVAL);
            return -1;
        }

        match resolved
            .descriptor
            .remove_directory_at(resolved.relative_path.as_str())
        {
            Ok(()) => 0,
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_unlink(path: *const c_char) -> c_int {
        clear_errno();

        if path.is_null() {
            set_errno(libc::EINVAL);
            return -1;
        }

        let path_str = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };

        let resolved = match resolve_relative_path(path_str) {
            Ok(result) => result,
            Err(wasi_fs::ResolveError::EscapeSandbox) => {
                set_errno(libc::EPERM);
                return -1;
            }
            Err(wasi_fs::ResolveError::NotFound) => {
                set_errno(libc::ENOENT);
                return -1;
            }
        };

        match resolved
            .descriptor
            .unlink_file_at(resolved.relative_path.as_str())
        {
            Ok(()) => 0,
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_remove(path: *const c_char) -> c_int {
        clear_errno();

        if path.is_null() {
            set_errno(libc::EINVAL);
            return -1;
        }

        let path_str = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };

        let resolved = match resolve_relative_path(path_str) {
            Ok(result) => result,
            Err(wasi_fs::ResolveError::EscapeSandbox) => {
                set_errno(libc::EPERM);
                return -1;
            }
            Err(wasi_fs::ResolveError::NotFound) => {
                set_errno(libc::ENOENT);
                return -1;
            }
        };

        match resolved
            .descriptor
            .unlink_file_at(resolved.relative_path.as_str())
        {
            Ok(()) => 0,
            Err(ErrorCode::IsDirectory) => match resolved
                .descriptor
                .remove_directory_at(resolved.relative_path.as_str())
            {
                Ok(()) => 0,
                Err(code) => {
                    set_errno(map_error_code(code));
                    -1
                }
            },
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_rename(
        old_path: *const c_char,
        new_path: *const c_char,
    ) -> c_int {
        clear_errno();

        if old_path.is_null() || new_path.is_null() {
            set_errno(libc::EINVAL);
            return -1;
        }

        let old_str = match CStr::from_ptr(old_path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };
        let new_str = match CStr::from_ptr(new_path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };

        let old_resolved = match resolve_relative_path(old_str) {
            Ok(result) => result,
            Err(wasi_fs::ResolveError::EscapeSandbox) => {
                set_errno(libc::EPERM);
                return -1;
            }
            Err(wasi_fs::ResolveError::NotFound) => {
                set_errno(libc::ENOENT);
                return -1;
            }
        };

        let new_resolved = match resolve_relative_path(new_str) {
            Ok(result) => result,
            Err(wasi_fs::ResolveError::EscapeSandbox) => {
                set_errno(libc::EPERM);
                return -1;
            }
            Err(wasi_fs::ResolveError::NotFound) => {
                // Renaming to a path in an unmapped preopen.
                set_errno(libc::ENOENT);
                return -1;
            }
        };

        match old_resolved.descriptor.rename_at(
            old_resolved.relative_path.as_str(),
            new_resolved.descriptor,
            new_resolved.relative_path.as_str(),
        ) {
            Ok(()) => 0,
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_access(path: *const c_char, mode: c_int) -> c_int {
        clear_errno();

        if path.is_null() {
            set_errno(libc::EINVAL);
            return -1;
        }

        let invalid_bits = mode & !(libc::R_OK | libc::W_OK | libc::X_OK | libc::F_OK);
        if invalid_bits != 0 {
            set_errno(libc::EINVAL);
            return -1;
        }

        let path_str = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };

        let resolved = match resolve_relative_path(path_str) {
            Ok(result) => result,
            Err(wasi_fs::ResolveError::EscapeSandbox) => {
                set_errno(libc::EPERM);
                return -1;
            }
            Err(wasi_fs::ResolveError::NotFound) => {
                set_errno(libc::ENOENT);
                return -1;
            }
        };

        let relative = if resolved.relative_path == "." {
            "."
        } else {
            resolved.relative_path.as_str()
        };

        match resolved
            .descriptor
            .stat_at(wasi_fs::path_flags(true), relative)
        {
            Ok(_) => 0,
            Err(code) => {
                set_errno(map_error_code(code));
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_isatty(fd: c_int) -> c_int {
        clear_errno();

        let table = file_table().lock().expect("file table mutex poisoned");
        let entry = match table.get(fd as i32) {
            Some(entry) => entry,
            None => {
                set_errno(libc::EBADF);
                return 0;
            }
        };

        match entry.descriptor.stat() {
            Ok(info) => {
                if info.type_ == DescriptorType::CharacterDevice {
                    return 1;
                }
                set_errno(libc::ENOTTY);
                0
            }
            Err(code) => {
                set_errno(map_error_code(code));
                0
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_chdir(path: *const c_char) -> c_int {
        clear_errno();

        if path.is_null() {
            set_errno(libc::EFAULT);
            return -1;
        }

        let path_str = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };

        let resolved = match wasi_fs::resolve_path(path_str) {
            Ok(result) => result,
            Err(wasi_fs::ResolveError::EscapeSandbox) => {
                set_errno(libc::EPERM);
                return -1;
            }
            Err(wasi_fs::ResolveError::NotFound) => {
                set_errno(libc::ENOENT);
                return -1;
            }
        };

        let relative = if resolved.relative_path == "." {
            "."
        } else {
            resolved.relative_path.as_str()
        };

        match resolved
            .descriptor
            .stat_at(wasi_fs::path_flags(true), relative)
        {
            Ok(info) => {
                if info.type_ != DescriptorType::Directory {
                    set_errno(libc::ENOTDIR);
                    return -1;
                }
            }
            Err(code) => {
                set_errno(map_error_code(code));
                return -1;
            }
        }

        wasi_fs::set_current_working_directory(resolved.normalized_path.clone());
        0
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_getcwd(buf: *mut c_char, size: usize) -> *mut c_char {
        clear_errno();

        if buf.is_null() {
            set_errno(libc::EFAULT);
            return ptr::null_mut();
        }
        if size == 0 {
            set_errno(libc::ERANGE);
            return ptr::null_mut();
        }

        let cwd = wasi_fs::current_working_directory();
        let display = if cwd.is_empty() { ".".to_string() } else { cwd };
        let bytes = display.as_bytes();

        if bytes.len() + 1 > size {
            set_errno(libc::ERANGE);
            return ptr::null_mut();
        }

        copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
        *buf.add(bytes.len()) = 0;
        buf
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_readlink(
        path: *const c_char,
        buf: *mut c_char,
        bufsiz: usize,
    ) -> isize {
        clear_errno();

        if path.is_null() || buf.is_null() {
            set_errno(libc::EFAULT);
            return -1;
        }
        if bufsiz == 0 {
            set_errno(libc::EINVAL);
            return -1;
        }

        let path_str = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_errno(libc::EINVAL);
                return -1;
            }
        };

        let resolved = match wasi_fs::resolve_path(path_str) {
            Ok(result) => result,
            Err(wasi_fs::ResolveError::EscapeSandbox) => {
                set_errno(libc::EPERM);
                return -1;
            }
            Err(wasi_fs::ResolveError::NotFound) => {
                set_errno(libc::ENOENT);
                return -1;
            }
        };

        let relative = if resolved.relative_path == "." {
            "."
        } else {
            resolved.relative_path.as_str()
        };

        let link_target = match resolved.descriptor.readlink_at(relative) {
            Ok(target) => target,
            Err(code) => {
                set_errno(map_error_code(code));
                return -1;
            }
        };

        let link_bytes = link_target.as_bytes();
        let copy_len = std::cmp::min(link_bytes.len(), bufsiz);
        copy_nonoverlapping(link_bytes.as_ptr(), buf as *mut u8, copy_len);
        copy_len as isize
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap__ZN6duckdb16DatabaseInstance21LoadExtensionSettingsEv(
        _this: *mut c_void,
    ) {
        // Skip loading statically linked extensions in the wasm build.
    }

    #[no_mangle]
    pub unsafe extern "C" fn _ZNSt3__220__shared_ptr_emplaceIN6duckdb8HTTPUtilENS_9allocatorIS2_EEE16__on_zero_sharedEv(
        _this: *mut c_void,
    ) {
    }

    #[no_mangle]
    pub unsafe extern "C" fn _ZNSt3__220__shared_ptr_emplaceIN6duckdb8HTTPUtilENS_9allocatorIS2_EEE21__on_zero_shared_weakEv(
        _this: *mut c_void,
    ) {
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_opendir(path: *const c_char) -> *mut libc::DIR {
        clear_errno();

        if path.is_null() {
            set_errno(libc::EINVAL);
            return ptr::null_mut();
        }

        let path_str = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_errno(libc::EINVAL);
                return ptr::null_mut();
            }
        };

        let resolved = match resolve_relative_path(path_str) {
            Ok(result) => result,
            Err(wasi_fs::ResolveError::EscapeSandbox) => {
                set_errno(libc::EPERM);
                return ptr::null_mut();
            }
            Err(wasi_fs::ResolveError::NotFound) => {
                set_errno(libc::ENOENT);
                return ptr::null_mut();
            }
        };

        let relative = if resolved.relative_path == "." {
            "."
        } else {
            resolved.relative_path.as_str()
        };

        let descriptor = match resolved.descriptor.open_at(
            wasi_fs::path_flags(true),
            relative,
            OpenFlags::DIRECTORY,
            DescriptorFlags::READ,
        ) {
            Ok(desc) => desc,
            Err(code) => {
                set_errno(map_error_code(code));
                return ptr::null_mut();
            }
        };

        let handle = match WasiDir::new(descriptor) {
            Ok(dir) => dir,
            Err(code) => {
                set_errno(map_error_code(code));
                return ptr::null_mut();
            }
        };

        Box::into_raw(Box::new(handle)) as *mut libc::DIR
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_readdir(dirp: *mut libc::DIR) -> *mut libc::dirent {
        clear_errno();

        if dirp.is_null() {
            set_errno(libc::EBADF);
            return ptr::null_mut();
        }

        let handle = &mut *(dirp as *mut WasiDir);
        if handle.finished {
            return ptr::null_mut();
        }

        loop {
            match handle.stream.read_directory_entry() {
                Ok(Some(entry)) => {
                    let name = entry.name.as_str();
                    if name == "." || name == ".." {
                        continue;
                    }

                    let dirent_ptr = handle.dirent_ptr();
                    let name_bytes = entry.name.as_bytes();

                    // d_name is a flexible array member (len 0 in the struct); the
                    // real storage is the over-sized WasiDir.entry buffer. Write the
                    // name at d_name's offset, bounded by DIRENT_NAME_CAP (+1 for NUL).
                    let name_off = std::mem::offset_of!(libc::dirent, d_name);
                    let capacity = DIRENT_NAME_CAP;
                    if name_bytes.len() + 1 > capacity {
                        set_errno(libc::ENAMETOOLONG);
                        return ptr::null_mut();
                    }

                    unsafe {
                        ptr::write_bytes(
                            dirent_ptr.cast::<u8>(),
                            0,
                            std::mem::size_of::<libc::dirent>() + capacity,
                        );
                        (*dirent_ptr).d_ino = 0;
                        (*dirent_ptr).d_type = descriptor_type_to_dirent_type(entry.type_);
                        let name_dst = dirent_ptr.cast::<u8>().add(name_off);
                        ptr::copy_nonoverlapping(name_bytes.as_ptr(), name_dst, name_bytes.len());
                        *name_dst.add(name_bytes.len()) = 0;

                        #[cfg(any(
                            target_os = "macos",
                            target_os = "ios",
                            target_os = "freebsd",
                            target_os = "netbsd",
                            target_os = "openbsd",
                            target_os = "dragonfly"
                        ))]
                        {
                            (*dirent_ptr).d_namlen = name_bytes.len() as _;
                            (*dirent_ptr).d_reclen = (name_off + name_bytes.len() + 1) as _;
                        }
                    }

                    return dirent_ptr;
                }
                Ok(None) => {
                    handle.finished = true;
                    return ptr::null_mut();
                }
                Err(code) => {
                    set_errno(map_error_code(code));
                    handle.finished = true;
                    return ptr::null_mut();
                }
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn __wrap_closedir(dirp: *mut libc::DIR) -> c_int {
        clear_errno();

        if dirp.is_null() {
            set_errno(libc::EBADF);
            return -1;
        }

        drop(Box::from_raw(dirp as *mut WasiDir));
        0
    }
}

// wasi-libc declares `errno` as a regular (single-threaded) global symbol and
// accesses it *directly* — `#include <__errno.h>` does `extern _Thread_local
// int errno; #define errno errno`, NOT `*__errno_location()`. DuckDB's C++ reads
// that same `errno` global. So the filesystem shims must set the real libc
// `errno`; an internal copy would be invisible to DuckDB and every "file does
// not exist yet" probe (open without O_CREAT) would read errno==0 ("Success")
// instead of ENOENT, so DuckDB would never take its create-on-open path and
// on-disk databases could not be created. The symbol is plain DATA (not TLS in
// the threadless wasip2 build), so a normal extern static resolves to it.
extern "C" {
    #[link_name = "errno"]
    static mut LIBC_ERRNO: c_int;
}

unsafe fn set_errno(code: i32) {
    LIBC_ERRNO = code;
}

unsafe fn clear_errno() {
    LIBC_ERRNO = 0;
}

#[no_mangle]
pub unsafe extern "C" fn __errno_location() -> *mut c_int {
    ptr::addr_of_mut!(LIBC_ERRNO)
}

#[no_mangle]
pub unsafe extern "C" fn getenv(name: *const c_char) -> *mut c_char {
    if name.is_null() {
        return std::ptr::null_mut();
    }

    let key = match CStr::from_ptr(name).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return std::ptr::null_mut(),
    };

    let value = match lookup_env_variable(&key) {
        Some(v) => v,
        None => return std::ptr::null_mut(),
    };

    let cache = GETENV_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache_ref = cache.lock().expect("getenv cache poisoned");
    if let Some(cached) = cache_ref.get(&key) {
        return cached.as_ptr() as *mut c_char;
    }

    let c_string = match CString::new(value) {
        Ok(cs) => cs,
        Err(_) => return std::ptr::null_mut(),
    };

    let arc = Arc::new(c_string);
    let ptr = arc.as_ptr() as *mut c_char;
    if let Some(old) = cache_ref.insert(key, arc) {
        drop(old);
    }
    ptr
}

#[derive(Debug, Clone)]
struct ConnectionState {
    database: duckdb::duckdb_database,
    handle: duckdb::duckdb_connection,
}

#[derive(Debug, Error)]
enum DuckDbError {
    #[error("{0}")]
    Message(String),

    #[error("sql text contains embedded NUL byte")]
    EmbeddedNull,
}

impl DuckDbError {
    fn message<S: Into<String>>(msg: S) -> Self {
        DuckDbError::Message(msg.into())
    }
}

impl From<DuckDbError> for Duckerror {
    fn from(err: DuckDbError) -> Self {
        match err {
            DuckDbError::Message(msg) => Duckerror::Internal(msg),
            DuckDbError::EmbeddedNull => {
                Duckerror::Invalidargument("string contains embedded null byte".to_string())
            }
        }
    }
}

impl From<Duckerror> for DuckDbError {
    fn from(err: Duckerror) -> Self {
        DuckDbError::Message(format_duckerror(&err))
    }
}

unsafe fn set_config_option(
    config: duckdb::duckdb_config,
    key: &str,
    value: &str,
) -> Result<(), DuckDbError> {
    let key_c = CString::new(key)
        .map_err(|_| DuckDbError::message("config key contains interior null byte"))?;
    let value_c = CString::new(value)
        .map_err(|_| DuckDbError::message("config value contains interior null byte"))?;
    let result = duckdb::duckdb_set_config(config, key_c.as_ptr(), value_c.as_ptr());
    if result == DUCKDB_SUCCESS {
        Ok(())
    } else {
        Err(DuckDbError::message(format!(
            "duckdb_set_config({key}, {value}) failed"
        )))
    }
}

#[cfg(not(feature = "browser"))]
unsafe fn configure_wasi_sandbox_config(
    config: duckdb::duckdb_config,
    preopens: &[(Descriptor, String)],
) -> Result<(), DuckDbError> {
    // Intentionally a no-op. The filesystem sandbox is provided by the wasi-fs
    // shims, which confine *all* DuckDB file access (open, read_csv/read_text,
    // COPY, attached databases) to the WASI preopened directories — nothing can
    // be reached outside them regardless of DuckDB settings.
    //
    // DuckDB's own `allowed_directories` allowlist cannot add finer scoping in
    // this wasm build: `duckdb_set_config("allowed_directories", ...)` is
    // rejected at config-creation time, and setting it at runtime does not
    // actually enforce (a path outside the allowlist is still read). The only
    // working DuckDB-level knob is the coarse `enable_external_access` boolean,
    // which is exposed to callers as an opt-in via `open-with-config`
    // (`enable_external_access=false` disables read_csv/read_text/COPY/attach).
    let _ = (config, preopens);
    Ok(())
}

#[cfg(not(feature = "browser"))]
fn normalize_allowed_directory(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.replace('\\', "/");
    let normalized = if normalized == "." {
        "/".to_string()
    } else {
        normalized
    };
    Some(normalized.trim_end_matches('/').to_string())
}

#[cfg(not(feature = "browser"))]
fn build_allowed_directories_literal_from_paths<'a, I>(paths: I) -> Option<String>
where
    I: Iterator<Item = &'a str>,
{
    let mut entries = paths
        .filter_map(|guest_path| normalize_allowed_directory(guest_path))
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return None;
    }
    entries.sort();
    entries.dedup();
    let mut literal = String::from("[");
    for (idx, entry) in entries.iter().enumerate() {
        if idx > 0 {
            literal.push(',');
        }
        literal.push('"');
        literal.push_str(&escape_list_value(entry));
        literal.push('"');
    }
    literal.push(']');
    Some(literal)
}

#[cfg(not(feature = "browser"))]
fn escape_list_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(not(feature = "browser"))]
fn select_temp_directory_from_paths<'a, I>(paths: I) -> Option<String>
where
    I: Iterator<Item = &'a str>,
{
    paths
        .filter_map(|guest_path| normalize_allowed_directory(guest_path))
        .map(|root| append_child_path(&root, ".duckdb-tmp"))
        .next()
}

#[cfg(not(feature = "browser"))]
fn append_child_path(root: &str, child: &str) -> String {
    if root.is_empty() || root == "." {
        child.to_string()
    } else if root.ends_with('/') {
        format!("{root}{child}")
    } else {
        format!("{root}/{child}")
    }
}

#[cfg(all(test, not(feature = "browser")))]
mod config_tests {
    use super::{
        append_child_path, build_allowed_directories_literal_from_paths,
        normalize_allowed_directory, select_temp_directory_from_paths,
    };

    #[test]
    fn normalize_strips_trailing_separators() {
        assert_eq!(
            normalize_allowed_directory("/var/lib/duckdb/").as_deref(),
            Some("/var/lib/duckdb")
        );
        assert_eq!(
            normalize_allowed_directory("C:\\\\data\\duckdb").as_deref(),
            Some("C:/data/duckdb")
        );
        assert_eq!(normalize_allowed_directory("   ").is_none(), true);
    }

    #[test]
    fn build_list_literal_deduplicates_and_escapes() {
        let dirs = vec!["/data", "/data/", "/tmp/with\"quote"];
        let literal =
            build_allowed_directories_literal_from_paths(dirs.iter().copied()).expect("literal");
        assert_eq!(literal, "[\"/data\",\"/tmp/with\\\"quote\"]");
    }

    #[test]
    fn append_child_handles_root_cases() {
        assert_eq!(append_child_path(".", ".duckdb-tmp"), ".duckdb-tmp");
        assert_eq!(
            append_child_path("/var/lib", ".duckdb-tmp"),
            "/var/lib/.duckdb-tmp"
        );
        assert_eq!(
            append_child_path("/var/lib/", ".duckdb-tmp"),
            "/var/lib/.duckdb-tmp"
        );
    }

    #[test]
    fn select_temp_directory_uses_first_preopen() {
        let dirs = vec!["/sandbox", "/other"];
        let choice =
            select_temp_directory_from_paths(dirs.iter().copied()).expect("expected temp dir");
        assert_eq!(choice, "/sandbox/.duckdb-tmp");
    }
}

/// Connection-free unit coverage for the dispatch + `complex` value-marshalling
/// helpers. These run on the host with NO live DuckDB (the unsafe vector
/// readers/writers need a connection and are exercised by the ducklink smoke
/// suite + integration tests), but every pure decision the marshalling makes --
/// the statement splitter, the LOAD detector, and the `complex` trust-boundary
/// caps (depth / list length / JSON-shape-vs-type) -- is the single source of
/// truth the unsafe paths call into, so testing them here covers the real logic.
#[cfg(test)]
mod marshalling_tests {
    use super::{
        complex_depth_within_cap, complex_list_len_within_cap, contains_load_keyword_ascii_ci,
        json_matches_complex_kind, split_sql_statements, statement_is_load, ComplexKind,
        COMPLEX_MAX_DEPTH, COMPLEX_MAX_LIST_LEN,
    };
    use serde_json::json;

    // ----- statement splitter (#73 LOAD-aware split) -----

    #[test]
    fn split_single_statement() {
        assert_eq!(split_sql_statements("SELECT 1"), vec!["SELECT 1"]);
    }

    #[test]
    fn split_multiple_statements() {
        let stmts = split_sql_statements("SELECT 1; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 1");
        assert_eq!(stmts[1].trim(), "SELECT 2");
    }

    #[test]
    fn split_empty_sql_is_empty() {
        assert!(split_sql_statements("").is_empty());
        // whitespace-only trailing buffer is dropped, not panicked on.
        assert!(split_sql_statements("   ").is_empty());
    }

    #[test]
    fn split_bare_semicolons_yield_blank_stmts_not_panic() {
        // `;;;` produces empty statements (one per separator); the callers skip
        // blanks via `stmt.trim().is_empty()`. The point here is no panic and
        // each produced statement is blank.
        let stmts = split_sql_statements(";;;");
        assert!(stmts.iter().all(|s| s.trim().is_empty()));
    }

    #[test]
    fn split_semicolon_inside_single_quote_is_not_a_boundary() {
        let stmts = split_sql_statements("SELECT ';'; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT ';'");
    }

    #[test]
    fn split_semicolon_inside_double_quote_is_not_a_boundary() {
        let stmts = split_sql_statements("SELECT \"a;b\"; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT \"a;b\"");
    }

    #[test]
    fn split_escaped_quote_doubling_stays_in_string() {
        // '' inside a single-quoted string is an escaped quote, not a close.
        let stmts = split_sql_statements("SELECT 'it''s; fine'");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "SELECT 'it''s; fine'");
    }

    #[test]
    fn split_unbalanced_quote_does_not_panic() {
        // A dangling open quote must not loop forever or panic; the trailing
        // buffer is flushed as a single (malformed) statement.
        let stmts = split_sql_statements("SELECT 'unterminated");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "SELECT 'unterminated");
    }

    // ----- LOAD detection -----

    #[test]
    fn statement_is_load_detects_load_case_insensitive() {
        assert!(statement_is_load("LOAD spatial"));
        assert!(statement_is_load("  load httpfs"));
        assert!(statement_is_load("LoAd json"));
    }

    #[test]
    fn statement_is_load_rejects_non_load() {
        assert!(!statement_is_load("SELECT 1"));
        assert!(!statement_is_load("INSTALL spatial"));
        assert!(!statement_is_load("")); // empty -> not load, no panic
        assert!(!statement_is_load("   "));
        // a word that merely starts with "load" must not match.
        assert!(!statement_is_load("loader()"));
    }

    // ----- LOAD keyword pre-filter (split fast-path guard) -----

    #[test]
    fn load_prefilter_matches_whole_word_ci() {
        assert!(contains_load_keyword_ascii_ci("LOAD spatial"));
        assert!(contains_load_keyword_ascii_ci("install x; load httpfs"));
        assert!(contains_load_keyword_ascii_ci("  LoAd  json  "));
        assert!(contains_load_keyword_ascii_ci("SELECT 1; LOAD y"));
    }

    #[test]
    fn load_prefilter_rejects_substrings_and_absence() {
        // The common hot path: a plain query has no LOAD -> prefilter bails,
        // skipping the allocating split entirely.
        assert!(!contains_load_keyword_ascii_ci("SELECT 1"));
        assert!(!contains_load_keyword_ascii_ci("SELECT load_factor FROM t"));
        assert!(!contains_load_keyword_ascii_ci("preload"));
        assert!(!contains_load_keyword_ascii_ci("reloaded"));
        assert!(!contains_load_keyword_ascii_ci(""));
    }

    // ----- complex depth cap (deeply-nested JSON guard) -----

    #[test]
    fn depth_cap_accepts_shallow() {
        assert!(complex_depth_within_cap(&json!(5), COMPLEX_MAX_DEPTH));
        assert!(complex_depth_within_cap(&json!([1, 2, 3]), COMPLEX_MAX_DEPTH));
        assert!(complex_depth_within_cap(
            &json!({"a": [1, {"b": 2}]}),
            COMPLEX_MAX_DEPTH
        ));
        assert!(complex_depth_within_cap(&json!(null), COMPLEX_MAX_DEPTH));
    }

    #[test]
    fn depth_cap_rejects_overdeep_array() {
        // Build [[[...]]] one level past the cap.
        let mut v = json!(0);
        for _ in 0..(COMPLEX_MAX_DEPTH + 2) {
            v = serde_json::Value::Array(vec![v]);
        }
        assert!(!complex_depth_within_cap(&v, COMPLEX_MAX_DEPTH));
    }

    #[test]
    fn depth_cap_rejects_overdeep_object() {
        let mut v = json!(0);
        for _ in 0..(COMPLEX_MAX_DEPTH + 2) {
            let mut m = serde_json::Map::new();
            m.insert("f".to_string(), v);
            v = serde_json::Value::Object(m);
        }
        assert!(!complex_depth_within_cap(&v, COMPLEX_MAX_DEPTH));
    }

    #[test]
    fn depth_cap_boundary_is_exact() {
        // Exactly `max_depth` levels of array nesting (a scalar wrapped N times)
        // is accepted; N+1 is rejected.
        let build = |levels: u32| {
            let mut v = json!(0);
            for _ in 0..levels {
                v = serde_json::Value::Array(vec![v]);
            }
            v
        };
        assert!(complex_depth_within_cap(&build(COMPLEX_MAX_DEPTH), COMPLEX_MAX_DEPTH));
        assert!(!complex_depth_within_cap(
            &build(COMPLEX_MAX_DEPTH + 1),
            COMPLEX_MAX_DEPTH
        ));
    }

    // ----- list length cap (oversized array guard) -----

    #[test]
    fn list_len_cap() {
        assert!(complex_list_len_within_cap(0, COMPLEX_MAX_LIST_LEN));
        assert!(complex_list_len_within_cap(COMPLEX_MAX_LIST_LEN, COMPLEX_MAX_LIST_LEN));
        assert!(!complex_list_len_within_cap(
            COMPLEX_MAX_LIST_LEN + 1,
            COMPLEX_MAX_LIST_LEN
        ));
    }

    // ----- JSON-shape-vs-type mismatch guard -----

    #[test]
    fn shape_list_requires_array() {
        assert!(json_matches_complex_kind(&json!([1, 2]), ComplexKind::List));
        // type says LIST but JSON is a number / object -> mismatch (must error).
        assert!(!json_matches_complex_kind(&json!(7), ComplexKind::List));
        assert!(!json_matches_complex_kind(&json!({"a": 1}), ComplexKind::List));
    }

    #[test]
    fn shape_struct_requires_object() {
        assert!(json_matches_complex_kind(&json!({"a": 1}), ComplexKind::Struct));
        assert!(!json_matches_complex_kind(&json!([1, 2]), ComplexKind::Struct));
        assert!(!json_matches_complex_kind(&json!(7), ComplexKind::Struct));
    }

    #[test]
    fn shape_scalar_rejects_containers() {
        assert!(json_matches_complex_kind(&json!(7), ComplexKind::Scalar));
        assert!(json_matches_complex_kind(&json!("s"), ComplexKind::Scalar));
        assert!(!json_matches_complex_kind(&json!([1]), ComplexKind::Scalar));
        assert!(!json_matches_complex_kind(&json!({"a": 1}), ComplexKind::Scalar));
    }

    #[test]
    fn shape_null_always_allowed() {
        // A JSON null marks the row invalid regardless of declared kind.
        assert!(json_matches_complex_kind(&json!(null), ComplexKind::List));
        assert!(json_matches_complex_kind(&json!(null), ComplexKind::Struct));
        assert!(json_matches_complex_kind(&json!(null), ComplexKind::Scalar));
    }

    // ----- malformed JSON: serde returns Err, never panics -----

    #[test]
    fn malformed_json_is_err_not_panic() {
        // This mirrors the parse step in write_duckvalue_to_vector's Complex arm:
        // a malformed component payload must surface as an Err, not a panic.
        assert!(serde_json::from_str::<serde_json::Value>("{not json").is_err());
        assert!(serde_json::from_str::<serde_json::Value>("[1, 2,").is_err());
        assert!(serde_json::from_str::<serde_json::Value>("").is_err());
    }

    #[test]
    fn pathologically_deep_json_string_parses_to_err() {
        // serde_json caps parse recursion (default 128) and returns Err rather
        // than overflowing the stack; confirm an adversarial deep string is
        // rejected at the parse boundary.
        let deep = "[".repeat(5000) + &"]".repeat(5000);
        let parsed = serde_json::from_str::<serde_json::Value>(&deep);
        assert!(parsed.is_err());
    }
}

impl ConnectionState {
    fn open(path: Option<&str>) -> Result<Self, DuckDbError> {
        Self::open_with_config(path, &[])
    }

    fn open_with_config(
        path: Option<&str>,
        options: &[(String, String)],
    ) -> Result<Self, DuckDbError> {
        // Make compiled-in (embedded) extension scalars available; their
        // definitions are replayed onto this connection by register_connection_handle.
        register_embedded_extensions();
        #[cfg(feature = "browser")]
        if let Some(_) = path {
            return Err(DuckDbError::message(
                "persistent storage is not available in the browser-oriented build",
            ));
        }

        unsafe {
            struct DuckDbConfigGuard(duckdb::duckdb_config);
            impl Drop for DuckDbConfigGuard {
                fn drop(&mut self) {
                    unsafe {
                        if !self.0.is_null() {
                            duckdb::duckdb_destroy_config(&mut self.0);
                        }
                    }
                }
            }

            let config_guard = {
                let mut config: duckdb::duckdb_config = ptr::null_mut();
                if duckdb::duckdb_create_config(&mut config) != DUCKDB_SUCCESS {
                    return Err(DuckDbError::message(
                        "duckdb_create_config failed for WASI build",
                    ));
                }
                DuckDbConfigGuard(config)
            };

            let preopen_dirs = wasi_fs::preopened_directories();
            #[cfg(not(feature = "browser"))]
            {
                configure_wasi_sandbox_config(config_guard.0, preopen_dirs)?;
            }

            for (name, value) in options {
                let c_name = CString::new(name.as_str()).map_err(|_| DuckDbError::EmbeddedNull)?;
                let c_value =
                    CString::new(value.as_str()).map_err(|_| DuckDbError::EmbeddedNull)?;
                if duckdb::duckdb_set_config(config_guard.0, c_name.as_ptr(), c_value.as_ptr())
                    != DUCKDB_SUCCESS
                {
                    return Err(DuckDbError::message(format!(
                        "invalid configuration option '{name}'"
                    )));
                }
            }

            let c_path = match path {
                Some(p) => Some(CString::new(p).map_err(|_| DuckDbError::EmbeddedNull)?),
                None => None,
            };

            let mut database: duckdb::duckdb_database = ptr::null_mut();
            let mut error: *mut std::os::raw::c_char = ptr::null_mut();
            let state = duckdb::duckdb_open_ext(
                c_path.as_ref().map(|s| s.as_ptr()).unwrap_or(ptr::null()),
                &mut database,
                config_guard.0,
                &mut error,
            );

            drop(config_guard);

            if state != DUCKDB_SUCCESS {
                let message = extract_and_free_c_string(error)
                    .unwrap_or_else(|| "duckdb_open_ext failed".to_string());
                if !database.is_null() {
                    duckdb::duckdb_close(&mut database);
                }
                return Err(DuckDbError::message(message));
            }

            // M2: storage StorageExtensions are now registered DYNAMICALLY at
            // query time (see ConnectionState::sync_storage_extensions), based on
            // the ATTACH `TYPE` names that components declare via the host's
            // `register-storage` WIT call. The old hardcoded "sqlitewasm"
            // registration here is intentionally removed; the dynamic path
            // (storage-host.storage-list-types -> wasm_register_storage_extension)
            // covers sqlitewasm and any other backend (mysql, postgres, ...).

            let mut handle: duckdb::duckdb_connection = ptr::null_mut();
            let state = duckdb::duckdb_connect(database, &mut handle);
            if state != DUCKDB_SUCCESS {
                let message = extract_last_error(handle)
                    .unwrap_or_else(|| "duckdb_connect failed".to_string());
                duckdb::duckdb_close(&mut database);
                return Err(DuckDbError::message(message));
            }

            register_connection_handle(handle, database).map_err(|err| {
                duckdb::duckdb_disconnect(&mut handle);
                duckdb::duckdb_close(&mut database);
                DuckDbError::from(err)
            })?;

            Ok(ConnectionState { database, handle })
        }
    }

    fn interrupt(&self) {
        unsafe {
            duckdb::duckdb_interrupt(self.handle);
        }
    }

    /// Pulls the set of ATTACH `TYPE` names that storage components have
    /// registered with the host (via the `register-storage` WIT call) and
    /// registers a wasm StorageExtension for each one not yet registered. This
    /// is the DYNAMIC replacement for the old hardcoded "sqlitewasm" call at
    /// DB-open: register-storage happens at LOAD time (mid-session), and
    /// `StorageExtension::Register` just inserts into the DBConfig callback
    /// registry (lock-guarded, copy-on-write), which ATTACH reads at bind time
    /// -- so mid-session registration is safe. Called lazily before each query
    /// so any `ATTACH ... (TYPE x)` sees the backend its component declared.
    ///
    /// The C++ side guards against double-registration via StorageExtension::Find,
    /// and we additionally short-circuit already-seen types here.
    fn sync_storage_extensions(&self) {
        // httpfs M1: register the stub FileSystem subsystem (http:// / https://)
        // on this database. The C++ side is idempotent (process-wide once-guard +
        // dup-name try/catch), so calling it before every query is cheap and
        // proves the mechanism without dynamic gating (that is M2).
        unsafe {
            wasm_register_file_system(self.database);
        }

        // Item 3 / M2a: register each custom index TYPE a component has declared
        // (via `register-index-type`, surfaced through the
        // `index-host.index-type-list` import) so `CREATE INDEX ... USING <type>`
        // routes to a WasmBoundIndex bound to that type. The C++ side is
        // idempotent (FindByName dup-check), so calling before every query is
        // cheap. This replaces M1's hardcoded "wasm_hnsw".
        for type_name in bindings::duckdb::extension::index_host::index_type_list() {
            if let Ok(c_type) = CString::new(type_name.as_str()) {
                unsafe {
                    wasm_register_index_type(self.database, c_type.as_ptr());
                }
            }
        }

        // Item 2: register any collations components have declared (mid-session).
        self.sync_collations();

        // Item 4: pull any pragmas components have declared (mid-session) into the
        // process-wide registry, so `PRAGMA <name>(...)` can be intercepted.
        sync_pragmas();

        let types = bindings::duckdb::extension::storage_host::storage_list_types();
        if types.is_empty() {
            return;
        }
        let seen = STORAGE_REGISTERED_TYPES.get_or_init(|| Mutex::new(Default::default()));
        let mut guard = match seen.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        for type_name in types {
            if guard.contains(&type_name) {
                continue;
            }
            if let Ok(c_type) = CString::new(type_name.as_str()) {
                unsafe {
                    wasm_register_storage_extension(self.database, c_type.as_ptr());
                }
                guard.insert(type_name);
            }
        }
    }

    /// Item 2: pulls the collations components have declared (via the
    /// `register-collation` WIT call, surfaced through `collation-host.collation-list`)
    /// and registers a DuckDB collation for each one not yet registered. A
    /// collation reuses an already-registered sort-key scalar as its transform;
    /// the C++ shim looks the scalar up in the catalog and wraps it in a
    /// CreateCollationInfo. Mid-session safe: the binder reads collations from the
    /// system catalog at bind time, and CreateCollation uses IGNORE_ON_CONFLICT.
    /// Called lazily before each query (after the scalar-registration drain that
    /// happens at LOAD time, so the transform scalar already exists).
    fn sync_collations(&self) {
        let collations = bindings::duckdb::extension::collation_host::collation_list();
        if collations.is_empty() {
            return;
        }
        let seen = COLLATION_REGISTERED_NAMES.get_or_init(|| Mutex::new(Default::default()));
        let mut guard = match seen.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        for spec in collations {
            if guard.contains(&spec.name) {
                continue;
            }
            let c_name = match CString::new(spec.name.as_str()) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let c_scalar = match CString::new(spec.transform_scalar.as_str()) {
                Ok(s) => s,
                Err(_) => continue,
            };
            unsafe {
                wasm_register_collation(
                    self.database,
                    c_name.as_ptr(),
                    c_scalar.as_ptr(),
                    spec.combinable,
                );
            }
            guard.insert(spec.name);
        }
    }

    fn execute(&self, sql: &str) -> Result<QueryResult, DuckDbError> {
        // Item 4: intercept `PRAGMA <name>(...)` for component-declared pragmas.
        if let Some(result) = self.run_intercepted_pragma(sql)? {
            return Ok(result);
        }
        let (columns, rows) = self.collect_rows(sql)?;
        Ok(QueryResult { columns, rows })
    }

    /// Item 4: if `sql` is `PRAGMA <name>(...)` for a component-declared pragma,
    /// dispatch it (the component RETURNS a SQL script as text -- it does NOT
    /// re-enter SQL during the callback), run that script on this connection, and
    /// return an empty result. Returns `Ok(None)` if `sql` is not such a pragma.
    fn run_intercepted_pragma(&self, sql: &str) -> Result<Option<QueryResult>, DuckDbError> {
        let parsed = match parse_pragma_call(sql) {
            Some(p) => p,
            None => return Ok(None),
        };
        // Ensure the pragma registry is populated (pragmas are declared at LOAD;
        // the pull is otherwise lazy inside collect_rows).
        sync_pragmas();
        let handle = {
            let guard = declared_pragmas()
                .lock()
                .expect("declared pragmas mutex poisoned");
            match guard.get(&parsed.name.to_ascii_lowercase()) {
                Some(h) => *h,
                None => return Ok(None),
            }
        };

        // Dispatch the pragma to the owning component, passing the parsed args as
        // text values. The component returns the generated SQL script as a text
        // value (Some) or none.
        let args: Vec<Duckvalue> = parsed.args.into_iter().map(Duckvalue::Text).collect();
        let returned = callback_dispatch::call_pragma(handle, &args)
            .map_err(|err| DuckDbError::message(format_duckerror(&err)))?;

        let script = match returned {
            Some(Duckvalue::Text(s)) => s,
            Some(_) => {
                return Err(DuckDbError::message(format!(
                    "pragma '{}' must return a SQL script (text)",
                    parsed.name
                )))
            }
            None => String::new(),
        };

        // Run the returned script on this same connection. Multiple statements are
        // separated by ';'; duckdb_query runs the last statement's result, so we
        // execute each non-empty statement individually for robustness.
        for statement in split_sql_statements(&script) {
            let trimmed = statement.trim();
            if trimmed.is_empty() {
                continue;
            }
            let _ = self.collect_rows(trimmed)?;
        }

        Ok(QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
        })
        .map(Some)
    }

    /// Runs `sql` and serializes the entire result to an Arrow IPC stream, using
    /// DuckDB's (non-deprecated) result + data-chunk to Arrow conversion API.
    fn query_arrow_ipc(&self, sql: &str) -> Result<Vec<u8>, DuckDbError> {
        self.sync_storage_extensions();
        // Storage-registration timing: a `LOAD <storage-ext>` registers its
        // ATTACH `TYPE` host-side, but `sync_storage_extensions` above only ran
        // ONCE, before this batch executes. So a batch like
        // `LOAD x; ATTACH ... (TYPE x)` would bind the ATTACH before the type is
        // registered. When the batch carries a LOAD AND more statements, run each
        // statement on its own (re-syncing before each) so a dependent ATTACH
        // sees the type its preceding LOAD just declared. Single statements (the
        // common case) keep the fast single-`duckdb_query` path untouched.
        if let Some(last) = self.split_for_load_sync(sql) {
            for stmt in &last.0 {
                if stmt.trim().is_empty() {
                    continue;
                }
                self.sync_storage_extensions();
                let _ = self.query_arrow_ipc_one(stmt)?;
            }
            self.sync_storage_extensions();
            return self.query_arrow_ipc_one(&last.1);
        }
        self.query_arrow_ipc_one(sql)
    }

    fn query_arrow_ipc_one(&self, sql: &str) -> Result<Vec<u8>, DuckDbError> {
        use arrow_array::{ffi::from_ffi, RecordBatch, StructArray};
        use arrow_data::ffi::FFI_ArrowArray;
        use arrow_ipc::writer::StreamWriter;
        use arrow_schema::{ffi::FFI_ArrowSchema, Schema};
        use std::sync::Arc;

        // Destroys the arrow options on every return path.
        struct ArrowOptionsGuard(duckdb::duckdb_arrow_options);
        impl Drop for ArrowOptionsGuard {
            fn drop(&mut self) {
                unsafe {
                    if !self.0.is_null() {
                        duckdb::duckdb_destroy_arrow_options(&mut self.0);
                    }
                }
            }
        }

        let c_sql = CString::new(sql).map_err(|_| DuckDbError::EmbeddedNull)?;

        unsafe {
            let mut result = std::mem::MaybeUninit::<duckdb::duckdb_result>::zeroed();
            let state = duckdb::duckdb_query(self.handle, c_sql.as_ptr(), result.as_mut_ptr());
            let mut result = result.assume_init();
            if state != DUCKDB_SUCCESS {
                let message = extract_result_error(&result)
                    .unwrap_or_else(|| "duckdb_query failed".to_string());
                duckdb::duckdb_destroy_result(&mut result);
                return Err(DuckDbError::message(message));
            }

            let collected = (|| -> Result<Vec<u8>, DuckDbError> {
                let options =
                    ArrowOptionsGuard(duckdb::duckdb_result_get_arrow_options(&mut result));

                // Build the Arrow schema from the result's column types/names.
                let column_count = duckdb::duckdb_column_count(&mut result);
                let mut logical_types: Vec<duckdb::duckdb_logical_type> =
                    Vec::with_capacity(column_count as usize);
                let mut names: Vec<*const c_char> = Vec::with_capacity(column_count as usize);
                for col in 0..column_count {
                    logical_types.push(duckdb::duckdb_column_logical_type(&mut result, col));
                    names.push(duckdb::duckdb_column_name(&mut result, col));
                }

                let mut ffi_schema = FFI_ArrowSchema::empty();
                let schema_err = duckdb::duckdb_to_arrow_schema(
                    options.0,
                    logical_types.as_mut_ptr(),
                    names.as_mut_ptr(),
                    column_count,
                    &mut ffi_schema as *mut FFI_ArrowSchema as *mut c_void,
                );
                for lt in &mut logical_types {
                    duckdb::duckdb_destroy_logical_type(lt);
                }
                if let Some(message) = take_error_data(schema_err) {
                    return Err(DuckDbError::message(format!("arrow schema: {message}")));
                }

                let schema = Arc::new(
                    Schema::try_from(&ffi_schema)
                        .map_err(|e| DuckDbError::message(format!("arrow schema: {e}")))?,
                );

                // Fetch chunks and convert each to an Arrow struct array.
                let mut batches: Vec<RecordBatch> = Vec::new();
                loop {
                    let mut chunk = duckdb::duckdb_fetch_chunk(result);
                    if chunk.is_null() {
                        break;
                    }
                    let mut ffi_array = FFI_ArrowArray::empty();
                    let array_err = duckdb::duckdb_data_chunk_to_arrow(
                        options.0,
                        chunk,
                        &mut ffi_array as *mut FFI_ArrowArray as *mut c_void,
                    );
                    duckdb::duckdb_destroy_data_chunk(&mut chunk);
                    if let Some(message) = take_error_data(array_err) {
                        return Err(DuckDbError::message(format!("arrow array: {message}")));
                    }
                    let data = from_ffi(ffi_array, &ffi_schema)
                        .map_err(|e| DuckDbError::message(format!("arrow import: {e}")))?;
                    batches.push(RecordBatch::from(StructArray::from(data)));
                }

                let mut buffer: Vec<u8> = Vec::new();
                {
                    let mut writer = StreamWriter::try_new(&mut buffer, schema.as_ref())
                        .map_err(|e| DuckDbError::message(format!("arrow ipc: {e}")))?;
                    for batch in &batches {
                        writer
                            .write(batch)
                            .map_err(|e| DuckDbError::message(format!("arrow ipc: {e}")))?;
                    }
                    writer
                        .finish()
                        .map_err(|e| DuckDbError::message(format!("arrow ipc: {e}")))?;
                }
                Ok(buffer)
            })();

            duckdb::duckdb_destroy_result(&mut result);
            collected
        }
    }

    fn collect_rows(&self, sql: &str) -> Result<(Vec<Columndef>, Vec<Row>), DuckDbError> {
        self.sync_storage_extensions();
        // See `query_arrow_ipc` for the rationale: split a LOAD-carrying batch so
        // each statement re-syncs storage types before it binds. Single
        // statements keep the fast path.
        if let Some(last) = self.split_for_load_sync(sql) {
            for stmt in &last.0 {
                if stmt.trim().is_empty() {
                    continue;
                }
                self.sync_storage_extensions();
                let _ = self.collect_rows_one(stmt)?;
            }
            self.sync_storage_extensions();
            return self.collect_rows_one(&last.1);
        }
        self.collect_rows_one(sql)
    }

    /// If `sql` is a multi-statement batch that contains a `LOAD`, returns the
    /// leading statements (`.0`) and the trailing statement (`.1`) to run one at a
    /// time with a storage-type re-sync before each. Returns `None` for a single
    /// statement or a batch with no LOAD (the common, fast single-query path).
    fn split_for_load_sync(&self, sql: &str) -> Option<(Vec<String>, String)> {
        // Fast-path bail BEFORE the allocating statement split: the split only
        // matters for a multi-statement batch that contains a LOAD. A single
        // statement has no `;` separating two statements, and a batch with no
        // LOAD never needs the per-statement re-sync. Both are cheap byte scans
        // (no allocation) and skip building the Vec<String> for the common
        // single-statement / non-LOAD query -- the hot path.
        if !sql.contains(';') || !contains_load_keyword_ascii_ci(sql) {
            return None;
        }
        let stmts = split_sql_statements(sql);
        if stmts.len() < 2 {
            return None;
        }
        let has_load = stmts.iter().any(|s| statement_is_load(s));
        if !has_load {
            return None;
        }
        let mut leading: Vec<String> = stmts;
        let last = leading.pop().unwrap();
        Some((leading, last))
    }

    fn collect_rows_one(&self, sql: &str) -> Result<(Vec<Columndef>, Vec<Row>), DuckDbError> {
        let c_sql = CString::new(sql).map_err(|_| DuckDbError::EmbeddedNull)?;

        unsafe {
            let mut result = std::mem::MaybeUninit::<duckdb::duckdb_result>::zeroed();
            let state = duckdb::duckdb_query(self.handle, c_sql.as_ptr(), result.as_mut_ptr());
            let mut result = result.assume_init();

            if state != DUCKDB_SUCCESS {
                let message = extract_result_error(&result)
                    .unwrap_or_else(|| "duckdb_query failed".to_string());
                duckdb::duckdb_destroy_result(&mut result);
                return Err(DuckDbError::message(message));
            }
            let query_result = marshal_result(&result)?;
            duckdb::duckdb_destroy_result(&mut result);
            Ok(query_result)
        }
    }
}

impl Drop for ConnectionState {
    fn drop(&mut self) {
        unsafe {
            unregister_connection_handle(self.handle);
            if !self.handle.is_null() {
                duckdb::duckdb_disconnect(&mut self.handle);
            }
            if !self.database.is_null() {
                duckdb::duckdb_close(&mut self.database);
            }
        }
    }
}

impl exported_database::GuestConnection for ConnectionState {}

struct QueryStream {
    columns: Vec<Columndef>,
    rows: RefCell<Vec<Row>>,
    cursor: RefCell<usize>,
}

impl QueryStream {
    fn new(columns: Vec<Columndef>, rows: Vec<Row>) -> Self {
        QueryStream {
            columns,
            rows: RefCell::new(rows),
            cursor: RefCell::new(0),
        }
    }
}

impl exported_database::GuestResultStream for QueryStream {
    fn schema(&self) -> Vec<Columndef> {
        self.columns.clone()
    }

    fn next(&self, max_rows: u32) -> Result<Option<Vec<Row>>, Duckerror> {
        let mut cursor = self.cursor.borrow_mut();
        let rows_ref = self.rows.borrow();
        if *cursor >= rows_ref.len() {
            return Ok(None);
        }
        let remaining = rows_ref.len() - *cursor;
        if remaining == 0 {
            *cursor = rows_ref.len();
            return Ok(None);
        }
        let batch_len = if max_rows == 0 {
            remaining
        } else {
            remaining.min(max_rows as usize)
        };
        let end = *cursor + batch_len;
        let slice = rows_ref[*cursor..end].to_vec();
        *cursor = end;
        Ok(Some(slice))
    }

    fn close(&self) -> () {
        let mut cursor = self.cursor.borrow_mut();
        *cursor = self.rows.borrow().len();
        self.rows.borrow_mut().clear();
        ()
    }
}

/// A compiled DuckDB prepared statement, reusable across executions with
/// different bound parameters.
struct PreparedStatementState {
    stmt: RefCell<duckdb::duckdb_prepared_statement>,
}

impl PreparedStatementState {
    fn prepare(conn: &ConnectionState, sql: &str) -> Result<Self, DuckDbError> {
        let c_sql = CString::new(sql).map_err(|_| DuckDbError::EmbeddedNull)?;
        unsafe {
            let mut stmt: duckdb::duckdb_prepared_statement = ptr::null_mut();
            let state = duckdb::duckdb_prepare(conn.handle, c_sql.as_ptr(), &mut stmt);
            if state != DUCKDB_SUCCESS {
                let message = prepare_error_message(stmt);
                if !stmt.is_null() {
                    duckdb::duckdb_destroy_prepare(&mut stmt);
                }
                return Err(DuckDbError::message(message));
            }
            Ok(PreparedStatementState {
                stmt: RefCell::new(stmt),
            })
        }
    }
}

/// Extracts the message from a `duckdb_error_data` if it represents an error,
/// then destroys it. Returns `None` when there is no error (or a null handle).
unsafe fn take_error_data(mut err: duckdb::duckdb_error_data) -> Option<String> {
    if err.is_null() {
        return None;
    }
    let message = if duckdb::duckdb_error_data_has_error(err) {
        let ptr = duckdb::duckdb_error_data_message(err);
        if ptr.is_null() {
            Some("arrow conversion failed".to_string())
        } else {
            Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    } else {
        None
    };
    duckdb::duckdb_destroy_error_data(&mut err);
    message
}

unsafe fn prepare_error_message(stmt: duckdb::duckdb_prepared_statement) -> String {
    if stmt.is_null() {
        return "duckdb_prepare failed".to_string();
    }
    let ptr = duckdb::duckdb_prepare_error(stmt);
    if ptr.is_null() {
        "duckdb_prepare failed".to_string()
    } else {
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

impl Drop for PreparedStatementState {
    fn drop(&mut self) {
        unsafe {
            let mut stmt = *self.stmt.borrow();
            if !stmt.is_null() {
                duckdb::duckdb_destroy_prepare(&mut stmt);
            }
        }
    }
}

impl exported_database::GuestPreparedStatement for PreparedStatementState {
    fn parameter_count(&self) -> u32 {
        unsafe { duckdb::duckdb_nparams(*self.stmt.borrow()) as u32 }
    }

    fn execute(&self, params: Vec<Duckvalue>) -> Result<QueryResult, Duckerror> {
        unsafe {
            let stmt = *self.stmt.borrow();
            duckdb::duckdb_clear_bindings(stmt);
            for (offset, value) in params.iter().enumerate() {
                let idx = (offset + 1) as duckdb::idx_t;
                let state = match value {
                    Duckvalue::Null => duckdb::duckdb_bind_null(stmt, idx),
                    Duckvalue::Boolean(v) => duckdb::duckdb_bind_boolean(stmt, idx, *v),
                    Duckvalue::Int64(v) => duckdb::duckdb_bind_int64(stmt, idx, *v),
                    Duckvalue::Uint64(v) => duckdb::duckdb_bind_uint64(stmt, idx, *v),
                    Duckvalue::Float64(v) => duckdb::duckdb_bind_double(stmt, idx, *v),
                    Duckvalue::Text(v) => duckdb::duckdb_bind_varchar_length(
                        stmt,
                        idx,
                        v.as_ptr() as *const c_char,
                        v.len() as duckdb::idx_t,
                    ),
                    Duckvalue::Blob(bytes) => duckdb::duckdb_bind_blob(
                        stmt,
                        idx,
                        bytes.as_ptr() as *const c_void,
                        bytes.len() as duckdb::idx_t,
                    ),
                    // No dedicated int32/timestamp bind in the minimal C-API; bind
                    // the int64 representation (DuckDB casts to the param's type;
                    // timestamp is micros-since-1970 as int64).
                    Duckvalue::Int32(v) => duckdb::duckdb_bind_int64(stmt, idx, *v as i64),
                    Duckvalue::Timestamp(micros) => {
                        duckdb::duckdb_bind_int64(stmt, idx, *micros)
                    }
                    Duckvalue::Int8(v) => duckdb::duckdb_bind_int64(stmt, idx, *v as i64),
                    Duckvalue::Int16(v) => duckdb::duckdb_bind_int64(stmt, idx, *v as i64),
                    Duckvalue::Uint8(v) => duckdb::duckdb_bind_uint64(stmt, idx, *v as u64),
                    Duckvalue::Uint16(v) => duckdb::duckdb_bind_uint64(stmt, idx, *v as u64),
                    Duckvalue::Uint32(v) => duckdb::duckdb_bind_uint64(stmt, idx, *v as u64),
                    Duckvalue::Float32(v) => duckdb::duckdb_bind_double(stmt, idx, *v as f64),
                    Duckvalue::Date(days) => duckdb::duckdb_bind_int64(stmt, idx, *days as i64),
                    Duckvalue::Time(micros) => duckdb::duckdb_bind_int64(stmt, idx, *micros),
                    Duckvalue::Timestamptz(micros) => {
                        duckdb::duckdb_bind_int64(stmt, idx, *micros)
                    }
                    // No dedicated decimal/interval/uuid bind in the minimal C-API.
                    // Bind the int64 magnitude (DuckDB casts to the param's type);
                    // these composite params are not exercised by the suite.
                    Duckvalue::Decimal(d) => {
                        let v = ((d.upper as i128) << 64 | d.lower as i128) as i64;
                        duckdb::duckdb_bind_int64(stmt, idx, v)
                    }
                    Duckvalue::Interval(iv) => {
                        duckdb::duckdb_bind_int64(stmt, idx, iv.micros)
                    }
                    Duckvalue::Uuid(u) => {
                        duckdb::duckdb_bind_uint64(stmt, idx, u.lo)
                    }
                    // ESCAPE-HATCH: bind the JSON text (DuckDB casts to the param's
                    // declared type). Complex params are not exercised by the suite.
                    Duckvalue::Complex(c) => duckdb::duckdb_bind_varchar_length(
                        stmt,
                        idx,
                        c.json.as_ptr() as *const c_char,
                        c.json.len() as duckdb::idx_t,
                    ),
                };
                if state != DUCKDB_SUCCESS {
                    return Err(Duckerror::from(DuckDbError::message(format!(
                        "failed to bind parameter {idx}"
                    ))));
                }
            }

            let mut result = MaybeUninit::<duckdb::duckdb_result>::zeroed();
            let state = duckdb::duckdb_execute_prepared(stmt, result.as_mut_ptr());
            let mut result = result.assume_init();
            if state != DUCKDB_SUCCESS {
                let message = extract_result_error(&result)
                    .unwrap_or_else(|| "duckdb_execute_prepared failed".to_string());
                duckdb::duckdb_destroy_result(&mut result);
                return Err(Duckerror::from(DuckDbError::message(message)));
            }
            let marshalled = marshal_result(&result);
            duckdb::duckdb_destroy_result(&mut result);
            let (columns, rows) = marshalled.map_err(Duckerror::from)?;
            Ok(QueryResult { columns, rows })
        }
    }
}

/// A DuckDB appender for fast bulk row insertion into an existing table.
struct AppenderState {
    appender: RefCell<duckdb::duckdb_appender>,
}

impl AppenderState {
    fn create(
        conn: &ConnectionState,
        schema: Option<&str>,
        table: &str,
    ) -> Result<Self, DuckDbError> {
        let c_table = CString::new(table).map_err(|_| DuckDbError::EmbeddedNull)?;
        let c_schema = match schema {
            Some(s) => Some(CString::new(s).map_err(|_| DuckDbError::EmbeddedNull)?),
            None => None,
        };
        unsafe {
            let mut appender: duckdb::duckdb_appender = ptr::null_mut();
            let state = duckdb::duckdb_appender_create(
                conn.handle,
                c_schema.as_ref().map(|s| s.as_ptr()).unwrap_or(ptr::null()),
                c_table.as_ptr(),
                &mut appender,
            );
            if state != DUCKDB_SUCCESS {
                let message = appender_error_message(appender);
                if !appender.is_null() {
                    duckdb::duckdb_appender_destroy(&mut appender);
                }
                return Err(DuckDbError::message(message));
            }
            Ok(AppenderState {
                appender: RefCell::new(appender),
            })
        }
    }
}

unsafe fn appender_error_message(appender: duckdb::duckdb_appender) -> String {
    if appender.is_null() {
        return "duckdb_appender_create failed".to_string();
    }
    let ptr = duckdb::duckdb_appender_error(appender);
    if ptr.is_null() {
        "duckdb appender error".to_string()
    } else {
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

impl Drop for AppenderState {
    fn drop(&mut self) {
        unsafe {
            let mut appender = *self.appender.borrow();
            if !appender.is_null() {
                // destroy flushes and closes the appender.
                duckdb::duckdb_appender_destroy(&mut appender);
            }
        }
    }
}

impl exported_database::GuestAppender for AppenderState {
    fn append_row(&self, values: Vec<Duckvalue>) -> Result<(), Duckerror> {
        unsafe {
            let appender = *self.appender.borrow();
            for value in &values {
                let state = match value {
                    Duckvalue::Null => duckdb::duckdb_append_null(appender),
                    Duckvalue::Boolean(v) => duckdb::duckdb_append_bool(appender, *v),
                    Duckvalue::Int64(v) => duckdb::duckdb_append_int64(appender, *v),
                    Duckvalue::Uint64(v) => duckdb::duckdb_append_uint64(appender, *v),
                    Duckvalue::Float64(v) => duckdb::duckdb_append_double(appender, *v),
                    Duckvalue::Text(v) => duckdb::duckdb_append_varchar_length(
                        appender,
                        v.as_ptr() as *const c_char,
                        v.len() as duckdb::idx_t,
                    ),
                    Duckvalue::Blob(bytes) => duckdb::duckdb_append_blob(
                        appender,
                        bytes.as_ptr() as *const c_void,
                        bytes.len() as duckdb::idx_t,
                    ),
                    // Minimal C-API appender: widen int32 / pass timestamp micros
                    // through the int64 append (DuckDB casts to the column type).
                    Duckvalue::Int32(v) => duckdb::duckdb_append_int64(appender, *v as i64),
                    Duckvalue::Timestamp(micros) => {
                        duckdb::duckdb_append_int64(appender, *micros)
                    }
                    Duckvalue::Int8(v) => duckdb::duckdb_append_int64(appender, *v as i64),
                    Duckvalue::Int16(v) => duckdb::duckdb_append_int64(appender, *v as i64),
                    Duckvalue::Uint8(v) => duckdb::duckdb_append_uint64(appender, *v as u64),
                    Duckvalue::Uint16(v) => duckdb::duckdb_append_uint64(appender, *v as u64),
                    Duckvalue::Uint32(v) => duckdb::duckdb_append_uint64(appender, *v as u64),
                    Duckvalue::Float32(v) => duckdb::duckdb_append_double(appender, *v as f64),
                    Duckvalue::Date(days) => duckdb::duckdb_append_int64(appender, *days as i64),
                    Duckvalue::Time(micros) => duckdb::duckdb_append_int64(appender, *micros),
                    Duckvalue::Timestamptz(micros) => {
                        duckdb::duckdb_append_int64(appender, *micros)
                    }
                    // Minimal C-API appender: widen the int64 magnitude (DuckDB
                    // casts to the column type). Composite appends are not used.
                    Duckvalue::Decimal(d) => {
                        let v = ((d.upper as i128) << 64 | d.lower as i128) as i64;
                        duckdb::duckdb_append_int64(appender, v)
                    }
                    Duckvalue::Interval(iv) => {
                        duckdb::duckdb_append_int64(appender, iv.micros)
                    }
                    Duckvalue::Uuid(u) => {
                        duckdb::duckdb_append_uint64(appender, u.lo)
                    }
                    // ESCAPE-HATCH: append the JSON text (DuckDB casts to the
                    // column type). Complex appends are not exercised.
                    Duckvalue::Complex(c) => duckdb::duckdb_append_varchar_length(
                        appender,
                        c.json.as_ptr() as *const c_char,
                        c.json.len() as duckdb::idx_t,
                    ),
                };
                if state != DUCKDB_SUCCESS {
                    return Err(Duckerror::from(DuckDbError::message(appender_error_message(
                        appender,
                    ))));
                }
            }
            if duckdb::duckdb_appender_end_row(appender) != DUCKDB_SUCCESS {
                return Err(Duckerror::from(DuckDbError::message(appender_error_message(
                    appender,
                ))));
            }
            Ok(())
        }
    }

    fn flush(&self) -> Result<(), Duckerror> {
        unsafe {
            let appender = *self.appender.borrow();
            if duckdb::duckdb_appender_flush(appender) != DUCKDB_SUCCESS {
                return Err(Duckerror::from(DuckDbError::message(appender_error_message(
                    appender,
                ))));
            }
            Ok(())
        }
    }

    fn close(&self) -> Result<(), Duckerror> {
        unsafe {
            let appender = *self.appender.borrow();
            if duckdb::duckdb_appender_close(appender) != DUCKDB_SUCCESS {
                return Err(Duckerror::from(DuckDbError::message(appender_error_message(
                    appender,
                ))));
            }
            Ok(())
        }
    }
}

struct Component;

fn register_connection_handle(
    handle: duckdb::duckdb_connection,
    database: duckdb::duckdb_database,
) -> Result<(), Duckerror> {
    {
        let mut guard = active_connections()
            .lock()
            .expect("active connections mutex poisoned");
        guard.push(ConnectionHandle(handle, database));
    }

    let definitions = {
        let guard = scalar_function_definitions()
            .lock()
            .expect("scalar function registry mutex poisoned");
        guard.clone()
    };

    for definition in definitions {
        unsafe {
            if let Err(err) = register_scalar_function_on_connection(handle, &definition) {
                unregister_connection_handle(handle);
                return Err(err);
            }
        }
    }

    let table_definitions = {
        let guard = table_function_definitions()
            .lock()
            .expect("table function registry mutex poisoned");
        guard.clone()
    };

    for definition in table_definitions {
        unsafe {
            if let Err(err) = register_table_function_on_connection(handle, &definition) {
                unregister_connection_handle(handle);
                return Err(err);
            }
        }
    }

    let aggregate_definitions = {
        let guard = aggregate_function_definitions()
            .lock()
            .expect("aggregate function registry mutex poisoned");
        guard.clone()
    };

    for definition in aggregate_definitions {
        unsafe {
            if let Err(err) = register_aggregate_function_on_connection(handle, &definition) {
                unregister_connection_handle(handle);
                return Err(err);
            }
        }
    }

    Ok(())
}

fn unregister_connection_handle(handle: duckdb::duckdb_connection) {
    let mut guard = active_connections()
        .lock()
        .expect("active connections mutex poisoned");
    guard.retain(|conn| conn.0 != handle);
}

impl exported_database::Guest for Component {
    type Connection = ConnectionState;
    type ResultStream = QueryStream;
    type PreparedStatement = PreparedStatementState;
    type Appender = AppenderState;

    fn open(path: Option<String>) -> Result<Connection, String> {
        let state = ConnectionState::open(path.as_deref()).map_err(|err| err.to_string())?;
        Ok(Connection::new(state))
    }

    fn open_with_config(
        path: Option<String>,
        options: Vec<(String, String)>,
    ) -> Result<Connection, String> {
        let state = ConnectionState::open_with_config(path.as_deref(), &options)
            .map_err(|err| err.to_string())?;
        Ok(Connection::new(state))
    }

    fn close(conn: Connection) {
        // Dropping the inner state releases the DuckDB resources.
        conn.into_inner::<ConnectionState>();
    }

    fn interrupt(conn: ConnectionBorrow<'_>) {
        conn.get::<ConnectionState>().interrupt();
    }

    fn execute(conn: ConnectionBorrow<'_>, sql: String) -> Result<QueryResult, Duckerror> {
        conn.get::<ConnectionState>()
            .execute(&sql)
            .map_err(Duckerror::from)
    }

    fn open_stream(
        conn: ConnectionBorrow<'_>,
        sql: String,
    ) -> Result<exported_database::ResultStream, Duckerror> {
        let (columns, rows) = conn
            .get::<ConnectionState>()
            .collect_rows(&sql)
            .map_err(Duckerror::from)?;
        Ok(exported_database::ResultStream::new(QueryStream::new(
            columns, rows,
        )))
    }

    fn prepare(
        conn: ConnectionBorrow<'_>,
        sql: String,
    ) -> Result<exported_database::PreparedStatement, Duckerror> {
        let state = PreparedStatementState::prepare(conn.get::<ConnectionState>(), &sql)
            .map_err(Duckerror::from)?;
        Ok(exported_database::PreparedStatement::new(state))
    }

    fn query_arrow(conn: ConnectionBorrow<'_>, sql: String) -> Result<Vec<u8>, Duckerror> {
        conn.get::<ConnectionState>()
            .query_arrow_ipc(&sql)
            .map_err(Duckerror::from)
    }

    fn create_appender(
        conn: ConnectionBorrow<'_>,
        schema: Option<String>,
        table: String,
    ) -> Result<exported_database::Appender, Duckerror> {
        let state = AppenderState::create(conn.get::<ConnectionState>(), schema.as_deref(), &table)
            .map_err(Duckerror::from)?;
        Ok(exported_database::Appender::new(state))
    }

    fn register_extension(name: String, requires: Vec<Capabilitykind>) -> Result<bool, String> {
        record_extension_registration(&name, &requires)
            .map_err(|err| format!("failed to register extension {name}: {err}"))?;
        Ok(true)
    }

    fn list_registered_extensions() -> Vec<ExtensionInfo> {
        extension_loader::list_registered_extensions()
            .into_iter()
            .map(|entry| ExtensionInfo {
                name: entry.name,
                requires: entry.requires,
            })
            .collect()
    }

    // Bridge one DuckDB UI HTTP request to the statically-linked `ui` extension's
    // HttpServer (the native host owns the socket; httplib can't listen() here).
    // Calls the extension's C entry, which serializes the response as
    // [u32 status][u32 ctype_len][ctype][u32 body_len][body].
    fn handle_ui_request(
        method: String,
        path: String,
        headers: String,
        body: Vec<u8>,
    ) -> Option<exported_database::UiResponse> {
        extern "C" {
            fn duckdb_ui_handle_request(
                method: *const c_char,
                path: *const c_char,
                headers: *const c_char,
                body: *const u8,
                body_len: usize,
                out_len: *mut usize,
            ) -> *mut u8;
            fn duckdb_ui_free(buf: *mut u8);
        }

        let c_method = std::ffi::CString::new(method).ok()?;
        let c_path = std::ffi::CString::new(path).ok()?;
        let c_headers = std::ffi::CString::new(headers).ok()?;
        let mut out_len: usize = 0;
        let buf = unsafe {
            duckdb_ui_handle_request(
                c_method.as_ptr(),
                c_path.as_ptr(),
                c_headers.as_ptr(),
                body.as_ptr(),
                body.len(),
                &mut out_len,
            )
        };
        if buf.is_null() || out_len < 12 {
            if !buf.is_null() {
                unsafe { duckdb_ui_free(buf) };
            }
            return None;
        }

        let slice = unsafe { std::slice::from_raw_parts(buf, out_len) };
        let read_u32 = |off: usize| -> usize {
            u32::from_ne_bytes([slice[off], slice[off + 1], slice[off + 2], slice[off + 3]]) as usize
        };
        let status = read_u32(0) as u16;
        let headers_len = read_u32(4);
        let result = (|| {
            let headers_end = 8usize.checked_add(headers_len)?;
            if headers_end + 4 > out_len {
                return None;
            }
            let headers = String::from_utf8_lossy(&slice[8..headers_end]).into_owned();
            let body_len = read_u32(headers_end);
            let body_start = headers_end + 4;
            if body_start.checked_add(body_len)? > out_len {
                return None;
            }
            Some(exported_database::UiResponse {
                status,
                headers,
                body: slice[body_start..body_start + body_len].to_vec(),
            })
        })();
        unsafe { duckdb_ui_free(buf) };
        result
    }
}

#[no_mangle]
pub extern "C" fn duckdb_component_load_extension(name: *const c_char) -> bool {
    if name.is_null() {
        return false;
    }
    let extension_name = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .to_string();
    clog!(
        "[duckdb-core] requesting host load for '{}'",
        extension_name
    );
    let handled = bindings::duckdb::component::host_extension_loader::request_load(&extension_name);
    if !handled {
        clog!(
            "[duckdb-core] host declined extension '{}'; falling back to native path",
            extension_name
        );
        return false;
    }
    clog!(
        "[duckdb-core] host reported '{}' ready; fetching pending registrations",
        extension_name
    );
    let pending = extension_loader_hooks::get_pending_registrations();
    clog!(
        "[duckdb-core] '{}' pending registrations: scalars={}, tables={}, aggregates={}",
        extension_name,
        pending.scalars.len(),
        pending.tables.len(),
        pending.aggregates.len()
    );
    match process_pending_registrations(&extension_name, pending) {
        Ok(()) => true,
        Err(err) => {
            clog!("failed to register functions for extension {extension_name}: {err}");
            false
        }
    }
}

bindings::exports::duckdb::component::database::__export_duckdb_component_database_cabi!(
    Component with_types_in bindings::exports::duckdb::component::database
);

fn runtime_unavailable_error() -> Duckerror {
    Duckerror::Unsupported("extension runtime not available".to_string())
}

fn process_pending_registrations(
    extension: &str,
    pending: extension_loader_hooks::PendingRegistrations,
) -> Result<(), Duckerror> {
    if pending.scalars.is_empty()
        && pending.tables.is_empty()
        && pending.aggregates.is_empty()
        && pending.macros.is_empty()
    {
        clog!("[duckdb-core] no registrations returned for '{extension}'");
    }
    for entry in pending.scalars.into_iter().collect::<Vec<_>>() {
        register_pending_scalar(entry)?;
    }
    for entry in pending.tables.into_iter().collect::<Vec<_>>() {
        register_pending_table(entry)?;
    }
    for entry in pending.aggregates.into_iter().collect::<Vec<_>>() {
        register_pending_aggregate(entry)?;
    }
    for entry in pending.macros.into_iter().collect::<Vec<_>>() {
        // A macro failure must not fail the whole extension load (which would
        // make the loader hook return false and DuckDB report the unrelated
        // "extension loading disabled" error). Log and continue.
        if let Err(err) = register_pending_macro(entry) {
            clog!("[duckdb-core] macro registration failed (continuing): {err:?}");
        }
    }
    for entry in pending.replacement_scans.into_iter().collect::<Vec<_>>() {
        if let Err(err) = register_pending_replacement_scan(entry) {
            clog!("[duckdb-core] replacement-scan registration failed (continuing): {err:?}");
        }
    }
    for entry in pending.logical_types.into_iter().collect::<Vec<_>>() {
        if let Err(err) = register_pending_logical_type(entry) {
            clog!("[duckdb-core] logical-type registration failed (continuing): {err:?}");
        }
    }
    for entry in pending.casts.into_iter().collect::<Vec<_>>() {
        if let Err(err) = register_pending_cast(entry) {
            clog!("[duckdb-core] cast registration failed (continuing): {err:?}");
        }
    }
    Ok(())
}

struct CastFunctionEntry {
    callback_handle: u32,
    source: Logicaltype,
    target: Logicaltype,
}

fn duckdb_type_to_logical(type_id: duckdb::duckdb_type) -> Option<Logicaltype> {
    match type_id {
        duckdb::DUCKDB_TYPE_BOOLEAN => Some(Logicaltype::Boolean),
        duckdb::DUCKDB_TYPE_BIGINT => Some(Logicaltype::Int64),
        duckdb::DUCKDB_TYPE_UBIGINT => Some(Logicaltype::Uint64),
        duckdb::DUCKDB_TYPE_DOUBLE => Some(Logicaltype::Float64),
        duckdb::DUCKDB_TYPE_VARCHAR => Some(Logicaltype::Text),
        duckdb::DUCKDB_TYPE_BLOB => Some(Logicaltype::Blob),
        duckdb::DUCKDB_TYPE_INTEGER => Some(Logicaltype::Int32),
        duckdb::DUCKDB_TYPE_TIMESTAMP => Some(Logicaltype::Timestamp),
        duckdb::DUCKDB_TYPE_TINYINT => Some(Logicaltype::Int8),
        duckdb::DUCKDB_TYPE_SMALLINT => Some(Logicaltype::Int16),
        duckdb::DUCKDB_TYPE_UTINYINT => Some(Logicaltype::Uint8),
        duckdb::DUCKDB_TYPE_USMALLINT => Some(Logicaltype::Uint16),
        duckdb::DUCKDB_TYPE_UINTEGER => Some(Logicaltype::Uint32),
        duckdb::DUCKDB_TYPE_FLOAT => Some(Logicaltype::Float32),
        duckdb::DUCKDB_TYPE_DATE => Some(Logicaltype::Date),
        duckdb::DUCKDB_TYPE_TIME => Some(Logicaltype::Time),
        duckdb::DUCKDB_TYPE_TIMESTAMP_TZ => Some(Logicaltype::Timestamptz),
        duckdb::DUCKDB_TYPE_DECIMAL => Some(Logicaltype::Decimal),
        duckdb::DUCKDB_TYPE_INTERVAL => Some(Logicaltype::Interval),
        duckdb::DUCKDB_TYPE_UUID => Some(Logicaltype::Uuid),
        // The builtin GEOMETRY type is physically a WKB string_t blob, identical
        // to BLOB, so it flows through component casts/scalars as a blob value.
        duckdb::DUCKDB_TYPE_GEOMETRY => Some(Logicaltype::Blob),
        _ => None,
    }
}

/// Resolves a SQL type name (base or custom) to its logical type and physical
/// enum by asking DuckDB the type of `CAST(NULL AS <name>)`.
unsafe fn resolve_logical_type(
    conn: duckdb::duckdb_connection,
    type_name: &str,
) -> Result<(duckdb::duckdb_logical_type, Logicaltype), String> {
    let sql = format!("SELECT CAST(NULL AS {type_name}) AS x");
    let c_sql = CString::new(sql).map_err(|_| "type name contained NUL".to_string())?;
    let mut result = std::mem::MaybeUninit::<duckdb::duckdb_result>::zeroed();
    let state = duckdb::duckdb_query(conn, c_sql.as_ptr(), result.as_mut_ptr());
    let mut result = result.assume_init();
    if state != DUCKDB_SUCCESS {
        let msg =
            extract_result_error(&result).unwrap_or_else(|| "type resolution failed".to_string());
        duckdb::duckdb_destroy_result(&mut result);
        return Err(msg);
    }
    let logical = duckdb::duckdb_column_logical_type(&mut result, 0);
    duckdb::duckdb_destroy_result(&mut result);
    if logical.is_null() {
        return Err(format!("could not resolve type '{type_name}'"));
    }
    let type_id = duckdb::duckdb_get_type_id(logical);
    match duckdb_type_to_logical(type_id) {
        Some(enum_ty) => Ok((logical, enum_ty)),
        None => {
            let mut logical_mut = logical;
            duckdb::duckdb_destroy_logical_type(&mut logical_mut);
            Err(format!(
                "unsupported physical type id {type_id} for '{type_name}'"
            ))
        }
    }
}

/// Resolve a DuckDB type-expression string (e.g. "INTEGER[]" or
/// "STRUCT(a INTEGER, b VARCHAR)") to its real `duckdb_logical_type`, WITHOUT
/// requiring the result to map to a scalar `Logicaltype` enum. Used by the
/// escape-hatch `complex` type, whose nested types (LIST/STRUCT) have no scalar
/// enum. Mirrors `resolve_logical_type` but returns only the logical type.
unsafe fn resolve_logical_type_only(
    conn: duckdb::duckdb_connection,
    type_name: &str,
) -> Result<duckdb::duckdb_logical_type, String> {
    let sql = format!("SELECT CAST(NULL AS {type_name}) AS x");
    let c_sql = CString::new(sql).map_err(|_| "type name contained NUL".to_string())?;
    let mut result = std::mem::MaybeUninit::<duckdb::duckdb_result>::zeroed();
    let state = duckdb::duckdb_query(conn, c_sql.as_ptr(), result.as_mut_ptr());
    let mut result = result.assume_init();
    if state != DUCKDB_SUCCESS {
        let msg =
            extract_result_error(&result).unwrap_or_else(|| "type resolution failed".to_string());
        duckdb::duckdb_destroy_result(&mut result);
        return Err(msg);
    }
    let logical = duckdb::duckdb_column_logical_type(&mut result, 0);
    duckdb::duckdb_destroy_result(&mut result);
    if logical.is_null() {
        return Err(format!("could not resolve type '{type_name}'"));
    }
    Ok(logical)
}

fn register_pending_cast(entry: extension_loader_hooks::CastRegistration) -> Result<(), Duckerror> {
    let extension_loader_hooks::CastRegistration {
        source,
        target,
        callback_handle,
    } = entry;
    clog!("[duckdb-core] registering cast {source} -> {target} (callback={callback_handle})");
    for database in distinct_active_databases() {
        unsafe { register_cast_on_database(database, &source, &target, callback_handle) }.map_err(
            |err| Duckerror::Internal(format!("failed to register cast {source}->{target}: {err}")),
        )?;
    }
    Ok(())
}

unsafe fn register_cast_on_database(
    database: duckdb::duckdb_database,
    source: &str,
    target: &str,
    callback_handle: u32,
) -> Result<(), String> {
    let mut conn: duckdb::duckdb_connection = ptr::null_mut();
    if duckdb::duckdb_connect(database, &mut conn) != DUCKDB_SUCCESS {
        return Err("duckdb_connect failed for cast registration".to_string());
    }
    let outcome = (|| {
        let (mut source_lt, source_enum) = resolve_logical_type(conn, source)?;
        let (mut target_lt, target_enum) = resolve_logical_type(conn, target)?;
        let cast = duckdb::duckdb_create_cast_function();
        duckdb::duckdb_cast_function_set_source_type(cast, source_lt);
        duckdb::duckdb_cast_function_set_target_type(cast, target_lt);
        duckdb::duckdb_cast_function_set_function(cast, Some(cast_function_callback));
        let entry = Box::new(CastFunctionEntry {
            callback_handle,
            source: source_enum,
            target: target_enum,
        });
        duckdb::duckdb_cast_function_set_extra_info(
            cast,
            Box::into_raw(entry) as *mut c_void,
            Some(cast_entry_destroy),
        );
        let state = duckdb::duckdb_register_cast_function(conn, cast);
        let mut cast_mut = cast;
        duckdb::duckdb_destroy_cast_function(&mut cast_mut);
        duckdb::duckdb_destroy_logical_type(&mut source_lt);
        duckdb::duckdb_destroy_logical_type(&mut target_lt);
        if state != DUCKDB_SUCCESS {
            return Err("duckdb_register_cast_function failed".to_string());
        }
        Ok(())
    })();
    duckdb::duckdb_disconnect(&mut conn);
    outcome
}

unsafe extern "C" fn cast_entry_destroy(ptr: *mut c_void) {
    if !ptr.is_null() {
        let _ = Box::from_raw(ptr as *mut CastFunctionEntry);
    }
}

unsafe extern "C" fn cast_function_callback(
    info: duckdb::duckdb_function_info,
    count: duckdb::idx_t,
    input: duckdb::duckdb_vector,
    output: duckdb::duckdb_vector,
) -> bool {
    match execute_cast(info, count, input, output) {
        Ok(()) => true,
        Err(err) => {
            if let Ok(message) = sanitize_error_message(&format_duckerror(&err)) {
                duckdb::duckdb_function_set_error(info, message.as_ptr());
            }
            false
        }
    }
}

unsafe fn execute_cast(
    info: duckdb::duckdb_function_info,
    count: duckdb::idx_t,
    input: duckdb::duckdb_vector,
    output: duckdb::duckdb_vector,
) -> Result<(), Duckerror> {
    let entry_ptr = duckdb::duckdb_cast_function_get_extra_info(info);
    if entry_ptr.is_null() {
        return Err(Duckerror::Internal("cast missing dispatcher entry".to_string()));
    }
    let entry = &*(entry_ptr as *const CastFunctionEntry);
    let column = ScalarInputColumn {
        vector: input,
        logical: entry.source.clone(),
    };
    for row in 0..count {
        let value = read_scalar_argument(&column, row)?;
        let result = callback_dispatch::call_cast(entry.callback_handle, &value)?;
        write_duckvalue_to_vector(output, &entry.target, row, result)?;
    }
    Ok(())
}

fn register_pending_logical_type(
    entry: extension_loader_hooks::LogicalTypeRegistration,
) -> Result<(), Duckerror> {
    let extension_loader_hooks::LogicalTypeRegistration { name, physical } = entry;
    // DuckDB has no C API to register a named type, so create it as a SQL type
    // alias on a transient connection (catalog-level, like macros). `physical`
    // is a SQL type expression (e.g. "INTEGER"); the name is a quoted ident.
    let sql = format!("CREATE TYPE {} AS {physical}", quote_ident(&name));
    clog!("[duckdb-core] registering logical type '{name}' via: {sql}");
    for database in distinct_active_databases() {
        unsafe { execute_on_transient_connection(database, &sql) }.map_err(|err| {
            Duckerror::Internal(format!("failed to register logical type '{name}': {err}"))
        })?;
    }
    Ok(())
}

fn register_pending_scalar(
    entry: extension_loader_hooks::ScalarRegistration,
) -> Result<(), Duckerror> {
    let arg_summary = summarize_loader_funcargs(&entry.arguments);
    let return_ty = describe_loader_logicaltype(&entry.returns);
    let option_summary = summarize_loader_funcopts(entry.options.as_ref());
    clog!(
        "[duckdb-core] registering scalar '{}' (callback={}, args={}, returns={}, opts={})",
        entry.name, entry.callback_handle, arg_summary, return_ty, option_summary
    );
    let extension_loader_hooks::ScalarRegistration {
        name,
        arguments,
        returns,
        callback_handle,
        options,
    } = entry;
    let id = NEXT_SCALAR_FUNCTION_ID.fetch_add(1, Ordering::Relaxed);
    let definition = Arc::new(ScalarFunctionDefinition {
        id,
        name,
        arguments: arguments
            .into_iter()
            .map(|arg| convert_loader_logicaltype(arg.logical))
            .collect(),
        returns: convert_loader_logicaltype(returns),
        callback_handle,
        embedded: None,
        options: options.map(convert_loader_funcopts),
    });

    push_scalar_function_definition(definition.clone());

    if let Err(err) = register_scalar_function_with_existing_connections(&definition) {
        remove_scalar_function_definition(id);
        Err(err)
    } else {
        Ok(())
    }
}

fn register_pending_table(
    entry: extension_loader_hooks::TableRegistration,
) -> Result<(), Duckerror> {
    let arg_summary = summarize_loader_funcargs(&entry.arguments);
    let column_summary = summarize_loader_columns(&entry.columns);
    let option_summary = summarize_loader_extopts(entry.options.as_ref());
    clog!(
        "[duckdb-core] registering table '{}' (callback={}, args={}, columns={}, opts={})",
        entry.name, entry.callback_handle, arg_summary, column_summary, option_summary
    );
    let extension_loader_hooks::TableRegistration {
        name,
        arguments,
        columns,
        callback_handle,
        options,
    } = entry;
    let id = NEXT_TABLE_FUNCTION_ID.fetch_add(1, Ordering::Relaxed);
    let definition = Arc::new(TableFunctionDefinition {
        id,
        name,
        arguments: arguments
            .into_iter()
            .map(|arg| TableArgument {
                name: arg.name,
                logical: convert_loader_logicaltype(arg.logical),
            })
            .collect(),
        columns: columns
            .into_iter()
            .map(|col| Columndef {
                name: col.name,
                logical: convert_loader_logicaltype(col.logical),
            })
            .collect(),
        callback_handle,
        options: options.map(convert_loader_extopts),
    });

    push_table_function_definition(definition.clone());

    if let Err(err) = register_table_function_with_existing_connections(&definition) {
        let mut defs = table_function_definitions()
            .lock()
            .expect("table function registry mutex poisoned");
        defs.retain(|entry| entry.id != id);
        Err(err)
    } else {
        Ok(())
    }
}

fn register_pending_aggregate(
    entry: extension_loader_hooks::AggregateRegistration,
) -> Result<(), Duckerror> {
    let arg_summary = summarize_loader_funcargs(&entry.arguments);
    let return_ty = describe_loader_logicaltype(&entry.returns);
    let option_summary = summarize_loader_funcopts(entry.options.as_ref());
    clog!(
        "[duckdb-core] registering aggregate '{}' (callback={}, args={}, returns={}, opts={})",
        entry.name, entry.callback_handle, arg_summary, return_ty, option_summary
    );
    let extension_loader_hooks::AggregateRegistration {
        name,
        arguments,
        returns,
        callback_handle,
        options,
    } = entry;
    let id = NEXT_AGGREGATE_FUNCTION_ID.fetch_add(1, Ordering::Relaxed);
    let definition = Arc::new(AggregateFunctionDefinition {
        id,
        name,
        arguments: arguments
            .into_iter()
            .map(|arg| convert_loader_logicaltype(arg.logical))
            .collect(),
        returns: convert_loader_logicaltype(returns),
        callback_handle,
        options: options.map(convert_loader_funcopts),
    });

    push_aggregate_function_definition(definition.clone());

    if let Err(err) = register_aggregate_function_with_existing_connections(&definition) {
        let mut defs = aggregate_function_definitions()
            .lock()
            .expect("aggregate function registry mutex poisoned");
        defs.retain(|entry| entry.id != id);
        Err(err)
    } else {
        Ok(())
    }
}

fn register_pending_macro(
    entry: extension_loader_hooks::MacroRegistration,
) -> Result<(), Duckerror> {
    let extension_loader_hooks::MacroRegistration {
        schema,
        name,
        parameters,
        definition_sql,
    } = entry;
    let parameters: Vec<String> = parameters.into_iter().collect();
    let sql = build_create_macro_sql(&schema, &name, &parameters, &definition_sql);
    // The extension's `catalog.register-macro` call was captured by the host,
    // forwarded through `extension-loader-hooks`, and turned into the exact
    // `CREATE MACRO` SQL below. This works because the libduckdb archive is built
    // with wasm exception handling (wasi-sdk-33 `eh` multilib + `-fwasm-exceptions`);
    // DuckDB's macro binder throws during overload resolution, which now unwinds
    // and is caught instead of aborting the module.
    create_macro_on_active_databases(&name, &sql)
}

/// Runs `CREATE MACRO` on a transient connection to each active database, never
/// the connection executing LOAD (which is a busy ClientContext). Macros live
/// in the catalog, so they become visible to all connections of that database.
fn create_macro_on_active_databases(name: &str, sql: &str) -> Result<(), Duckerror> {
    let databases: Vec<duckdb::duckdb_database> = {
        let guard = active_connections()
            .lock()
            .expect("active connections mutex poisoned");
        let mut seen: Vec<duckdb::duckdb_database> = Vec::new();
        for conn in guard.iter() {
            if !conn.1.is_null() && !seen.iter().any(|db| *db == conn.1) {
                seen.push(conn.1);
            }
        }
        seen
    };

    if databases.is_empty() {
        return Err(Duckerror::Invalidstate(format!(
            "no active database available to register macro '{name}'"
        )));
    }

    for database in databases {
        unsafe { execute_on_transient_connection(database, sql) }.map_err(|err| {
            Duckerror::Internal(format!("failed to register macro '{name}': {err}"))
        })?;
    }
    Ok(())
}

fn build_create_macro_sql(
    schema: &str,
    name: &str,
    parameters: &[String],
    definition_sql: &str,
) -> String {
    let mut sql = String::from("CREATE OR REPLACE MACRO ");
    if !schema.is_empty() {
        sql.push_str(&quote_ident(schema));
        sql.push('.');
    }
    sql.push_str(&quote_ident(name));
    sql.push('(');
    for (idx, param) in parameters.iter().enumerate() {
        if idx > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&quote_ident(param));
    }
    sql.push_str(") AS (");
    sql.push_str(definition_sql);
    sql.push(')');
    sql
}

fn quote_ident(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('"');
    for ch in ident.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

fn replacement_scans() -> &'static Mutex<Vec<ReplacementScanSpec>> {
    REPLACEMENT_SCANS.get_or_init(|| Mutex::new(Vec::new()))
}

fn replacement_scan_databases() -> &'static Mutex<Vec<DatabaseHandle>> {
    REPLACEMENT_SCAN_DATABASES.get_or_init(|| Mutex::new(Vec::new()))
}

fn register_pending_replacement_scan(
    entry: extension_loader_hooks::ReplacementScanRegistration,
) -> Result<(), Duckerror> {
    let extensions: Vec<String> = entry.extensions.into_iter().collect();
    clog!(
        "[duckdb-core] registering replacement scan {:?} -> '{}'",
        extensions,
        entry.function_name
    );
    {
        let mut guard = replacement_scans()
            .lock()
            .expect("replacement scan registry poisoned");
        guard.push(ReplacementScanSpec {
            extensions,
            function_name: entry.function_name,
        });
    }
    // Install one global replacement-scan callback per database (idempotent).
    for database in distinct_active_databases() {
        unsafe { ensure_replacement_scan_callback(database) };
    }
    Ok(())
}

unsafe fn ensure_replacement_scan_callback(database: duckdb::duckdb_database) {
    let mut installed = replacement_scan_databases()
        .lock()
        .expect("replacement scan databases poisoned");
    if installed.iter().any(|d| d.0 == database) {
        return;
    }
    duckdb::duckdb_add_replacement_scan(
        database,
        Some(replacement_scan_callback),
        ptr::null_mut(),
        None,
    );
    installed.push(DatabaseHandle(database));
}

/// Called by DuckDB when a query references an unknown table. If the name
/// matches a registered file extension, rewrite it to the registered table
/// function, passing the original name as the function's argument.
unsafe extern "C" fn replacement_scan_callback(
    info: duckdb::duckdb_replacement_scan_info,
    table_name: *const c_char,
    _data: *mut c_void,
) {
    if table_name.is_null() {
        return;
    }
    let name = match CStr::from_ptr(table_name).to_str() {
        Ok(name) => name,
        Err(_) => return,
    };
    let function_name = {
        let guard = match replacement_scans().lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        guard.iter().find_map(|spec| {
            let matches = spec
                .extensions
                .iter()
                .any(|ext| name == ext || name.ends_with(&format!(".{ext}")));
            matches.then(|| spec.function_name.clone())
        })
    };
    let function_name = match function_name {
        Some(name) => name,
        None => return,
    };
    let func_c = match CString::new(function_name) {
        Ok(c) => c,
        Err(_) => return,
    };
    duckdb::duckdb_replacement_scan_set_function_name(info, func_c.as_ptr());
    let mut value = duckdb::duckdb_create_varchar(table_name);
    duckdb::duckdb_replacement_scan_add_parameter(info, value);
    duckdb::duckdb_destroy_value(&mut value);
}

unsafe fn execute_on_transient_connection(
    database: duckdb::duckdb_database,
    sql: &str,
) -> Result<(), String> {
    let c_sql = CString::new(sql).map_err(|_| "macro SQL contained interior NUL".to_string())?;
    let mut conn: duckdb::duckdb_connection = ptr::null_mut();
    if duckdb::duckdb_connect(database, &mut conn) != DUCKDB_SUCCESS {
        return Err("duckdb_connect failed for macro registration".to_string());
    }
    let mut result = std::mem::MaybeUninit::<duckdb::duckdb_result>::zeroed();
    let state = duckdb::duckdb_query(conn, c_sql.as_ptr(), result.as_mut_ptr());
    let mut result = result.assume_init();
    let outcome = if state != DUCKDB_SUCCESS {
        Err(extract_result_error(&result).unwrap_or_else(|| "duckdb_query failed".to_string()))
    } else {
        Ok(())
    };
    duckdb::duckdb_destroy_result(&mut result);
    duckdb::duckdb_disconnect(&mut conn);
    outcome
}

struct ConfigHost;

impl config_exports::Guest for ConfigHost {
    fn provider_version() -> String {
        "ducklink-core".to_string()
    }

    fn list_keys(_prefix: Option<String>) -> Vec<String> {
        Vec::new()
    }

    fn get_string(_path: String) -> Result<Option<String>, Configerror> {
        Ok(None)
    }

    fn get_bool(_path: String) -> Result<Option<bool>, Configerror> {
        Ok(None)
    }

    fn get_i64(_path: String) -> Result<Option<i64>, Configerror> {
        Ok(None)
    }

    fn get_u64(_path: String) -> Result<Option<u64>, Configerror> {
        Ok(None)
    }

    fn get_f64(_path: String) -> Result<Option<f64>, Configerror> {
        Ok(None)
    }

    fn get_bytes(_path: String) -> Result<Option<Vec<u8>>, Configerror> {
        Ok(None)
    }

    fn get_string_list(_path: String) -> Result<Option<Vec<String>>, Configerror> {
        Ok(None)
    }
}

config_exports::__export_duckdb_extension_config_2_0_0_cabi!(
    ConfigHost with_types_in bindings::exports::duckdb::extension::config
);

struct LoggingHost;

impl logging_exports::Guest for LoggingHost {
    fn log(_level: Loglevel, _message: String, _target: Option<String>) {}

    fn log_fields(_level: Loglevel, _message: String, _fields: Vec<Logfield>) {}
}

logging_exports::__export_duckdb_extension_logging_2_0_0_cabi!(
    LoggingHost with_types_in bindings::exports::duckdb::extension::logging
);

struct NoopScalarCallback;
struct NoopTableCallback;
struct NoopAggregateCallback;
struct NoopPragmaCallback;
struct NoopCastCallback;

#[derive(Default)]
struct ScalarRegistry;

#[derive(Default)]
struct ComponentTableRegistry;
#[derive(Default)]
struct ComponentAggregateRegistry;
#[derive(Default)]
struct ComponentPragmaRegistry;
struct NoopMacroRegistry;

/// An embedded scalar runs in-core with no WIT boundary (vs `callback_handle`,
/// which dispatches to a loaded extension component). The function receives one
/// row's arguments and returns its result, called directly per row.
type EmbeddedScalarFn = fn(&[Duckvalue]) -> Result<Duckvalue, Duckerror>;

#[derive(Debug)]
struct ScalarFunctionDefinition {
    id: u32,
    name: String,
    arguments: Vec<Logicaltype>,
    returns: Logicaltype,
    callback_handle: u32,
    /// Set for extensions compiled into the core (the embed framework); when
    /// `Some`, execute_scalar_function calls it directly instead of crossing the
    /// WIT boundary via `callback_handle`.
    embedded: Option<EmbeddedScalarFn>,
    options: Option<runtime_exports::Funcopts>,
}

#[derive(Clone)]
struct ScalarFunctionEntry {
    definition: Arc<ScalarFunctionDefinition>,
}

// ---- Embedded extensions (the embed framework) ----
// Extensions selected at build time (embed-<name> cargo features) are compiled
// into the core and registered as native scalars with no WIT boundary, so they
// run at native speed and are available even in the standalone (no host needed).
// The algorithm lives in a WIT-free crate (e.g. `isin`) shared with the
// component build; only the Duckvalue glue is here.

#[cfg(feature = "embed-isin")]
fn register_embedded_scalar(
    name: &str,
    arguments: Vec<Logicaltype>,
    returns: Logicaltype,
    embedded: EmbeddedScalarFn,
) {
    let id = NEXT_SCALAR_FUNCTION_ID.fetch_add(1, Ordering::Relaxed);
    push_scalar_function_definition(Arc::new(ScalarFunctionDefinition {
        id,
        name: name.to_string(),
        arguments,
        returns,
        callback_handle: 0,
        embedded: Some(embedded),
        options: None,
    }));
}

#[cfg(feature = "embed-isin")]
fn embed_arg_text(args: &[Duckvalue], i: usize, fname: &str) -> Result<String, Duckerror> {
    match args.get(i) {
        Some(Duckvalue::Text(s)) => Ok(s.clone()),
        Some(Duckvalue::Null) => Ok(String::new()),
        _ => Err(Duckerror::Invalidargument(format!(
            "{fname}: expected VARCHAR argument at position {i}"
        ))),
    }
}

/// Push the definitions for every compiled-in extension once; they are then
/// replayed onto each connection by register_connection_handle. Idempotent.
fn register_embedded_extensions() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    #[cfg(feature = "embed-isin")]
    {
        register_embedded_scalar(
            "isin_validate",
            vec![Logicaltype::Text],
            Logicaltype::Boolean,
            |args| {
                Ok(Duckvalue::Boolean(isin::validate(&embed_arg_text(
                    args,
                    0,
                    "isin_validate",
                )?)))
            },
        );
        register_embedded_scalar(
            "isin_check_digit",
            vec![Logicaltype::Text],
            Logicaltype::Int64,
            |args| match isin::check_digit(&embed_arg_text(args, 0, "isin_check_digit")?) {
                Some(d) => Ok(Duckvalue::Int64(d)),
                None => Ok(Duckvalue::Null),
            },
        );
    }
}

struct ScalarInputColumn {
    vector: duckdb::duckdb_vector,
    logical: Logicaltype,
}

#[derive(Debug)]
struct TableArgument {
    name: Option<String>,
    logical: Logicaltype,
}

#[derive(Debug)]
struct TableFunctionDefinition {
    id: u32,
    name: String,
    arguments: Vec<TableArgument>,
    columns: Vec<Columndef>,
    callback_handle: u32,
    options: Option<runtime_exports::Extopts>,
}

#[derive(Debug)]
struct TableFunctionEntry {
    definition: Arc<TableFunctionDefinition>,
}

struct TableFunctionBindData {
    definition: Arc<TableFunctionDefinition>,
    arguments: Vec<Duckvalue>,
}

struct TableFunctionState {
    definition: Arc<TableFunctionDefinition>,
    rows: Vec<Vec<Duckvalue>>,
    offset: usize,
}

#[derive(Debug)]
struct AggregateFunctionDefinition {
    id: u32,
    name: String,
    arguments: Vec<Logicaltype>,
    returns: Logicaltype,
    callback_handle: u32,
    options: Option<runtime_exports::Funcopts>,
}

#[derive(Debug)]
struct AggregateFunctionEntry {
    definition: Arc<AggregateFunctionDefinition>,
}

struct AggregateState {
    rows: Vec<Vec<Duckvalue>>,
}

// ---- Item 4: component-declared pragmas (PRAGMA -> generated SQL) ----

fn declared_pragmas() -> &'static Mutex<std::collections::HashMap<String, u32>> {
    DECLARED_PRAGMAS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Pull the pragmas components have declared (via the `pragma-host.pragma-list`
/// import) into the process-wide registry. Idempotent: names are keyed
/// lower-case so interception is case-insensitive. Mirrors `sync_collations`.
fn sync_pragmas() {
    let specs = pragma_host::pragma_list();
    if specs.is_empty() {
        return;
    }
    let mut guard = match declared_pragmas().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    for spec in specs {
        guard.insert(spec.name.to_ascii_lowercase(), spec.callback_handle);
    }
}

/// A parsed `PRAGMA <name>(<arg>, ...)` call.
struct ParsedPragma {
    name: String,
    args: Vec<String>,
}

/// Parse `PRAGMA <name>(<arg>, ...)` (also accepting `CALL <name>(...)`),
/// returning the pragma name and its arguments as strings. Single/double-quoted
/// args are unquoted; bare identifiers/numbers are taken verbatim. Returns None
/// if `sql` is not a pragma/call form. Only a single statement is considered.
fn parse_pragma_call(sql: &str) -> Option<ParsedPragma> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let lower = trimmed.to_ascii_lowercase();
    let rest = if let Some(r) = lower.strip_prefix("pragma") {
        if !r.starts_with(|c: char| c.is_whitespace()) {
            return None;
        }
        &trimmed[6..]
    } else if let Some(r) = lower.strip_prefix("call") {
        if !r.starts_with(|c: char| c.is_whitespace()) {
            return None;
        }
        &trimmed[4..]
    } else {
        return None;
    };
    let rest = rest.trim_start();
    let open = rest.find('(')?;
    let name = rest[..open].trim().to_string();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let close = rest.rfind(')')?;
    if close < open {
        return None;
    }
    let inner = &rest[open + 1..close];
    let args = split_pragma_args(inner);
    Some(ParsedPragma { name, args })
}

/// Split a pragma argument list on top-level commas, honoring single/double
/// quotes (with doubled-quote escapes), and unquote each argument.
fn split_pragma_args(inner: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut buf = String::new();
    let mut chars = inner.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    if chars.peek() == Some(&q) {
                        buf.push(q);
                        chars.next();
                    } else {
                        quote = None;
                    }
                } else {
                    buf.push(c);
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                ',' => {
                    args.push(buf.trim().to_string());
                    buf.clear();
                }
                _ => buf.push(c),
            },
        }
    }
    let last = buf.trim().to_string();
    if !last.is_empty() || !args.is_empty() {
        args.push(last);
    }
    args
}

/// Split a SQL script into statements on top-level semicolons, honoring
/// single/double-quoted strings (with doubled-quote escapes). Good enough for
/// the generated FTS index DDL (no semicolons inside string literals there, but
/// the quote handling keeps it robust).
/// True if `stmt` (one SQL statement) is a `LOAD <name>` -- the trigger for
/// running a multi-statement batch one statement at a time so a later
/// `ATTACH ... (TYPE <name>)` sees the storage type the LOAD just registered.
/// Tolerant of leading whitespace/comments-stripped input; matches the first
/// keyword case-insensitively.
fn statement_is_load(stmt: &str) -> bool {
    stmt.trim_start()
        .split(|c: char| c.is_whitespace())
        .next()
        .map(|kw| kw.eq_ignore_ascii_case("load"))
        .unwrap_or(false)
}

/// Reject component-supplied `complex` JSON whose recursion depth would overflow
/// the wasm stack when walked by `write_json_to_vector`. Pure + connection-free
/// so it is unit-testable; the unsafe writer enforces the same cap inline (the
/// `depth >` check), this mirrors it for the up-front validation + the tests.
fn complex_depth_within_cap(value: &serde_json::Value, max_depth: u32) -> bool {
    fn depth_ok(value: &serde_json::Value, remaining: u32) -> bool {
        match value {
            serde_json::Value::Array(items) => {
                if remaining == 0 {
                    return false;
                }
                items.iter().all(|v| depth_ok(v, remaining - 1))
            }
            serde_json::Value::Object(fields) => {
                if remaining == 0 {
                    return false;
                }
                fields.values().all(|v| depth_ok(v, remaining - 1))
            }
            _ => true,
        }
    }
    depth_ok(value, max_depth)
}

/// True when the JSON value's shape is compatible with the requested DuckDB
/// physical kind (LIST<->array, STRUCT<->object). A JSON null is always allowed
/// (it marks the row invalid). Pure mirror of the `as_array()/as_object()`
/// checks in `write_json_to_vector`, exposed for unit testing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ComplexKind {
    List,
    Struct,
    Scalar,
}

fn json_matches_complex_kind(value: &serde_json::Value, kind: ComplexKind) -> bool {
    if value.is_null() {
        return true;
    }
    match kind {
        ComplexKind::List => value.is_array(),
        ComplexKind::Struct => value.is_object(),
        ComplexKind::Scalar => !value.is_array() && !value.is_object(),
    }
}

/// True when the per-row LIST element count is within the reserve cap that
/// guards `duckdb_list_vector_reserve` from an adversarial huge allocation.
fn complex_list_len_within_cap(len: usize, max_len: usize) -> bool {
    len <= max_len
}

/// A short, allocation-free label for a JSON value's kind, for type-mismatch
/// error messages on the `complex` boundary.
fn json_value_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Cheap, allocation-free pre-filter: does `sql` contain the ASCII keyword
/// "load" as a whole word (case-insensitive)? Used to skip the allocating
/// statement split on the common non-LOAD query path. A false positive only
/// costs one extra `split_sql_statements` pass (still correct); it is
/// intentionally conservative (substring "preload" does NOT match, since the
/// preceding char is alphanumeric).
fn contains_load_keyword_ascii_ci(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let needle = b"load";
    if bytes.len() < needle.len() {
        return false;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    for i in 0..=bytes.len() - needle.len() {
        if bytes[i..i + needle.len()].eq_ignore_ascii_case(needle) {
            let before_ok = i == 0 || !is_word(bytes[i - 1]);
            let after_idx = i + needle.len();
            let after_ok = after_idx == bytes.len() || !is_word(bytes[after_idx]);
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

fn split_sql_statements(script: &str) -> Vec<String> {
    let mut stmts = Vec::new();
    let mut buf = String::new();
    let mut chars = script.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                buf.push(c);
                if c == q {
                    if chars.peek() == Some(&q) {
                        buf.push(q);
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    buf.push(c);
                }
                ';' => {
                    stmts.push(std::mem::take(&mut buf));
                }
                _ => buf.push(c),
            },
        }
    }
    if !buf.trim().is_empty() {
        stmts.push(buf);
    }
    stmts
}

fn scalar_function_definitions() -> &'static Mutex<Vec<Arc<ScalarFunctionDefinition>>> {
    SCALAR_FUNCTION_DEFINITIONS.get_or_init(|| Mutex::new(Vec::new()))
}

fn active_connections() -> &'static Mutex<Vec<ConnectionHandle>> {
    ACTIVE_CONNECTIONS.get_or_init(|| Mutex::new(Vec::new()))
}

fn push_scalar_function_definition(def: Arc<ScalarFunctionDefinition>) {
    let mut guard = scalar_function_definitions()
        .lock()
        .expect("scalar function registry mutex poisoned");
    guard.push(def);
}

fn remove_scalar_function_definition(id: u32) {
    let mut guard = scalar_function_definitions()
        .lock()
        .expect("scalar function registry mutex poisoned");
    guard.retain(|entry| entry.id != id);
}

/// Distinct databases backing the currently active connections.
fn distinct_active_databases() -> Vec<duckdb::duckdb_database> {
    let guard = active_connections()
        .lock()
        .expect("active connections mutex poisoned");
    let mut seen: Vec<duckdb::duckdb_database> = Vec::new();
    for conn in guard.iter() {
        if !conn.1.is_null() && !seen.iter().any(|db| *db == conn.1) {
            seen.push(conn.1);
        }
    }
    seen
}

/// Runs `register` on a transient connection to each active database. Extension
/// functions are registered while the active connection is mid-LOAD (a busy
/// ClientContext), so registering on it directly fails; a fresh connection
/// registers into the same catalog (functions are database-wide) and is closed.
unsafe fn register_on_each_database<F>(register: F) -> Result<(), Duckerror>
where
    F: Fn(duckdb::duckdb_connection) -> Result<(), Duckerror>,
{
    for database in distinct_active_databases() {
        let mut conn: duckdb::duckdb_connection = ptr::null_mut();
        if duckdb::duckdb_connect(database, &mut conn) != DUCKDB_SUCCESS {
            return Err(Duckerror::Internal(
                "duckdb_connect failed for function registration".to_string(),
            ));
        }
        let result = register(conn);
        duckdb::duckdb_disconnect(&mut conn);
        result?;
    }
    Ok(())
}

fn register_scalar_function_with_existing_connections(
    definition: &Arc<ScalarFunctionDefinition>,
) -> Result<(), Duckerror> {
    unsafe {
        register_on_each_database(|conn| register_scalar_function_on_connection(conn, definition))
    }
}

fn table_function_definitions() -> &'static Mutex<Vec<Arc<TableFunctionDefinition>>> {
    TABLE_FUNCTION_DEFINITIONS.get_or_init(|| Mutex::new(Vec::new()))
}

fn push_table_function_definition(def: Arc<TableFunctionDefinition>) {
    let mut guard = table_function_definitions()
        .lock()
        .expect("table function registry mutex poisoned");
    guard.push(def);
}

fn register_table_function_with_existing_connections(
    definition: &Arc<TableFunctionDefinition>,
) -> Result<(), Duckerror> {
    unsafe {
        register_on_each_database(|conn| register_table_function_on_connection(conn, definition))
    }
}

fn aggregate_function_definitions() -> &'static Mutex<Vec<Arc<AggregateFunctionDefinition>>> {
    AGGREGATE_FUNCTION_DEFINITIONS.get_or_init(|| Mutex::new(Vec::new()))
}

fn push_aggregate_function_definition(def: Arc<AggregateFunctionDefinition>) {
    let mut guard = aggregate_function_definitions()
        .lock()
        .expect("aggregate function registry mutex poisoned");
    guard.push(def);
}

fn register_aggregate_function_with_existing_connections(
    definition: &Arc<AggregateFunctionDefinition>,
) -> Result<(), Duckerror> {
    unsafe {
        register_on_each_database(|conn| {
            register_aggregate_function_on_connection(conn, definition)
        })
    }
}

impl runtime_exports::GuestScalarCallback for NoopScalarCallback {
    fn new(_handle: u32) -> Self {
        NoopScalarCallback
    }

    fn call(
        &self,
        _args: Vec<Duckvalue>,
        _ctx: runtime_exports::Invokeinfo,
    ) -> Result<Duckvalue, Duckerror> {
        Err(runtime_unavailable_error())
    }
}

impl runtime_exports::GuestTableCallback for NoopTableCallback {
    fn new(_handle: u32) -> Self {
        NoopTableCallback
    }

    fn call(&self, _args: Vec<Duckvalue>) -> Result<runtime_exports::Resultset, Duckerror> {
        Err(runtime_unavailable_error())
    }
}

impl runtime_exports::GuestAggregateCallback for NoopAggregateCallback {
    fn new(_handle: u32) -> Self {
        NoopAggregateCallback
    }

    fn call(&self, _rows: runtime_exports::Rowbatch) -> Result<Duckvalue, Duckerror> {
        Err(runtime_unavailable_error())
    }
}

impl runtime_exports::GuestPragmaCallback for NoopPragmaCallback {
    fn new(_handle: u32) -> Self {
        NoopPragmaCallback
    }

    fn call(&self, _args: Vec<Duckvalue>) -> Result<Option<Duckvalue>, Duckerror> {
        Err(runtime_unavailable_error())
    }
}

impl runtime_exports::GuestCastCallback for NoopCastCallback {
    fn new(_handle: u32) -> Self {
        NoopCastCallback
    }

    fn call(&self, _value: Duckvalue) -> Result<Duckvalue, Duckerror> {
        Err(runtime_unavailable_error())
    }
}

impl runtime_exports::GuestScalarRegistry for ScalarRegistry {
    fn register(
        &self,
        _name: String,
        _arguments: Vec<runtime_exports::Funcarg>,
        _returns: Logicaltype,
        _callback: runtime_exports::ScalarCallback,
        _options: Option<runtime_exports::Funcopts>,
    ) -> Result<u32, Duckerror> {
        let name = _name;
        if name.trim().is_empty() {
            return Err(Duckerror::Invalidargument(
                "function name cannot be empty".to_string(),
            ));
        }

        let arguments_vec: Vec<Logicaltype> = _arguments
            .into_iter()
            .map(|arg| convert_runtime_logicaltype(arg.logical))
            .collect();
        let returns = convert_runtime_logicaltype(_returns);
        let callback_handle = _callback.take_handle();
        let options = _options;

        let id = NEXT_SCALAR_FUNCTION_ID.fetch_add(1, Ordering::Relaxed);
        let definition = Arc::new(ScalarFunctionDefinition {
            id,
            name,
            arguments: arguments_vec,
            returns,
            callback_handle,
            embedded: None,
            options,
        });

        push_scalar_function_definition(definition.clone());

        if let Err(err) = register_scalar_function_with_existing_connections(&definition) {
            remove_scalar_function_definition(id);
            return Err(err);
        }

        Ok(id)
    }
}

impl runtime_exports::GuestTableRegistry for ComponentTableRegistry {
    fn register(
        &self,
        name: String,
        arguments: Vec<runtime_exports::Funcarg>,
        columns: Vec<Columndef>,
        callback: runtime_exports::TableCallback,
        options: Option<runtime_exports::Extopts>,
    ) -> Result<u32, Duckerror> {
        if name.trim().is_empty() {
            return Err(Duckerror::Invalidargument(
                "table function name cannot be empty".to_string(),
            ));
        }
        if columns.is_empty() {
            return Err(Duckerror::Invalidargument(
                "table function must define at least one column".to_string(),
            ));
        }

        let id = NEXT_TABLE_FUNCTION_ID.fetch_add(1, Ordering::Relaxed);
        let callback_handle = callback.take_handle();
        let definition = Arc::new(TableFunctionDefinition {
            id,
            name,
            arguments: arguments
                .into_iter()
                .map(|arg| TableArgument {
                    name: arg.name,
                    logical: convert_runtime_logicaltype(arg.logical),
                })
                .collect(),
            columns: columns
                .into_iter()
                .map(|col| Columndef {
                    name: col.name,
                    logical: convert_runtime_logicaltype(col.logical),
                })
                .collect(),
            callback_handle,
            options,
        });

        push_table_function_definition(definition.clone());

        if let Err(err) = register_table_function_with_existing_connections(&definition) {
            let mut defs = table_function_definitions()
                .lock()
                .expect("table function registry mutex poisoned");
            defs.retain(|entry| entry.id != id);
            return Err(err);
        }

        Ok(id)
    }
}

impl runtime_exports::GuestAggregateRegistry for ComponentAggregateRegistry {
    fn register(
        &self,
        name: String,
        arguments: Vec<runtime_exports::Funcarg>,
        returns: Logicaltype,
        callback: runtime_exports::AggregateCallback,
        options: Option<runtime_exports::Funcopts>,
    ) -> Result<u32, Duckerror> {
        if name.trim().is_empty() {
            return Err(Duckerror::Invalidargument(
                "aggregate function name cannot be empty".to_string(),
            ));
        }

        let id = NEXT_AGGREGATE_FUNCTION_ID.fetch_add(1, Ordering::Relaxed);
        let callback_handle = callback.take_handle();
        let definition = Arc::new(AggregateFunctionDefinition {
            id,
            name,
            arguments: arguments
                .into_iter()
                .map(|arg| convert_runtime_logicaltype(arg.logical))
                .collect(),
            returns,
            callback_handle,
            options,
        });

        push_aggregate_function_definition(definition.clone());

        if let Err(err) = register_aggregate_function_with_existing_connections(&definition) {
            let mut defs = aggregate_function_definitions()
                .lock()
                .expect("aggregate function registry mutex poisoned");
            defs.retain(|entry| entry.id != id);
            return Err(err);
        }

        Ok(id)
    }
}

impl runtime_exports::GuestPragmaRegistry for ComponentPragmaRegistry {
    fn register_call(
        &self,
        _name: String,
        _arguments: Vec<runtime_exports::Funcarg>,
        _returns: Logicaltype,
        _callback: runtime_exports::PragmaCallback,
        _options: Option<runtime_exports::Extopts>,
    ) -> Result<u32, Duckerror> {
        Err(runtime_unavailable_error())
    }
}

impl runtime_exports::GuestMacroRegistry for NoopMacroRegistry {
    fn register_scalar(
        &self,
        _name: String,
        _parameters: Vec<String>,
        _body_sql: String,
        _options: Option<runtime_exports::Extopts>,
    ) -> Result<bool, Duckerror> {
        Err(runtime_unavailable_error())
    }
}

struct RuntimeHost;

impl runtime_exports::Guest for RuntimeHost {
    type ScalarCallback = NoopScalarCallback;
    type TableCallback = NoopTableCallback;
    type AggregateCallback = NoopAggregateCallback;
    type PragmaCallback = NoopPragmaCallback;
    type CastCallback = NoopCastCallback;
    type ScalarRegistry = ScalarRegistry;
    type TableRegistry = ComponentTableRegistry;
    type AggregateRegistry = ComponentAggregateRegistry;
    type PragmaRegistry = ComponentPragmaRegistry;
    type MacroRegistry = NoopMacroRegistry;

    fn get_capability(kind: Capabilitykind) -> Option<runtime_exports::Capability> {
        match kind {
            Capabilitykind::Scalar => Some(runtime_exports::Capability::Scalar(
                runtime_exports::ScalarRegistry::new(ScalarRegistry::default()),
            )),
            Capabilitykind::Table => Some(runtime_exports::Capability::Table(
                runtime_exports::TableRegistry::new(ComponentTableRegistry::default()),
            )),
            Capabilitykind::Aggregate => Some(runtime_exports::Capability::Aggregate(
                runtime_exports::AggregateRegistry::new(ComponentAggregateRegistry::default()),
            )),
            _ => None,
        }
    }

    fn list_capabilities() -> Vec<Capabilitykind> {
        vec![
            Capabilitykind::Scalar,
            Capabilitykind::Table,
            Capabilitykind::Aggregate,
        ]
    }
}

runtime_exports::__export_duckdb_extension_runtime_2_0_0_cabi!(
    RuntimeHost with_types_in bindings::exports::duckdb::extension::runtime
);

fn convert_runtime_logicaltype(logical: runtime_exports::Logicaltype) -> Logicaltype {
    match logical {
        runtime_exports::Logicaltype::Boolean => Logicaltype::Boolean,
        runtime_exports::Logicaltype::Int64 => Logicaltype::Int64,
        runtime_exports::Logicaltype::Uint64 => Logicaltype::Uint64,
        runtime_exports::Logicaltype::Float64 => Logicaltype::Float64,
        runtime_exports::Logicaltype::Text => Logicaltype::Text,
        runtime_exports::Logicaltype::Blob => Logicaltype::Blob,
        runtime_exports::Logicaltype::Int32 => Logicaltype::Int32,
        runtime_exports::Logicaltype::Timestamp => Logicaltype::Timestamp,
        runtime_exports::Logicaltype::Int8 => Logicaltype::Int8,
        runtime_exports::Logicaltype::Int16 => Logicaltype::Int16,
        runtime_exports::Logicaltype::Uint8 => Logicaltype::Uint8,
        runtime_exports::Logicaltype::Uint16 => Logicaltype::Uint16,
        runtime_exports::Logicaltype::Uint32 => Logicaltype::Uint32,
        runtime_exports::Logicaltype::Float32 => Logicaltype::Float32,
        runtime_exports::Logicaltype::Date => Logicaltype::Date,
        runtime_exports::Logicaltype::Time => Logicaltype::Time,
        runtime_exports::Logicaltype::Timestamptz => Logicaltype::Timestamptz,
        runtime_exports::Logicaltype::Decimal => Logicaltype::Decimal,
        runtime_exports::Logicaltype::Interval => Logicaltype::Interval,
        runtime_exports::Logicaltype::Uuid => Logicaltype::Uuid,
        runtime_exports::Logicaltype::Complex(expr) => Logicaltype::Complex(expr),
    }
}

fn convert_loader_logicaltype(logical: extension_loader_hooks::Logicaltype) -> Logicaltype {
    match logical {
        extension_loader_hooks::Logicaltype::Boolean => Logicaltype::Boolean,
        extension_loader_hooks::Logicaltype::Int64 => Logicaltype::Int64,
        extension_loader_hooks::Logicaltype::Uint64 => Logicaltype::Uint64,
        extension_loader_hooks::Logicaltype::Float64 => Logicaltype::Float64,
        extension_loader_hooks::Logicaltype::Text => Logicaltype::Text,
        extension_loader_hooks::Logicaltype::Blob => Logicaltype::Blob,
        extension_loader_hooks::Logicaltype::Int32 => Logicaltype::Int32,
        extension_loader_hooks::Logicaltype::Timestamp => Logicaltype::Timestamp,
        extension_loader_hooks::Logicaltype::Int8 => Logicaltype::Int8,
        extension_loader_hooks::Logicaltype::Int16 => Logicaltype::Int16,
        extension_loader_hooks::Logicaltype::Uint8 => Logicaltype::Uint8,
        extension_loader_hooks::Logicaltype::Uint16 => Logicaltype::Uint16,
        extension_loader_hooks::Logicaltype::Uint32 => Logicaltype::Uint32,
        extension_loader_hooks::Logicaltype::Float32 => Logicaltype::Float32,
        extension_loader_hooks::Logicaltype::Date => Logicaltype::Date,
        extension_loader_hooks::Logicaltype::Time => Logicaltype::Time,
        extension_loader_hooks::Logicaltype::Timestamptz => Logicaltype::Timestamptz,
        extension_loader_hooks::Logicaltype::Decimal => Logicaltype::Decimal,
        extension_loader_hooks::Logicaltype::Interval => Logicaltype::Interval,
        extension_loader_hooks::Logicaltype::Uuid => Logicaltype::Uuid,
        extension_loader_hooks::Logicaltype::Complex(expr) => Logicaltype::Complex(expr),
    }
}

fn convert_loader_funcopts(opts: extension_loader_hooks::FuncOpts) -> runtime_exports::Funcopts {
    runtime_exports::Funcopts {
        description: opts.description,
        tags: opts.tags.into_iter().collect(),
        attributes: opts.attributes,
    }
}

fn convert_loader_extopts(opts: extension_loader_hooks::ExtOpts) -> runtime_exports::Extopts {
    runtime_exports::Extopts {
        description: opts.description,
        tags: opts.tags.into_iter().collect(),
    }
}

fn summarize_loader_funcargs(args: &[extension_loader_hooks::FuncArg]) -> String {
    if args.is_empty() {
        return "[]".to_string();
    }
    let parts: Vec<String> = args
        .iter()
        .map(|arg| {
            let name = arg
                .name
                .as_ref()
                .map(|s| {
                    let owned: String = s.clone().into();
                    owned
                })
                .unwrap_or_else(|| "-".to_string());
            format!(
                "{}:{}",
                name,
                describe_loader_logicaltype(&arg.logical)
            )
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

fn summarize_loader_columns(columns: &[extension_loader_hooks::Columndef]) -> String {
    if columns.is_empty() {
        return "[]".to_string();
    }
    let parts: Vec<String> = columns
        .iter()
        .map(|col| {
            format!(
                "{}:{}",
                col.name,
                describe_loader_logicaltype(&col.logical)
            )
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

fn summarize_loader_funcopts(options: Option<&extension_loader_hooks::FuncOpts>) -> String {
    match options {
        None => "none".to_string(),
        Some(opts) => {
            let description = opts
                .description
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("-");
            let tags = if opts.tags.is_empty() {
                "none".to_string()
            } else {
                format!("[{}]", opts.tags.join(", "))
            };
            let attrs = describe_loader_funcflags(opts.attributes);
            format!("description='{description}', tags={tags}, attrs={attrs}")
        }
    }
}

fn summarize_loader_extopts(options: Option<&extension_loader_hooks::ExtOpts>) -> String {
    match options {
        None => "none".to_string(),
        Some(opts) => {
            let description = opts
                .description
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("-");
            let tags = if opts.tags.is_empty() {
                "none".to_string()
            } else {
                format!("[{}]", opts.tags.join(", "))
            };
            format!("description='{description}', tags={tags}")
        }
    }
}

fn describe_loader_logicaltype(logical: &extension_loader_hooks::Logicaltype) -> &'static str {
    match logical {
        extension_loader_hooks::Logicaltype::Boolean => "BOOLEAN",
        extension_loader_hooks::Logicaltype::Int64 => "INT64",
        extension_loader_hooks::Logicaltype::Uint64 => "UINT64",
        extension_loader_hooks::Logicaltype::Float64 => "FLOAT64",
        extension_loader_hooks::Logicaltype::Text => "TEXT",
        extension_loader_hooks::Logicaltype::Blob => "BLOB",
        extension_loader_hooks::Logicaltype::Int32 => "INT32",
        extension_loader_hooks::Logicaltype::Timestamp => "TIMESTAMP",
        extension_loader_hooks::Logicaltype::Int8 => "INT8",
        extension_loader_hooks::Logicaltype::Int16 => "INT16",
        extension_loader_hooks::Logicaltype::Uint8 => "UINT8",
        extension_loader_hooks::Logicaltype::Uint16 => "UINT16",
        extension_loader_hooks::Logicaltype::Uint32 => "UINT32",
        extension_loader_hooks::Logicaltype::Float32 => "FLOAT32",
        extension_loader_hooks::Logicaltype::Date => "DATE",
        extension_loader_hooks::Logicaltype::Time => "TIME",
        extension_loader_hooks::Logicaltype::Timestamptz => "TIMESTAMPTZ",
        extension_loader_hooks::Logicaltype::Decimal => "DECIMAL",
        extension_loader_hooks::Logicaltype::Interval => "INTERVAL",
        extension_loader_hooks::Logicaltype::Uuid => "UUID",
        extension_loader_hooks::Logicaltype::Complex(_) => "COMPLEX",
    }
}

fn describe_loader_funcflags(flags: extension_loader_hooks::Funcflags) -> String {
    let mut parts = Vec::new();
    if flags.contains(extension_loader_hooks::Funcflags::DETERMINISTIC) {
        parts.push("deterministic");
    }
    if flags.contains(extension_loader_hooks::Funcflags::COMMUTATIVE) {
        parts.push("commutative");
    }
    if flags.contains(extension_loader_hooks::Funcflags::STATELESS) {
        parts.push("stateless");
    }
    if flags.contains(extension_loader_hooks::Funcflags::SIDEEFFECTING) {
        parts.push("sideeffecting");
    }
    if flags.contains(extension_loader_hooks::Funcflags::DEPRECATED) {
        parts.push("deprecated");
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        format!("[{}]", parts.join(", "))
    }
}

fn duckdb_type_for_logical(logical: &Logicaltype) -> duckdb::duckdb_type {
    match logical {
        Logicaltype::Boolean => duckdb::DUCKDB_TYPE_BOOLEAN,
        Logicaltype::Int64 => duckdb::DUCKDB_TYPE_BIGINT,
        Logicaltype::Uint64 => duckdb::DUCKDB_TYPE_UBIGINT,
        Logicaltype::Float64 => duckdb::DUCKDB_TYPE_DOUBLE,
        Logicaltype::Text => duckdb::DUCKDB_TYPE_VARCHAR,
        Logicaltype::Blob => duckdb::DUCKDB_TYPE_BLOB,
        Logicaltype::Int32 => duckdb::DUCKDB_TYPE_INTEGER,
        Logicaltype::Timestamp => duckdb::DUCKDB_TYPE_TIMESTAMP,
        Logicaltype::Int8 => duckdb::DUCKDB_TYPE_TINYINT,
        Logicaltype::Int16 => duckdb::DUCKDB_TYPE_SMALLINT,
        Logicaltype::Uint8 => duckdb::DUCKDB_TYPE_UTINYINT,
        Logicaltype::Uint16 => duckdb::DUCKDB_TYPE_USMALLINT,
        Logicaltype::Uint32 => duckdb::DUCKDB_TYPE_UINTEGER,
        Logicaltype::Float32 => duckdb::DUCKDB_TYPE_FLOAT,
        Logicaltype::Date => duckdb::DUCKDB_TYPE_DATE,
        Logicaltype::Time => duckdb::DUCKDB_TYPE_TIME,
        Logicaltype::Timestamptz => duckdb::DUCKDB_TYPE_TIMESTAMP_TZ,
        Logicaltype::Decimal => duckdb::DUCKDB_TYPE_DECIMAL,
        Logicaltype::Interval => duckdb::DUCKDB_TYPE_INTERVAL,
        Logicaltype::Uuid => duckdb::DUCKDB_TYPE_UUID,
        // ESCAPE-HATCH: a `complex` type cannot be built from a single type code;
        // `create_duckdb_logical_type` resolves it via `resolve_logical_type`
        // BEFORE this is reached, so this arm is a placeholder.
        Logicaltype::Complex(_) => duckdb::DUCKDB_TYPE_LIST,
    }
}

/// The default width/scale used when a bare `Logicaltype::Decimal` (which
/// carries no precision) must be materialized as a DuckDB logical type. width=38
/// forces the int128 (hugeint) physical representation, which is what
/// `write_duckvalue_to_vector` writes for `Duckvalue::Decimal`.
const DEFAULT_DECIMAL_WIDTH: u8 = 38;
const DEFAULT_DECIMAL_SCALE: u8 = 4;

/// Recursion cap for the `complex` JSON<->vector escape hatch. The JSON arrives
/// from a COMPONENT (a trust boundary): a `[[[[...]]]]` payload, or a recursive
/// `complex` type-expr, would otherwise recurse `write_json_to_vector` /
/// `read_vector_to_json` once per nesting level and overflow the wasm stack ->
/// an abort that takes down the whole instance / the host. Real nested types
/// (LIST<STRUCT<LIST<...>>>) are shallow; this cap is generous but bounded. Same
/// fix the WKB fuzz pass applied to the parser components' never-panic contract.
const COMPLEX_MAX_DEPTH: u32 = 64;

/// Per-row element cap for the `complex` LIST writer. A component-supplied JSON
/// array length feeds `duckdb_list_vector_reserve`; an adversarial huge length
/// would request an enormous allocation. The serde parse already bounds the
/// array to what actually fit in the JSON string, but cap the per-row reserve so
/// a pathological (but parseable) payload errors instead of OOM-aborting.
const COMPLEX_MAX_LIST_LEN: usize = 16 * 1024 * 1024;

unsafe fn create_duckdb_logical_type(
    connection: duckdb::duckdb_connection,
    logical: &Logicaltype,
) -> Result<duckdb::duckdb_logical_type, Duckerror> {
    // ESCAPE-HATCH: a `complex` type carries a DuckDB type-expression string
    // (e.g. "INTEGER[]" / "STRUCT(a INTEGER, b VARCHAR)"). Resolve it to a real
    // logical type via the same `CAST(NULL AS <expr>)` trick used by casts.
    if let Logicaltype::Complex(type_expr) = logical {
        // Use the logical-type-only resolver: a nested type like INTEGER[] has a
        // physical type id (LIST=24, STRUCT=25) that `duckdb_type_to_logical`
        // intentionally does not map to a scalar enum, so we must NOT require it.
        let logical_lt = resolve_logical_type_only(connection, type_expr).map_err(|e| {
            Duckerror::Invalidargument(format!(
                "could not resolve complex type '{type_expr}': {e}"
            ))
        })?;
        return Ok(logical_lt);
    }
    // DECIMAL cannot be built from a bare type code (duckdb_create_logical_type
    // rejects DUCKDB_TYPE_DECIMAL); it needs an explicit width/scale.
    if matches!(logical, Logicaltype::Decimal) {
        let ty = duckdb::duckdb_create_decimal_type(DEFAULT_DECIMAL_WIDTH, DEFAULT_DECIMAL_SCALE);
        return if ty.is_null() {
            Err(Duckerror::Internal(
                "duckdb_create_decimal_type returned null".to_string(),
            ))
        } else {
            Ok(ty)
        };
    }
    let ty = duckdb::duckdb_create_logical_type(duckdb_type_for_logical(logical));
    if ty.is_null() {
        Err(Duckerror::Internal(
            "duckdb_create_logical_type returned null".to_string(),
        ))
    } else {
        Ok(ty)
    }
}

struct ScalarFunctionGuard {
    function: duckdb::duckdb_scalar_function,
}

impl Drop for ScalarFunctionGuard {
    fn drop(&mut self) {
        unsafe {
            if !self.function.is_null() {
                duckdb::duckdb_destroy_scalar_function(&mut self.function);
                self.function = ptr::null_mut();
            }
        }
    }
}

struct TableFunctionGuard {
    function: duckdb::duckdb_table_function,
}

impl Drop for TableFunctionGuard {
    fn drop(&mut self) {
        unsafe {
            if !self.function.is_null() {
                duckdb::duckdb_destroy_table_function(&mut self.function);
                self.function = ptr::null_mut();
            }
        }
    }
}

struct AggregateFunctionGuard {
    function: duckdb::duckdb_aggregate_function,
}

impl Drop for AggregateFunctionGuard {
    fn drop(&mut self) {
        unsafe {
            if !self.function.is_null() {
                duckdb::duckdb_destroy_aggregate_function(&mut self.function);
                self.function = ptr::null_mut();
            }
        }
    }
}

unsafe fn register_scalar_function_on_connection(
    connection: duckdb::duckdb_connection,
    definition: &Arc<ScalarFunctionDefinition>,
) -> Result<(), Duckerror> {
    let function = duckdb::duckdb_create_scalar_function();
    if function.is_null() {
        return Err(Duckerror::Internal(
            "duckdb_create_scalar_function returned null".to_string(),
        ));
    }
    let _guard = ScalarFunctionGuard { function };

    let name_c = CString::new(definition.name.as_str()).map_err(|_| {
        Duckerror::Invalidargument("function name contains embedded null byte".to_string())
    })?;
    duckdb::duckdb_scalar_function_set_name(function, name_c.as_ptr());

    for logical in &definition.arguments {
        let mut logical_type = create_duckdb_logical_type(connection, logical)?;
        duckdb::duckdb_scalar_function_add_parameter(function, logical_type);
        duckdb::duckdb_destroy_logical_type(&mut logical_type);
    }

    let mut return_type = create_duckdb_logical_type(connection, &definition.returns)?;
    duckdb::duckdb_scalar_function_set_return_type(function, return_type);
    duckdb::duckdb_destroy_logical_type(&mut return_type);

    if let Some(opts) = definition.options.as_ref() {
        if !opts.attributes.contains(Funcflags::DETERMINISTIC) {
            duckdb::duckdb_scalar_function_set_volatile(function);
        }
    }

    duckdb::duckdb_scalar_function_set_function(function, Some(scalar_function_callback));

    let entry = Box::new(ScalarFunctionEntry {
        definition: definition.clone(),
    });
    let entry_ptr = Box::into_raw(entry) as *mut c_void;
    duckdb::duckdb_scalar_function_set_extra_info(
        function,
        entry_ptr,
        Some(scalar_function_entry_destroy),
    );

    let state = duckdb::duckdb_register_scalar_function(connection, function);
    if state != DUCKDB_SUCCESS {
        scalar_function_entry_destroy(entry_ptr);
        Err(Duckerror::Internal(format!(
            "duckdb_register_scalar_function failed for {}",
            definition.name
        )))
    } else {
        Ok(())
    }
}

unsafe extern "C" fn scalar_function_entry_destroy(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let _ = Box::from_raw(ptr as *mut ScalarFunctionEntry);
}

unsafe extern "C" fn scalar_function_callback(
    info: duckdb::duckdb_function_info,
    input: duckdb::duckdb_data_chunk,
    output: duckdb::duckdb_vector,
) {
    if let Err(err) = execute_scalar_function(info, input, output) {
        if let Ok(message) = sanitize_error_message(&format_duckerror(&err)) {
            duckdb::duckdb_scalar_function_set_error(info, message.as_ptr());
        }
    }
}

unsafe fn execute_scalar_function(
    info: duckdb::duckdb_function_info,
    input: duckdb::duckdb_data_chunk,
    output: duckdb::duckdb_vector,
) -> Result<(), Duckerror> {
    let entry_ptr = duckdb::duckdb_scalar_function_get_extra_info(info);
    if entry_ptr.is_null() {
        return Err(Duckerror::Internal(
            "scalar function missing dispatcher entry".to_string(),
        ));
    }
    let entry = &*(entry_ptr as *const ScalarFunctionEntry);

    let row_count = duckdb::duckdb_data_chunk_get_size(input);
    let mut columns = Vec::with_capacity(entry.definition.arguments.len());
    for (idx, logical) in entry.definition.arguments.iter().enumerate() {
        let vector = duckdb::duckdb_data_chunk_get_vector(input, idx as duckdb::idx_t);
        columns.push(ScalarInputColumn {
            vector,
            logical: logical.clone(),
        });
    }

    // Build the whole chunk's arguments and dispatch in ONE batched WIT call
    // (call-scalar-batch) rather than one call per row -- the per-row crossing
    // dominated extension scalar cost (~1.1us/row measured). Semantically
    // identical: row i's args are rows[i], the result is results[i].
    let mut rows = Vec::with_capacity(row_count as usize);
    for row in 0..row_count {
        let mut args = Vec::with_capacity(columns.len());
        for column in &columns {
            args.push(read_scalar_argument(column, row)?);
        }
        rows.push(args);
    }

    let results = if let Some(embedded) = entry.definition.embedded {
        // Compiled-into-core extension: run the Rust function directly, no WIT.
        let mut out = Vec::with_capacity(rows.len());
        for args in &rows {
            out.push(embedded(args)?);
        }
        out
    } else {
        let invoke = callback_dispatch::Invokeinfo {
            rowindex: Some(0),
            iswindow: false,
        };
        callback_dispatch::call_scalar_batch(
            entry.definition.callback_handle,
            rows.as_slice(),
            invoke,
        )
        .map_err(|err| err)?
    };

    if results.len() as u64 != row_count {
        return Err(Duckerror::Internal(format!(
            "scalar batch returned {} values for {} rows",
            results.len(),
            row_count
        )));
    }
    for (row, result) in results.into_iter().enumerate() {
        write_duckvalue_to_vector(
            output,
            &entry.definition.returns,
            row as duckdb::idx_t,
            result,
        )?;
    }

    Ok(())
}

unsafe fn register_table_function_on_connection(
    connection: duckdb::duckdb_connection,
    definition: &Arc<TableFunctionDefinition>,
) -> Result<(), Duckerror> {
    let function = duckdb::duckdb_create_table_function();
    if function.is_null() {
        return Err(Duckerror::Internal(
            "duckdb_create_table_function returned null".to_string(),
        ));
    }
    let _guard = TableFunctionGuard { function };

    let name_c = CString::new(definition.name.as_str()).map_err(|_| {
        Duckerror::Invalidargument("function name contains embedded null byte".to_string())
    })?;
    duckdb::duckdb_table_function_set_name(function, name_c.as_ptr());

    for arg in &definition.arguments {
        let mut logical_type = create_duckdb_logical_type(connection, &arg.logical)?;
        duckdb::duckdb_table_function_add_parameter(function, logical_type);
        duckdb::duckdb_destroy_logical_type(&mut logical_type);
    }

    let entry = Box::new(TableFunctionEntry {
        definition: definition.clone(),
    });
    let entry_ptr = Box::into_raw(entry) as *mut c_void;
    duckdb::duckdb_table_function_set_extra_info(
        function,
        entry_ptr,
        Some(table_function_entry_destroy),
    );

    duckdb::duckdb_table_function_set_bind(function, Some(table_function_bind));
    duckdb::duckdb_table_function_set_init(function, Some(table_function_init));
    duckdb::duckdb_table_function_set_function(function, Some(table_function_execute));

    let state = duckdb::duckdb_register_table_function(connection, function);
    if state != DUCKDB_SUCCESS {
        return Err(Duckerror::Internal(format!(
            "duckdb_register_table_function failed for {}",
            definition.name
        )));
    }

    Ok(())
}

unsafe extern "C" fn table_function_entry_destroy(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    drop(Box::from_raw(ptr as *mut TableFunctionEntry));
}

unsafe extern "C" fn table_function_bind(info: duckdb::duckdb_bind_info) {
    if let Err(err) = table_function_bind_impl(info) {
        let message = format_duckerror(&err);
        let cstring = CString::new(message).unwrap_or_else(|_| CString::new("bind error").unwrap());
        duckdb::duckdb_bind_set_error(info, cstring.as_ptr());
    }
}

unsafe fn table_function_bind_impl(info: duckdb::duckdb_bind_info) -> Result<(), Duckerror> {
    let entry_ptr = duckdb::duckdb_bind_get_extra_info(info);
    if entry_ptr.is_null() {
        return Err(Duckerror::Internal(
            "table function missing definition info".to_string(),
        ));
    }
    let entry = &*(entry_ptr as *const TableFunctionEntry);
    // The bind context has no connection handle; for resolving `complex` column
    // types (the escape-hatch) we need one, so open a transient connection on the
    // first active database. Plain (non-complex) columns ignore it.
    let mut bind_conn: duckdb::duckdb_connection = ptr::null_mut();
    let needs_conn = entry
        .definition
        .columns
        .iter()
        .any(|c| matches!(c.logical, Logicaltype::Complex(_)));
    if needs_conn {
        if let Some(db) = distinct_active_databases().into_iter().next() {
            duckdb::duckdb_connect(db, &mut bind_conn);
        }
    }
    let bind_conn_guard = bind_conn;
    let bind_result = (|| {
        for column in &entry.definition.columns {
            let mut logical_type = create_duckdb_logical_type(bind_conn_guard, &column.logical)?;
            let name_c = CString::new(column.name.as_str()).map_err(|_| {
                Duckerror::Invalidargument("column name contains embedded null byte".to_string())
            })?;
            duckdb::duckdb_bind_add_result_column(info, name_c.as_ptr(), logical_type);
            duckdb::duckdb_destroy_logical_type(&mut logical_type);
        }
        Ok::<(), Duckerror>(())
    })();
    if !bind_conn.is_null() {
        duckdb::duckdb_disconnect(&mut bind_conn);
    }
    bind_result?;

    let param_count = duckdb::duckdb_bind_get_parameter_count(info);
    if param_count as usize != entry.definition.arguments.len() {
        return Err(Duckerror::Invalidargument(format!(
            "expected {} arguments but received {}",
            entry.definition.arguments.len(),
            param_count
        )));
    }

    let mut arguments = Vec::with_capacity(param_count as usize);
    for (idx, arg_def) in entry.definition.arguments.iter().enumerate() {
        let mut value = duckdb::duckdb_bind_get_parameter(info, idx as duckdb::idx_t);
        let converted = match duckdb_value_to_duckvalue(value, &arg_def.logical) {
            Ok(val) => val,
            Err(err) => {
                duckdb::duckdb_destroy_value(&mut value);
                return Err(err);
            }
        };
        duckdb::duckdb_destroy_value(&mut value);
        arguments.push(converted);
    }

    let bind_data = Box::new(TableFunctionBindData {
        definition: entry.definition.clone(),
        arguments,
    });
    duckdb::duckdb_bind_set_bind_data(
        info,
        Box::into_raw(bind_data) as *mut c_void,
        Some(table_function_bind_data_destroy),
    );
    Ok(())
}

unsafe extern "C" fn table_function_bind_data_destroy(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    drop(Box::from_raw(ptr as *mut TableFunctionBindData));
}

unsafe extern "C" fn table_function_init(info: duckdb::duckdb_init_info) {
    if let Err(err) = table_function_init_impl(info) {
        let message = format_duckerror(&err);
        let cstring = CString::new(message).unwrap_or_else(|_| CString::new("init error").unwrap());
        duckdb::duckdb_init_set_error(info, cstring.as_ptr());
    }
}

unsafe fn table_function_init_impl(info: duckdb::duckdb_init_info) -> Result<(), Duckerror> {
    let bind_ptr = duckdb::duckdb_init_get_bind_data(info);
    if bind_ptr.is_null() {
        return Err(Duckerror::Internal(
            "table function missing bind data".to_string(),
        ));
    }
    let bind_data = &*(bind_ptr as *const TableFunctionBindData);
    let rows =
        callback_dispatch::call_table(bind_data.definition.callback_handle, &bind_data.arguments)
            .map_err(|err| err)?;

    let state = Box::new(TableFunctionState {
        definition: bind_data.definition.clone(),
        rows,
        offset: 0,
    });

    duckdb::duckdb_init_set_init_data(
        info,
        Box::into_raw(state) as *mut c_void,
        Some(table_function_state_destroy),
    );
    Ok(())
}

unsafe extern "C" fn table_function_state_destroy(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    drop(Box::from_raw(ptr as *mut TableFunctionState));
}

unsafe extern "C" fn table_function_execute(
    info: duckdb::duckdb_function_info,
    output: duckdb::duckdb_data_chunk,
) {
    if let Err(err) = table_function_execute_impl(info, output) {
        let message = format_duckerror(&err);
        let cstring =
            CString::new(message).unwrap_or_else(|_| CString::new("execute error").unwrap());
        duckdb::duckdb_function_set_error(info, cstring.as_ptr());
        duckdb::duckdb_data_chunk_set_size(output, 0);
    }
}

unsafe fn table_function_execute_impl(
    info: duckdb::duckdb_function_info,
    output: duckdb::duckdb_data_chunk,
) -> Result<(), Duckerror> {
    let state_ptr = duckdb::duckdb_function_get_init_data(info);
    if state_ptr.is_null() {
        duckdb::duckdb_data_chunk_set_size(output, 0);
        return Ok(());
    }
    let state = &mut *(state_ptr as *mut TableFunctionState);
    let remaining = state.rows.len().saturating_sub(state.offset);
    if remaining == 0 {
        duckdb::duckdb_data_chunk_set_size(output, 0);
        return Ok(());
    }
    let chunk_size = remaining.min(1024);
    duckdb::duckdb_data_chunk_set_size(output, chunk_size as duckdb::idx_t);
    let expected_columns = state.definition.columns.len();
    for (col_idx, column) in state.definition.columns.iter().enumerate() {
        let vector = duckdb::duckdb_data_chunk_get_vector(output, col_idx as duckdb::idx_t);
        for row in 0..chunk_size {
            let row_values = &state.rows[state.offset + row];
            if row_values.len() != expected_columns {
                return Err(Duckerror::Internal(format!(
                    "table function row {} returned {} columns, expected {}",
                    state.offset + row,
                    row_values.len(),
                    expected_columns
                )));
            }
            let value = row_values[col_idx].clone();
            write_duckvalue_to_vector(vector, &column.logical, row as duckdb::idx_t, value)?;
        }
    }
    state.offset += chunk_size;
    Ok(())
}

unsafe fn register_aggregate_function_on_connection(
    connection: duckdb::duckdb_connection,
    definition: &Arc<AggregateFunctionDefinition>,
) -> Result<(), Duckerror> {
    let function = duckdb::duckdb_create_aggregate_function();
    if function.is_null() {
        return Err(Duckerror::Internal(
            "duckdb_create_aggregate_function returned null".to_string(),
        ));
    }
    let _guard = AggregateFunctionGuard { function };

    let name_c = CString::new(definition.name.as_str()).map_err(|_| {
        Duckerror::Invalidargument("function name contains embedded null byte".to_string())
    })?;
    duckdb::duckdb_aggregate_function_set_name(function, name_c.as_ptr());

    for logical in &definition.arguments {
        let mut logical_type = create_duckdb_logical_type(connection, logical)?;
        duckdb::duckdb_aggregate_function_add_parameter(function, logical_type);
        duckdb::duckdb_destroy_logical_type(&mut logical_type);
    }

    let mut return_type = create_duckdb_logical_type(connection, &definition.returns)?;
    duckdb::duckdb_aggregate_function_set_return_type(function, return_type);
    duckdb::duckdb_destroy_logical_type(&mut return_type);

    duckdb::duckdb_aggregate_function_set_functions(
        function,
        Some(aggregate_state_size),
        Some(aggregate_state_init),
        Some(aggregate_state_update),
        Some(aggregate_state_combine),
        Some(aggregate_state_finalize),
    );
    duckdb::duckdb_aggregate_function_set_destructor(function, Some(aggregate_state_destructor));

    let entry = Box::new(AggregateFunctionEntry {
        definition: definition.clone(),
    });
    duckdb::duckdb_aggregate_function_set_extra_info(
        function,
        Box::into_raw(entry) as *mut c_void,
        Some(aggregate_function_entry_destroy),
    );

    let state = duckdb::duckdb_register_aggregate_function(connection, function);
    if state != DUCKDB_SUCCESS {
        return Err(Duckerror::Internal(format!(
            "duckdb_register_aggregate_function failed for {}",
            definition.name
        )));
    }

    Ok(())
}

unsafe extern "C" fn aggregate_function_entry_destroy(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    drop(Box::from_raw(ptr as *mut AggregateFunctionEntry));
}

unsafe extern "C" fn aggregate_state_size(_info: duckdb::duckdb_function_info) -> duckdb::idx_t {
    std::mem::size_of::<*mut AggregateState>() as duckdb::idx_t
}

unsafe extern "C" fn aggregate_state_init(
    _info: duckdb::duckdb_function_info,
    state: duckdb::duckdb_aggregate_state,
) {
    let slot = state as *mut *mut AggregateState;
    *slot = ptr::null_mut();
}

unsafe extern "C" fn aggregate_state_update(
    info: duckdb::duckdb_function_info,
    input: duckdb::duckdb_data_chunk,
    states: *mut duckdb::duckdb_aggregate_state,
) {
    if let Err(err) = aggregate_state_update_impl(info, input, states) {
        let message = format_duckerror(&err);
        let cstring = CString::new(message)
            .unwrap_or_else(|_| CString::new("aggregate update error").unwrap());
        duckdb::duckdb_aggregate_function_set_error(info, cstring.as_ptr());
    }
}

unsafe fn aggregate_state_update_impl(
    info: duckdb::duckdb_function_info,
    input: duckdb::duckdb_data_chunk,
    states: *mut duckdb::duckdb_aggregate_state,
) -> Result<(), Duckerror> {
    let entry_ptr = duckdb::duckdb_aggregate_function_get_extra_info(info);
    if entry_ptr.is_null() {
        return Err(Duckerror::Internal(
            "aggregate function missing definition info".to_string(),
        ));
    }
    let entry = &*(entry_ptr as *const AggregateFunctionEntry);

    let row_count = duckdb::duckdb_data_chunk_get_size(input);
    if row_count == 0 {
        return Ok(());
    }
    let mut columns = Vec::with_capacity(entry.definition.arguments.len());
    for (idx, logical) in entry.definition.arguments.iter().enumerate() {
        columns.push(ScalarInputColumn {
            vector: duckdb::duckdb_data_chunk_get_vector(input, idx as duckdb::idx_t),
            logical: logical.clone(),
        });
    }

    for row in 0..row_count {
        let state_ptr = *states.add(row as usize);
        let slot = state_ptr as *mut *mut AggregateState;
        let state = ensure_aggregate_state(slot);

        let mut values = Vec::with_capacity(columns.len());
        for column in &columns {
            values.push(read_scalar_argument(column, row)?);
        }
        (*state).rows.push(values);
    }

    Ok(())
}

unsafe fn ensure_aggregate_state(slot: *mut *mut AggregateState) -> *mut AggregateState {
    if (*slot).is_null() {
        let boxed = Box::new(AggregateState { rows: Vec::new() });
        *slot = Box::into_raw(boxed);
    }
    *slot
}

unsafe extern "C" fn aggregate_state_combine(
    info: duckdb::duckdb_function_info,
    source: *mut duckdb::duckdb_aggregate_state,
    target: *mut duckdb::duckdb_aggregate_state,
    count: duckdb::idx_t,
) {
    if let Err(err) = aggregate_state_combine_impl(source, target, count) {
        let message = format_duckerror(&err);
        let cstring = CString::new(message)
            .unwrap_or_else(|_| CString::new("aggregate combine error").unwrap());
        duckdb::duckdb_aggregate_function_set_error(info, cstring.as_ptr());
    }
}

unsafe fn aggregate_state_combine_impl(
    source: *mut duckdb::duckdb_aggregate_state,
    target: *mut duckdb::duckdb_aggregate_state,
    count: duckdb::idx_t,
) -> Result<(), Duckerror> {
    for i in 0..count as usize {
        let source_slot = *source.add(i) as *mut *mut AggregateState;
        if (*source_slot).is_null() {
            continue;
        }
        let target_slot = *target.add(i) as *mut *mut AggregateState;
        let target_state = ensure_aggregate_state(target_slot);
        let source_state = *source_slot;
        (*target_state)
            .rows
            .extend((*source_state).rows.iter().cloned());
        drop(Box::from_raw(source_state));
        *source_slot = ptr::null_mut();
    }
    Ok(())
}

unsafe extern "C" fn aggregate_state_finalize(
    info: duckdb::duckdb_function_info,
    states: *mut duckdb::duckdb_aggregate_state,
    result: duckdb::duckdb_vector,
    count: duckdb::idx_t,
    offset: duckdb::idx_t,
) {
    if let Err(err) = aggregate_state_finalize_impl(info, states, result, count, offset) {
        let message = format_duckerror(&err);
        let cstring = CString::new(message)
            .unwrap_or_else(|_| CString::new("aggregate finalize error").unwrap());
        duckdb::duckdb_aggregate_function_set_error(info, cstring.as_ptr());
    }
}

unsafe fn aggregate_state_finalize_impl(
    info: duckdb::duckdb_function_info,
    states: *mut duckdb::duckdb_aggregate_state,
    result: duckdb::duckdb_vector,
    count: duckdb::idx_t,
    offset: duckdb::idx_t,
) -> Result<(), Duckerror> {
    let entry_ptr = duckdb::duckdb_aggregate_function_get_extra_info(info);
    if entry_ptr.is_null() {
        return Err(Duckerror::Internal(
            "aggregate function missing definition info".to_string(),
        ));
    }
    let entry = &*(entry_ptr as *const AggregateFunctionEntry);

    for i in 0..count as usize {
        let slot = *states.add(i) as *mut *mut AggregateState;
        let rows = take_aggregate_rows(slot);
        let value = callback_dispatch::call_aggregate(entry.definition.callback_handle, &rows)
            .map_err(|err| err)?;
        write_duckvalue_to_vector(
            result,
            &entry.definition.returns,
            offset + i as duckdb::idx_t,
            value,
        )?;
    }
    Ok(())
}

unsafe fn take_aggregate_rows(slot: *mut *mut AggregateState) -> Vec<Vec<Duckvalue>> {
    if (*slot).is_null() {
        Vec::new()
    } else {
        let boxed = Box::from_raw(*slot);
        *slot = ptr::null_mut();
        boxed.rows
    }
}

unsafe extern "C" fn aggregate_state_destructor(
    states: *mut duckdb::duckdb_aggregate_state,
    count: duckdb::idx_t,
) {
    for i in 0..count as usize {
        let slot = *states.add(i) as *mut *mut AggregateState;
        if !slot.is_null() && !(*slot).is_null() {
            drop(Box::from_raw(*slot));
            *slot = ptr::null_mut();
        }
    }
}

unsafe fn read_scalar_argument(
    column: &ScalarInputColumn,
    row: duckdb::idx_t,
) -> Result<Duckvalue, Duckerror> {
    let validity = duckdb::duckdb_vector_get_validity(column.vector);
    let is_valid = validity.is_null() || duckdb::duckdb_validity_row_is_valid(validity, row);
    if !is_valid {
        return Ok(Duckvalue::Null);
    }

    match &column.logical {
        Logicaltype::Boolean => {
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut bool;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Boolean(value))
        }
        Logicaltype::Int64 => {
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut i64;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Int64(value))
        }
        Logicaltype::Uint64 => {
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut u64;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Uint64(value))
        }
        Logicaltype::Float64 => {
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut f64;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Float64(value))
        }
        Logicaltype::Text => {
            let data =
                duckdb::duckdb_vector_get_data(column.vector) as *mut duckdb::duckdb_string_t;
            let string_value = ptr::read(data.add(row as usize));
            let bytes = duckdb_string_to_vec(string_value);
            let text = String::from_utf8(bytes).map_err(|_| {
                Duckerror::Invalidargument("text argument contained invalid UTF-8 data".to_string())
            })?;
            Ok(Duckvalue::Text(text))
        }
        Logicaltype::Blob => {
            let data =
                duckdb::duckdb_vector_get_data(column.vector) as *mut duckdb::duckdb_string_t;
            let string_value = ptr::read(data.add(row as usize));
            Ok(Duckvalue::Blob(duckdb_string_to_vec(string_value)))
        }
        Logicaltype::Int32 => {
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut i32;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Int32(value))
        }
        Logicaltype::Timestamp => {
            // DUCKDB_TYPE_TIMESTAMP is physically an int64 of micros since 1970.
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut i64;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Timestamp(value))
        }
        Logicaltype::Int8 => {
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut i8;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Int8(value))
        }
        Logicaltype::Int16 => {
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut i16;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Int16(value))
        }
        Logicaltype::Uint8 => {
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut u8;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Uint8(value))
        }
        Logicaltype::Uint16 => {
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut u16;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Uint16(value))
        }
        Logicaltype::Uint32 => {
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut u32;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Uint32(value))
        }
        Logicaltype::Float32 => {
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut f32;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Float32(value))
        }
        Logicaltype::Date => {
            // DUCKDB_TYPE_DATE is physically an int32 of days since 1970-01-01.
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut i32;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Date(value))
        }
        Logicaltype::Time => {
            // DUCKDB_TYPE_TIME is physically an int64 of micros since midnight.
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut i64;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Time(value))
        }
        Logicaltype::Timestamptz => {
            // DUCKDB_TYPE_TIMESTAMP_TZ is physically an int64 of micros since 1970 (UTC).
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut i64;
            let value = *data.add(row as usize);
            Ok(Duckvalue::Timestamptz(value))
        }
        Logicaltype::Decimal => {
            // We materialize DECIMAL as decimal(38, S), whose physical storage is
            // an int128 of the unscaled value. Read the raw 128-bit integer.
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut i128;
            let raw = *data.add(row as usize) as u128;
            Ok(Duckvalue::Decimal(Decimalvalue {
                lower: raw as u64,
                upper: (raw >> 64) as u64,
                width: DEFAULT_DECIMAL_WIDTH,
                scale: DEFAULT_DECIMAL_SCALE,
            }))
        }
        Logicaltype::Interval => {
            // DUCKDB_TYPE_INTERVAL is physically a {months:i32, days:i32, micros:i64}.
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut duckdb::duckdb_interval;
            let iv = *data.add(row as usize);
            Ok(Duckvalue::Interval(Intervalvalue {
                months: iv.months,
                days: iv.days,
                micros: iv.micros,
            }))
        }
        Logicaltype::Uuid => {
            // DUCKDB_TYPE_UUID is physically a hugeint with the high bit flipped
            // (so unsigned ordering matches). Flip it back to recover the logical
            // 128-bit UUID, then split into hi/lo halves.
            let data = duckdb::duckdb_vector_get_data(column.vector) as *mut duckdb::duckdb_hugeint;
            let hh = *data.add(row as usize);
            let physical = ((hh.upper as u128) << 64) | hh.lower as u128;
            let logical = physical ^ (1u128 << 127);
            Ok(Duckvalue::Uuid(Uuidvalue {
                hi: (logical >> 64) as u64,
                lo: logical as u64,
            }))
        }
        Logicaltype::Complex(type_expr) => {
            // ESCAPE-HATCH read path: serialize the LIST/STRUCT vector row to JSON
            // so a component can RECEIVE nested args as `complex{type-expr, json}`.
            let col_type = duckdb::duckdb_vector_get_column_type(column.vector);
            let json = read_vector_to_json(column.vector, col_type, row);
            let mut col_type_mut = col_type;
            duckdb::duckdb_destroy_logical_type(&mut col_type_mut);
            Ok(Duckvalue::Complex(Complexvalue {
                type_expr: type_expr.clone(),
                json: json.to_string(),
            }))
        }
    }
}

/// Read a single row of a duckdb vector (whose logical type is `col_type`) into a
/// `serde_json::Value`. Recurses into LIST/STRUCT children via the C vector API.
unsafe fn read_vector_to_json(
    vector: duckdb::duckdb_vector,
    col_type: duckdb::duckdb_logical_type,
    row: duckdb::idx_t,
) -> serde_json::Value {
    read_vector_to_json_depth(vector, col_type, row, 0)
}

unsafe fn read_vector_to_json_depth(
    vector: duckdb::duckdb_vector,
    col_type: duckdb::duckdb_logical_type,
    row: duckdb::idx_t,
    depth: u32,
) -> serde_json::Value {
    use serde_json::Value as J;
    // Trust boundary: a component can declare an arbitrarily deep `complex`
    // type-expr. Cap the recursion so reading the vector cannot overflow the
    // wasm stack; past the cap we truncate to null rather than panic.
    if depth > COMPLEX_MAX_DEPTH {
        return J::Null;
    }
    let validity = duckdb::duckdb_vector_get_validity(vector);
    if !validity.is_null() && !duckdb::duckdb_validity_row_is_valid(validity, row) {
        return J::Null;
    }
    let type_id = duckdb::duckdb_get_type_id(col_type);
    match type_id {
        duckdb::DUCKDB_TYPE_LIST => {
            let entries = duckdb::duckdb_vector_get_data(vector) as *const duckdb::duckdb_list_entry;
            let entry = *entries.add(row as usize);
            let child = duckdb::duckdb_list_vector_get_child(vector);
            let child_type = duckdb::duckdb_list_type_child_type(col_type);
            let mut out = Vec::with_capacity(entry.length as usize);
            for i in 0..entry.length {
                out.push(read_vector_to_json_depth(
                    child,
                    child_type,
                    (entry.offset + i) as duckdb::idx_t,
                    depth + 1,
                ));
            }
            let mut ct = child_type;
            duckdb::duckdb_destroy_logical_type(&mut ct);
            J::Array(out)
        }
        duckdb::DUCKDB_TYPE_STRUCT => {
            let field_count = duckdb::duckdb_struct_type_child_count(col_type);
            let mut map = serde_json::Map::new();
            for i in 0..field_count {
                let name_ptr = duckdb::duckdb_struct_type_child_name(col_type, i);
                let field_name = CStr::from_ptr(name_ptr).to_string_lossy().into_owned();
                duckdb::duckdb_free(name_ptr as *mut c_void);
                let field_child = duckdb::duckdb_struct_vector_get_child(vector, i);
                let field_type = duckdb::duckdb_struct_type_child_type(col_type, i);
                map.insert(
                    field_name,
                    read_vector_to_json_depth(field_child, field_type, row, depth + 1),
                );
                let mut ft = field_type;
                duckdb::duckdb_destroy_logical_type(&mut ft);
            }
            J::Object(map)
        }
        duckdb::DUCKDB_TYPE_BOOLEAN => {
            J::Bool(*(duckdb::duckdb_vector_get_data(vector) as *const bool).add(row as usize))
        }
        duckdb::DUCKDB_TYPE_TINYINT => {
            J::from(*(duckdb::duckdb_vector_get_data(vector) as *const i8).add(row as usize))
        }
        duckdb::DUCKDB_TYPE_SMALLINT => {
            J::from(*(duckdb::duckdb_vector_get_data(vector) as *const i16).add(row as usize))
        }
        duckdb::DUCKDB_TYPE_INTEGER => {
            J::from(*(duckdb::duckdb_vector_get_data(vector) as *const i32).add(row as usize))
        }
        duckdb::DUCKDB_TYPE_BIGINT => {
            J::from(*(duckdb::duckdb_vector_get_data(vector) as *const i64).add(row as usize))
        }
        duckdb::DUCKDB_TYPE_UTINYINT => {
            J::from(*(duckdb::duckdb_vector_get_data(vector) as *const u8).add(row as usize))
        }
        duckdb::DUCKDB_TYPE_USMALLINT => {
            J::from(*(duckdb::duckdb_vector_get_data(vector) as *const u16).add(row as usize))
        }
        duckdb::DUCKDB_TYPE_UINTEGER => {
            J::from(*(duckdb::duckdb_vector_get_data(vector) as *const u32).add(row as usize))
        }
        duckdb::DUCKDB_TYPE_UBIGINT => {
            J::from(*(duckdb::duckdb_vector_get_data(vector) as *const u64).add(row as usize))
        }
        duckdb::DUCKDB_TYPE_FLOAT => {
            J::from(*(duckdb::duckdb_vector_get_data(vector) as *const f32).add(row as usize))
        }
        duckdb::DUCKDB_TYPE_DOUBLE => {
            J::from(*(duckdb::duckdb_vector_get_data(vector) as *const f64).add(row as usize))
        }
        duckdb::DUCKDB_TYPE_VARCHAR => {
            let data = duckdb::duckdb_vector_get_data(vector) as *mut duckdb::duckdb_string_t;
            let bytes = duckdb_string_to_vec(ptr::read(data.add(row as usize)));
            J::String(String::from_utf8_lossy(&bytes).into_owned())
        }
        _ => J::Null,
    }
}

unsafe fn write_duckvalue_to_vector(
    vector: duckdb::duckdb_vector,
    logical: &Logicaltype,
    row: duckdb::idx_t,
    value: Duckvalue,
) -> Result<(), Duckerror> {
    let validity = duckdb::duckdb_vector_get_validity(vector);
    match value {
        Duckvalue::Null => {
            // DuckDB allocates the validity mask lazily; a result vector that
            // has only seen valid rows has a NULL mask, and
            // duckdb_validity_set_row_invalid on it is a silent no-op (the row
            // stays "valid" over uninitialized data -> garbage blob/int/text).
            // Force the mask to exist, then re-fetch and mark the row invalid.
            duckdb::duckdb_vector_ensure_validity_writable(vector);
            let validity = duckdb::duckdb_vector_get_validity(vector);
            duckdb::duckdb_validity_set_row_invalid(validity, row);
            Ok(())
        }
        Duckvalue::Boolean(v) => {
            if !matches!(logical, Logicaltype::Boolean) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected boolean result, got {}",
                    duckvalue_kind(&Duckvalue::Boolean(v))
                )));
            }
            let data = duckdb::duckdb_vector_get_data(vector) as *mut bool;
            *data.add(row as usize) = v;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Int64(v) => {
            if !matches!(logical, Logicaltype::Int64) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected int64 result, got {}",
                    duckvalue_kind(&Duckvalue::Int64(v))
                )));
            }
            let data = duckdb::duckdb_vector_get_data(vector) as *mut i64;
            *data.add(row as usize) = v;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Uint64(v) => {
            if !matches!(logical, Logicaltype::Uint64) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected uint64 result, got {}",
                    duckvalue_kind(&Duckvalue::Uint64(v))
                )));
            }
            let data = duckdb::duckdb_vector_get_data(vector) as *mut u64;
            *data.add(row as usize) = v;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Float64(v) => {
            if !matches!(logical, Logicaltype::Float64) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected float64 result, got {}",
                    duckvalue_kind(&Duckvalue::Float64(v))
                )));
            }
            let data = duckdb::duckdb_vector_get_data(vector) as *mut f64;
            *data.add(row as usize) = v;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Text(text) => {
            if !matches!(logical, Logicaltype::Text) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected text result, got {}",
                    duckvalue_kind(&Duckvalue::Text(text.clone()))
                )));
            }
            let bytes = text.into_bytes();
            duckdb::duckdb_vector_assign_string_element_len(
                vector,
                row,
                bytes.as_ptr() as *const c_char,
                bytes.len() as duckdb::idx_t,
            );
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Blob(blob) => {
            if !matches!(logical, Logicaltype::Blob) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected blob result, got {}",
                    duckvalue_kind(&Duckvalue::Blob(blob.clone()))
                )));
            }
            duckdb::duckdb_vector_assign_string_element_len(
                vector,
                row,
                blob.as_ptr() as *const c_char,
                blob.len() as duckdb::idx_t,
            );
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Int32(v) => {
            if !matches!(logical, Logicaltype::Int32) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected int32 result, got {}",
                    duckvalue_kind(&Duckvalue::Int32(v))
                )));
            }
            let data = duckdb::duckdb_vector_get_data(vector) as *mut i32;
            *data.add(row as usize) = v;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Timestamp(micros) => {
            if !matches!(logical, Logicaltype::Timestamp) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected timestamp result, got {}",
                    duckvalue_kind(&Duckvalue::Timestamp(micros))
                )));
            }
            // DUCKDB_TYPE_TIMESTAMP is physically an int64 of micros since 1970.
            let data = duckdb::duckdb_vector_get_data(vector) as *mut i64;
            *data.add(row as usize) = micros;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Int8(v) => {
            if !matches!(logical, Logicaltype::Int8) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected int8 result, got {}",
                    duckvalue_kind(&Duckvalue::Int8(v))
                )));
            }
            let data = duckdb::duckdb_vector_get_data(vector) as *mut i8;
            *data.add(row as usize) = v;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Int16(v) => {
            if !matches!(logical, Logicaltype::Int16) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected int16 result, got {}",
                    duckvalue_kind(&Duckvalue::Int16(v))
                )));
            }
            let data = duckdb::duckdb_vector_get_data(vector) as *mut i16;
            *data.add(row as usize) = v;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Uint8(v) => {
            if !matches!(logical, Logicaltype::Uint8) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected uint8 result, got {}",
                    duckvalue_kind(&Duckvalue::Uint8(v))
                )));
            }
            let data = duckdb::duckdb_vector_get_data(vector) as *mut u8;
            *data.add(row as usize) = v;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Uint16(v) => {
            if !matches!(logical, Logicaltype::Uint16) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected uint16 result, got {}",
                    duckvalue_kind(&Duckvalue::Uint16(v))
                )));
            }
            let data = duckdb::duckdb_vector_get_data(vector) as *mut u16;
            *data.add(row as usize) = v;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Uint32(v) => {
            if !matches!(logical, Logicaltype::Uint32) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected uint32 result, got {}",
                    duckvalue_kind(&Duckvalue::Uint32(v))
                )));
            }
            let data = duckdb::duckdb_vector_get_data(vector) as *mut u32;
            *data.add(row as usize) = v;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Float32(v) => {
            if !matches!(logical, Logicaltype::Float32) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected float32 result, got {}",
                    duckvalue_kind(&Duckvalue::Float32(v))
                )));
            }
            let data = duckdb::duckdb_vector_get_data(vector) as *mut f32;
            *data.add(row as usize) = v;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Date(days) => {
            if !matches!(logical, Logicaltype::Date) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected date result, got {}",
                    duckvalue_kind(&Duckvalue::Date(days))
                )));
            }
            // DUCKDB_TYPE_DATE is physically an int32 of days since 1970-01-01.
            let data = duckdb::duckdb_vector_get_data(vector) as *mut i32;
            *data.add(row as usize) = days;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Time(micros) => {
            if !matches!(logical, Logicaltype::Time) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected time result, got {}",
                    duckvalue_kind(&Duckvalue::Time(micros))
                )));
            }
            // DUCKDB_TYPE_TIME is physically an int64 of micros since midnight.
            let data = duckdb::duckdb_vector_get_data(vector) as *mut i64;
            *data.add(row as usize) = micros;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Timestamptz(micros) => {
            if !matches!(logical, Logicaltype::Timestamptz) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected timestamptz result, got {}",
                    duckvalue_kind(&Duckvalue::Timestamptz(micros))
                )));
            }
            // DUCKDB_TYPE_TIMESTAMP_TZ is physically an int64 of micros since 1970 (UTC).
            let data = duckdb::duckdb_vector_get_data(vector) as *mut i64;
            *data.add(row as usize) = micros;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Decimal(d) => {
            if !matches!(logical, Logicaltype::Decimal) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected decimal result, got {}",
                    duckvalue_kind(&Duckvalue::Decimal(d))
                )));
            }
            // The result vector is decimal(38, S); its physical storage is an
            // int128 of the unscaled value = (upper << 64 | lower).
            let raw = ((d.upper as u128) << 64) | d.lower as u128;
            let data = duckdb::duckdb_vector_get_data(vector) as *mut i128;
            *data.add(row as usize) = raw as i128;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Interval(iv) => {
            if !matches!(logical, Logicaltype::Interval) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected interval result, got {}",
                    duckvalue_kind(&Duckvalue::Interval(iv))
                )));
            }
            // DUCKDB_TYPE_INTERVAL is physically {months:i32, days:i32, micros:i64}.
            let data = duckdb::duckdb_vector_get_data(vector) as *mut duckdb::duckdb_interval;
            *data.add(row as usize) = duckdb::duckdb_interval {
                months: iv.months,
                days: iv.days,
                micros: iv.micros,
            };
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Uuid(u) => {
            if !matches!(logical, Logicaltype::Uuid) {
                return Err(Duckerror::Invalidargument(format!(
                    "expected uuid result, got {}",
                    duckvalue_kind(&Duckvalue::Uuid(u))
                )));
            }
            // DUCKDB_TYPE_UUID is physically a hugeint with the high bit flipped.
            // Flip the logical UUID's MSB to produce the physical storage value.
            let logical_uuid = ((u.hi as u128) << 64) | u.lo as u128;
            let physical = logical_uuid ^ (1u128 << 127);
            let data = duckdb::duckdb_vector_get_data(vector) as *mut duckdb::duckdb_hugeint;
            *data.add(row as usize) = duckdb::duckdb_hugeint {
                lower: physical as u64,
                upper: (physical >> 64) as i64,
            };
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Complex(c) => {
            // ESCAPE-HATCH: reconstruct a real LIST/STRUCT vector value from JSON.
            // The result vector's column type IS the resolved nested type (the
            // scalar declared `complex("<type-expr>")`), so introspect it directly.
            let json: serde_json::Value = serde_json::from_str(&c.json).map_err(|e| {
                Duckerror::Invalidargument(format!(
                    "complex value json parse failed for '{}': {e}",
                    c.type_expr
                ))
            })?;
            // Up-front depth gate (the recursive writer also enforces this, but
            // reject an over-deep payload BEFORE touching the vector so a
            // pathological component input never half-writes a row).
            if !complex_depth_within_cap(&json, COMPLEX_MAX_DEPTH) {
                return Err(Duckerror::Invalidargument(format!(
                    "complex value json for '{}' exceeds max nesting depth {COMPLEX_MAX_DEPTH}",
                    c.type_expr
                )));
            }
            let col_type = duckdb::duckdb_vector_get_column_type(vector);
            let res = write_json_to_vector(vector, col_type, row, &json);
            let mut col_type_mut = col_type;
            duckdb::duckdb_destroy_logical_type(&mut col_type_mut);
            res?;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
    }
}

/// Write a `serde_json::Value` into a single row of a duckdb vector whose logical
/// type is `col_type`. Recurses into LIST child vectors and STRUCT field vectors
/// using the duckdb C vector API (no WIT recursion involved). The recursion lives
/// entirely in Rust/C, so arbitrary nesting depth is supported.
unsafe fn write_json_to_vector(
    vector: duckdb::duckdb_vector,
    col_type: duckdb::duckdb_logical_type,
    row: duckdb::idx_t,
    json: &serde_json::Value,
) -> Result<(), Duckerror> {
    write_json_to_vector_depth(vector, col_type, row, json, 0)
}

unsafe fn write_json_to_vector_depth(
    vector: duckdb::duckdb_vector,
    col_type: duckdb::duckdb_logical_type,
    row: duckdb::idx_t,
    json: &serde_json::Value,
    depth: u32,
) -> Result<(), Duckerror> {
    // Trust boundary: a component-supplied JSON / type-expr can be arbitrarily
    // deeply nested. Cap the recursion so a `[[[[...]]]]` payload errors instead
    // of overflowing the wasm stack (which aborts the whole instance).
    if depth > COMPLEX_MAX_DEPTH {
        return Err(Duckerror::Invalidargument(format!(
            "complex JSON nesting exceeds max depth {COMPLEX_MAX_DEPTH}"
        )));
    }
    let type_id = duckdb::duckdb_get_type_id(col_type);

    // JSON null -> mark the row invalid.
    if json.is_null() {
        duckdb::duckdb_vector_ensure_validity_writable(vector);
        let validity = duckdb::duckdb_vector_get_validity(vector);
        duckdb::duckdb_validity_set_row_invalid(validity, row);
        return Ok(());
    }

    match type_id {
        duckdb::DUCKDB_TYPE_LIST => {
            // Type-vs-JSON mismatch guard: the resolved type says LIST, so the
            // component's JSON for this row must be an array (or null, handled
            // above). A number/object/string here is an adversarial/buggy
            // payload -> error, never an out-of-shape access.
            if !json_matches_complex_kind(json, ComplexKind::List) {
                return Err(Duckerror::Invalidargument(format!(
                    "complex type is LIST but JSON value is {}",
                    json_value_kind(json)
                )));
            }
            let arr = json.as_array().ok_or_else(|| {
                Duckerror::Invalidargument("expected JSON array for LIST".to_string())
            })?;
            // Cap the per-row element count before it reaches
            // duckdb_list_vector_reserve (a giant length would request a huge
            // allocation and could OOM-abort the instance).
            if !complex_list_len_within_cap(arr.len(), COMPLEX_MAX_LIST_LEN) {
                return Err(Duckerror::Invalidargument(format!(
                    "complex LIST row length {} exceeds max {COMPLEX_MAX_LIST_LEN}",
                    arr.len()
                )));
            }
            let child_type = duckdb::duckdb_list_type_child_type(col_type);
            // Append this row's elements to the child vector at the running tail.
            // Reserve/grow BEFORE fetching the child pointer, since a reserve can
            // reallocate the child's backing storage.
            let start = duckdb::duckdb_list_vector_get_size(vector);
            let new_size = start + arr.len() as duckdb::idx_t;
            duckdb::duckdb_list_vector_reserve(vector, new_size);
            duckdb::duckdb_list_vector_set_size(vector, new_size);
            let child = duckdb::duckdb_list_vector_get_child(vector);
            for (i, elem) in arr.iter().enumerate() {
                let child_row = start + i as duckdb::idx_t;
                let r = write_json_to_vector_depth(child, child_type, child_row, elem, depth + 1);
                if r.is_err() {
                    let mut ct = child_type;
                    duckdb::duckdb_destroy_logical_type(&mut ct);
                    return r;
                }
            }
            let mut ct = child_type;
            duckdb::duckdb_destroy_logical_type(&mut ct);
            // Set this row's (offset, length) window into the child vector.
            let entries = duckdb::duckdb_vector_get_data(vector) as *mut duckdb::duckdb_list_entry;
            *entries.add(row as usize) = duckdb::duckdb_list_entry {
                offset: start,
                length: arr.len() as u64,
            };
            let validity = duckdb::duckdb_vector_get_validity(vector);
            if !validity.is_null() {
                duckdb::duckdb_validity_set_row_valid(validity, row);
            }
            Ok(())
        }
        duckdb::DUCKDB_TYPE_STRUCT => {
            // Type-vs-JSON mismatch guard (see the LIST arm): a STRUCT type
            // requires a JSON object. Missing fields are tolerated below
            // (defaulted to null); a non-object value is rejected here.
            if !json_matches_complex_kind(json, ComplexKind::Struct) {
                return Err(Duckerror::Invalidargument(format!(
                    "complex type is STRUCT but JSON value is {}",
                    json_value_kind(json)
                )));
            }
            let obj = json.as_object().ok_or_else(|| {
                Duckerror::Invalidargument("expected JSON object for STRUCT".to_string())
            })?;
            let field_count = duckdb::duckdb_struct_type_child_count(col_type);
            for i in 0..field_count {
                let name_ptr = duckdb::duckdb_struct_type_child_name(col_type, i);
                let field_name = CStr::from_ptr(name_ptr).to_string_lossy().into_owned();
                duckdb::duckdb_free(name_ptr as *mut c_void);
                let field_child = duckdb::duckdb_struct_vector_get_child(vector, i);
                let field_type = duckdb::duckdb_struct_type_child_type(col_type, i);
                let field_json = obj.get(&field_name).cloned().unwrap_or(serde_json::Value::Null);
                let r =
                    write_json_to_vector_depth(field_child, field_type, row, &field_json, depth + 1);
                let mut ft = field_type;
                duckdb::duckdb_destroy_logical_type(&mut ft);
                r?;
            }
            let validity = duckdb::duckdb_vector_get_validity(vector);
            if !validity.is_null() {
                duckdb::duckdb_validity_set_row_valid(validity, row);
            }
            Ok(())
        }
        // Scalar leaves: write the JSON primitive into the typed physical slot.
        _ => write_json_scalar_to_vector(vector, type_id, row, json),
    }
}

/// Write a JSON primitive into a scalar vector slot of physical `type_id`.
unsafe fn write_json_scalar_to_vector(
    vector: duckdb::duckdb_vector,
    type_id: duckdb::duckdb_type,
    row: duckdb::idx_t,
    json: &serde_json::Value,
) -> Result<(), Duckerror> {
    let validity = duckdb::duckdb_vector_get_validity(vector);
    macro_rules! set_valid {
        () => {
            if !validity.is_null() {
                duckdb::duckdb_validity_set_row_valid(validity, row);
            }
        };
    }
    match type_id {
        duckdb::DUCKDB_TYPE_BOOLEAN => {
            let v = json.as_bool().unwrap_or(false);
            (duckdb::duckdb_vector_get_data(vector) as *mut bool).add(row as usize).write(v);
        }
        duckdb::DUCKDB_TYPE_TINYINT => {
            let v = json.as_i64().unwrap_or(0) as i8;
            (duckdb::duckdb_vector_get_data(vector) as *mut i8).add(row as usize).write(v);
        }
        duckdb::DUCKDB_TYPE_SMALLINT => {
            let v = json.as_i64().unwrap_or(0) as i16;
            (duckdb::duckdb_vector_get_data(vector) as *mut i16).add(row as usize).write(v);
        }
        duckdb::DUCKDB_TYPE_INTEGER => {
            let v = json.as_i64().unwrap_or(0) as i32;
            (duckdb::duckdb_vector_get_data(vector) as *mut i32).add(row as usize).write(v);
        }
        duckdb::DUCKDB_TYPE_BIGINT => {
            let v = json.as_i64().unwrap_or(0);
            (duckdb::duckdb_vector_get_data(vector) as *mut i64).add(row as usize).write(v);
        }
        duckdb::DUCKDB_TYPE_UTINYINT => {
            let v = json.as_u64().unwrap_or(0) as u8;
            (duckdb::duckdb_vector_get_data(vector) as *mut u8).add(row as usize).write(v);
        }
        duckdb::DUCKDB_TYPE_USMALLINT => {
            let v = json.as_u64().unwrap_or(0) as u16;
            (duckdb::duckdb_vector_get_data(vector) as *mut u16).add(row as usize).write(v);
        }
        duckdb::DUCKDB_TYPE_UINTEGER => {
            let v = json.as_u64().unwrap_or(0) as u32;
            (duckdb::duckdb_vector_get_data(vector) as *mut u32).add(row as usize).write(v);
        }
        duckdb::DUCKDB_TYPE_UBIGINT => {
            let v = json.as_u64().unwrap_or(0);
            (duckdb::duckdb_vector_get_data(vector) as *mut u64).add(row as usize).write(v);
        }
        duckdb::DUCKDB_TYPE_FLOAT => {
            let v = json.as_f64().unwrap_or(0.0) as f32;
            (duckdb::duckdb_vector_get_data(vector) as *mut f32).add(row as usize).write(v);
        }
        duckdb::DUCKDB_TYPE_DOUBLE => {
            let v = json.as_f64().unwrap_or(0.0);
            (duckdb::duckdb_vector_get_data(vector) as *mut f64).add(row as usize).write(v);
        }
        duckdb::DUCKDB_TYPE_VARCHAR | duckdb::DUCKDB_TYPE_BLOB => {
            let s = match json {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let c = CString::new(s).map_err(|_| {
                Duckerror::Invalidargument("complex string element had embedded NUL".to_string())
            })?;
            duckdb::duckdb_vector_assign_string_element(vector, row, c.as_ptr());
        }
        other => {
            return Err(Duckerror::Unsupported(format!(
                "complex JSON leaf type {other} not yet supported in escape-hatch writer"
            )));
        }
    }
    set_valid!();
    Ok(())
}

unsafe fn duckdb_string_to_vec(string: duckdb::duckdb_string_t) -> Vec<u8> {
    let mut value = string;
    let len = duckdb::duckdb_string_t_length(ptr::read(&value)) as usize;
    let data_ptr = duckdb::duckdb_string_t_data(&mut value as *mut duckdb::duckdb_string_t);
    let slice = slice::from_raw_parts(data_ptr as *const u8, len);
    slice.to_vec()
}

fn duckvalue_kind(value: &Duckvalue) -> &'static str {
    match value {
        Duckvalue::Null => "null",
        Duckvalue::Boolean(_) => "boolean",
        Duckvalue::Int64(_) => "int64",
        Duckvalue::Uint64(_) => "uint64",
        Duckvalue::Float64(_) => "float64",
        Duckvalue::Text(_) => "text",
        Duckvalue::Blob(_) => "blob",
        Duckvalue::Int32(_) => "int32",
        Duckvalue::Timestamp(_) => "timestamp",
        Duckvalue::Int8(_) => "int8",
        Duckvalue::Int16(_) => "int16",
        Duckvalue::Uint8(_) => "uint8",
        Duckvalue::Uint16(_) => "uint16",
        Duckvalue::Uint32(_) => "uint32",
        Duckvalue::Float32(_) => "float32",
        Duckvalue::Date(_) => "date",
        Duckvalue::Time(_) => "time",
        Duckvalue::Timestamptz(_) => "timestamptz",
        Duckvalue::Decimal(_) => "decimal",
        Duckvalue::Interval(_) => "interval",
        Duckvalue::Uuid(_) => "uuid",
        Duckvalue::Complex(_) => "complex",
    }
}

fn format_duckerror(err: &Duckerror) -> String {
    match err {
        Duckerror::Invalidargument(msg) => format!("invalid argument: {msg}"),
        Duckerror::Unsupported(msg) => format!("unsupported: {msg}"),
        Duckerror::Invalidstate(msg) => format!("invalid state: {msg}"),
        Duckerror::Io(msg) => format!("i/o error: {msg}"),
        Duckerror::Internal(msg) => format!("internal error: {msg}"),
    }
}

fn sanitize_error_message(msg: &str) -> Result<CString, std::ffi::NulError> {
    let cleaned = msg.replace('\0', " ");
    CString::new(cleaned)
}

unsafe fn duckdb_value_to_duckvalue(
    value: duckdb::duckdb_value,
    logical: &Logicaltype,
) -> Result<Duckvalue, Duckerror> {
    if duckdb::duckdb_is_null_value(value) {
        return Ok(Duckvalue::Null);
    }

    let result = match logical {
        Logicaltype::Boolean => Duckvalue::Boolean(duckdb::duckdb_get_bool(value)),
        Logicaltype::Int64 => Duckvalue::Int64(duckdb::duckdb_get_int64(value)),
        Logicaltype::Int32 => Duckvalue::Int32(duckdb::duckdb_get_int32(value)),
        Logicaltype::Uint64 => Duckvalue::Uint64(duckdb::duckdb_get_uint64(value)),
        Logicaltype::Float64 => Duckvalue::Float64(duckdb::duckdb_get_double(value)),
        Logicaltype::Timestamp => Duckvalue::Timestamp(duckdb::duckdb_get_timestamp(value).micros),
        Logicaltype::Int8 => Duckvalue::Int8(duckdb::duckdb_get_int8(value)),
        Logicaltype::Int16 => Duckvalue::Int16(duckdb::duckdb_get_int16(value)),
        Logicaltype::Uint8 => Duckvalue::Uint8(duckdb::duckdb_get_uint8(value)),
        Logicaltype::Uint16 => Duckvalue::Uint16(duckdb::duckdb_get_uint16(value)),
        Logicaltype::Uint32 => Duckvalue::Uint32(duckdb::duckdb_get_uint32(value)),
        Logicaltype::Float32 => Duckvalue::Float32(duckdb::duckdb_get_float(value)),
        Logicaltype::Date => Duckvalue::Date(duckdb::duckdb_get_date(value).days),
        Logicaltype::Time => Duckvalue::Time(duckdb::duckdb_get_time(value).micros),
        Logicaltype::Timestamptz => {
            Duckvalue::Timestamptz(duckdb::duckdb_get_timestamp_tz(value).micros)
        }
        Logicaltype::Decimal => {
            let d = duckdb::duckdb_get_decimal(value);
            Duckvalue::Decimal(Decimalvalue {
                lower: d.value.lower,
                upper: d.value.upper as u64,
                width: d.width,
                scale: d.scale,
            })
        }
        Logicaltype::Interval => {
            let iv = duckdb::duckdb_get_interval(value);
            Duckvalue::Interval(Intervalvalue {
                months: iv.months,
                days: iv.days,
                micros: iv.micros,
            })
        }
        Logicaltype::Uuid => {
            // UUID is hugeint-backed; the value API returns the physical hugeint
            // (high bit flipped). Flip it back to the logical 128-bit UUID.
            let hh = duckdb::duckdb_get_hugeint(value);
            let physical = ((hh.upper as u128) << 64) | hh.lower as u128;
            let logical_uuid = physical ^ (1u128 << 127);
            Duckvalue::Uuid(Uuidvalue {
                hi: (logical_uuid >> 64) as u64,
                lo: logical_uuid as u64,
            })
        }
        Logicaltype::Text => {
            let ptr = duckdb::duckdb_get_varchar(value);
            if ptr.is_null() {
                Duckvalue::Text(String::new())
            } else {
                let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
                duckdb::duckdb_free(ptr as *mut c_void);
                Duckvalue::Text(s)
            }
        }
        Logicaltype::Blob => {
            let blob = duckdb::duckdb_get_blob(value);
            if blob.data.is_null() || blob.size == 0 {
                Duckvalue::Blob(Vec::new())
            } else {
                let slice = slice::from_raw_parts(blob.data as *const u8, blob.size as usize);
                let mut vec = Vec::with_capacity(slice.len());
                vec.extend_from_slice(slice);
                duckdb::duckdb_free(blob.data);
                Duckvalue::Blob(vec)
            }
        }
        Logicaltype::Complex(type_expr) => {
            // ESCAPE-HATCH value-param read: there is no scalar duckdb_value API to
            // walk a nested LIST/STRUCT here (the vector-based read in
            // read_scalar_argument handles the exercised path). Carry the declared
            // type with a JSON null so the boundary stays total.
            Duckvalue::Complex(Complexvalue {
                type_expr: type_expr.clone(),
                json: "null".to_string(),
            })
        }
    };

    Ok(result)
}

// The `__cxa_*` exception ABI is now provided by the real exception-handling
// libc++abi baked into libduckdb-wasi.a (wasi-sdk-33 `eh` multilib). The
// previous abort-stubs here only existed because the old no-exceptions libc++
// lacked them; defining them now would clash with the real runtime.

fn extract_and_free_c_string(ptr: *mut std::os::raw::c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }

    unsafe {
        let message = CStr::from_ptr(ptr).to_string_lossy().into_owned();
        duckdb::duckdb_free(ptr.cast());
        Some(message)
    }
}

fn extract_last_error(_conn: duckdb::duckdb_connection) -> Option<String> {
    None
}

fn extract_result_error(result: &duckdb::duckdb_result) -> Option<String> {
    unsafe {
        let ptr = duckdb::duckdb_result_error(result as *const _ as *mut _);
        if ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn _ZN6duckdb8HTTPUtil8ShutdownEv() {}

unsafe fn marshal_result(
    result: &duckdb::duckdb_result,
) -> Result<(Vec<Columndef>, Vec<Row>), DuckDbError> {
    let result_mut = result as *const _ as *mut duckdb::duckdb_result;
    let column_count = duckdb::duckdb_column_count(result_mut);
    let row_count = duckdb::duckdb_row_count(result_mut);

    // Resolve each column's DuckDB type once so values come back as the matching
    // duckvalue variant (numbers/booleans typed instead of stringified).
    let mut columns = Vec::with_capacity(column_count as usize);
    let mut type_ids = Vec::with_capacity(column_count as usize);
    for idx in 0..column_count {
        let name_ptr = duckdb::duckdb_column_name(result_mut, idx);
        let name = if name_ptr.is_null() {
            format!("column{}", idx)
        } else {
            CStr::from_ptr(name_ptr).to_string_lossy().into_owned()
        };
        let mut logical_type = duckdb::duckdb_column_logical_type(result_mut, idx);
        let type_id = duckdb::duckdb_get_type_id(logical_type);
        duckdb::duckdb_destroy_logical_type(&mut logical_type);
        type_ids.push(type_id);
        columns.push(Columndef {
            name,
            logical: marshal_logical_for_type(type_id),
        });
    }

    let mut rows = Vec::with_capacity(row_count as usize);
    for row_idx in 0..row_count {
        let mut row = Vec::with_capacity(column_count as usize);
        for col_idx in 0..column_count {
            if duckdb::duckdb_value_is_null(result_mut, col_idx, row_idx) {
                row.push(Duckvalue::Null);
                continue;
            }
            // BLOB columns must be read as raw bytes -- duckdb_value_varchar
            // would render them as DuckDB's "\xNN" escape text and corrupt
            // binary payloads (e.g. the registry site's .wasm/.png assets).
            if type_ids[col_idx as usize] == duckdb::DUCKDB_TYPE_BLOB {
                let blob = duckdb::duckdb_value_blob(result_mut, col_idx, row_idx);
                let bytes = if blob.data.is_null() || blob.size == 0 {
                    Vec::new()
                } else {
                    let slice =
                        std::slice::from_raw_parts(blob.data as *const u8, blob.size as usize);
                    let owned = slice.to_vec();
                    duckdb::duckdb_free(blob.data);
                    owned
                };
                row.push(Duckvalue::Blob(bytes));
                continue;
            }
            let value_ptr = duckdb::duckdb_value_varchar(result_mut, col_idx, row_idx);
            let text = if value_ptr.is_null() {
                String::new()
            } else {
                let value = CStr::from_ptr(value_ptr).to_string_lossy().into_owned();
                duckdb::duckdb_free(value_ptr.cast());
                value
            };
            row.push(marshal_value_for_type(type_ids[col_idx as usize], text));
        }
        rows.push(row);
    }

    Ok((columns, rows))
}

/// Maps a DuckDB column type to the `duckvalue` variant used for its values.
/// Numeric and boolean types map to typed variants; everything else (VARCHAR,
/// BLOB, DATE/TIMESTAMP/DECIMAL/LIST/STRUCT, ...) renders as text.
fn marshal_logical_for_type(type_id: duckdb::duckdb_type) -> Logicaltype {
    match type_id {
        duckdb::DUCKDB_TYPE_BOOLEAN => Logicaltype::Boolean,
        duckdb::DUCKDB_TYPE_TINYINT
        | duckdb::DUCKDB_TYPE_SMALLINT
        | duckdb::DUCKDB_TYPE_INTEGER
        | duckdb::DUCKDB_TYPE_BIGINT => Logicaltype::Int64,
        duckdb::DUCKDB_TYPE_UTINYINT
        | duckdb::DUCKDB_TYPE_USMALLINT
        | duckdb::DUCKDB_TYPE_UINTEGER
        | duckdb::DUCKDB_TYPE_UBIGINT => Logicaltype::Uint64,
        duckdb::DUCKDB_TYPE_FLOAT | duckdb::DUCKDB_TYPE_DOUBLE => Logicaltype::Float64,
        duckdb::DUCKDB_TYPE_BLOB => Logicaltype::Blob,
        _ => Logicaltype::Text,
    }
}

/// Parses DuckDB's string rendering of a cell into the typed `duckvalue` variant
/// for its column type. Falls back to text if the value does not parse (e.g.
/// non-finite floats render as "inf"/"nan").
fn marshal_value_for_type(type_id: duckdb::duckdb_type, text: String) -> Duckvalue {
    match type_id {
        duckdb::DUCKDB_TYPE_BOOLEAN => Duckvalue::Boolean(text == "true"),
        duckdb::DUCKDB_TYPE_TINYINT
        | duckdb::DUCKDB_TYPE_SMALLINT
        | duckdb::DUCKDB_TYPE_INTEGER
        | duckdb::DUCKDB_TYPE_BIGINT => match text.parse::<i64>() {
            Ok(value) => Duckvalue::Int64(value),
            Err(_) => Duckvalue::Text(text),
        },
        duckdb::DUCKDB_TYPE_UTINYINT
        | duckdb::DUCKDB_TYPE_USMALLINT
        | duckdb::DUCKDB_TYPE_UINTEGER
        | duckdb::DUCKDB_TYPE_UBIGINT => match text.parse::<u64>() {
            Ok(value) => Duckvalue::Uint64(value),
            Err(_) => Duckvalue::Text(text),
        },
        duckdb::DUCKDB_TYPE_FLOAT | duckdb::DUCKDB_TYPE_DOUBLE => match text.parse::<f64>() {
            Ok(value) => Duckvalue::Float64(value),
            Err(_) => Duckvalue::Text(text),
        },
        _ => Duckvalue::Text(text),
    }
}
