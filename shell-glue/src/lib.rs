//! Route A install glue: the C entry the merged libduckdb-wasi.a calls on `LOAD`
//! (`duckdb_component_load_extension`) wired, via wit-bindgen, to the host's
//! componentized-extension surface, and the scalar-registration trampoline that
//! installs the loaded extension's scalar functions onto the SHELL's own DuckDB
//! connection. Linked into the real DuckDB shell command by the clang driver in
//! scripts/build-shell-ext-wasm.sh.
//!
//! The scalar marshalling + registration mirror core/src/lib.rs (the proven
//! composed-core path). The one shell-specific addition is the db-handle bridge:
//! a tiny C++ hook in the shell's OpenDB path hands a dedicated sibling
//! `duckdb::Connection*` (cast to a C-API duckdb_connection) to
//! `duckdb_shell_ext_set_connection`; scalars register on that connection (a
//! fresh one, never the LOAD-busy connection) and land in the database catalog,
//! visible to the shell's primary connection.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicPtr, Ordering};

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "shell-glue",
        generate_all,
    });
}

use bindings::duckdb::component::{extension_loader_hooks, host_extension_loader};
use bindings::duckdb::extension::callback_dispatch;
use bindings::duckdb::extension::types::{Duckerror, Duckvalue, Logicaltype};
use libduckdb_sys as duckdb;

const DUCKDB_SUCCESS: duckdb::duckdb_state = 0;

/// The dedicated registration connection handed in by the C++ db-handle bridge
/// (a sibling `duckdb::Connection*` reinterpreted as a C-API duckdb_connection).
static REGISTRATION_CONN: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

fn clog(msg: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{msg}");
}

/// Called by the shell's OpenDB hook (shell_ext_bridge.cpp) with a sibling
/// connection on the shell's database. Scalars loaded via component extensions
/// are registered on this connection.
#[no_mangle]
pub unsafe extern "C" fn duckdb_shell_ext_set_connection(conn: *mut c_void) {
    REGISTRATION_CONN.store(conn, Ordering::SeqCst);
    clog("[shell-glue] registration connection set");
}

/// The C entry `LoadExternalExtensionInternal` calls for `LOAD <name>` (the
/// merged archive is built with DUCKDB_DISABLE_EXTENSION_LOAD, routing here).
#[no_mangle]
pub unsafe extern "C" fn duckdb_component_load_extension(name: *const c_char) -> bool {
    if name.is_null() {
        return false;
    }
    let extension_name = CStr::from_ptr(name).to_string_lossy().into_owned();
    clog(&format!("[shell-glue] requesting host load for '{extension_name}'"));

    if !host_extension_loader::request_load(&extension_name) {
        clog(&format!(
            "[shell-glue] host declined extension '{extension_name}'"
        ));
        return false;
    }

    let pending = extension_loader_hooks::get_pending_registrations();
    clog(&format!(
        "[shell-glue] '{extension_name}' pending: scalars={}, tables={}, aggregates={}",
        pending.scalars.len(),
        pending.tables.len(),
        pending.aggregates.len()
    ));

    let mut ok = true;
    for entry in pending.scalars {
        if let Err(err) = register_pending_scalar(entry) {
            clog(&format!("[shell-glue] scalar registration failed: {err}"));
            ok = false;
        }
    }
    if !pending.tables.is_empty() || !pending.aggregates.is_empty() || !pending.macros.is_empty() {
        clog("[shell-glue] note: only scalar functions are wired in this glue");
    }
    ok
}

struct ScalarDef {
    name: String,
    arguments: Vec<Logicaltype>,
    returns: Logicaltype,
    callback_handle: u32,
}

fn convert_loader_logicaltype(logical: extension_loader_hooks::Logicaltype) -> Logicaltype {
    // extension-loader-hooks reuses duckdb:extension/types.logicaltype, so this is
    // a structural pass-through (kept explicit to match the core).
    match logical {
        Logicaltype::Boolean => Logicaltype::Boolean,
        Logicaltype::Int64 => Logicaltype::Int64,
        Logicaltype::Uint64 => Logicaltype::Uint64,
        Logicaltype::Float64 => Logicaltype::Float64,
        Logicaltype::Text => Logicaltype::Text,
        Logicaltype::Blob => Logicaltype::Blob,
        Logicaltype::Int32 => Logicaltype::Int32,
        Logicaltype::Timestamp => Logicaltype::Timestamp,
        Logicaltype::Int8 => Logicaltype::Int8,
        Logicaltype::Int16 => Logicaltype::Int16,
        Logicaltype::Uint8 => Logicaltype::Uint8,
        Logicaltype::Uint16 => Logicaltype::Uint16,
        Logicaltype::Uint32 => Logicaltype::Uint32,
        Logicaltype::Float32 => Logicaltype::Float32,
        Logicaltype::Date => Logicaltype::Date,
        Logicaltype::Time => Logicaltype::Time,
        Logicaltype::Timestamptz => Logicaltype::Timestamptz,
        Logicaltype::Decimal => Logicaltype::Decimal,
        Logicaltype::Interval => Logicaltype::Interval,
        Logicaltype::Uuid => Logicaltype::Uuid,
        Logicaltype::Complex(expr) => Logicaltype::Complex(expr),
    }
}

unsafe fn register_pending_scalar(
    entry: extension_loader_hooks::ScalarRegistration,
) -> Result<(), String> {
    let conn = REGISTRATION_CONN.load(Ordering::SeqCst) as duckdb::duckdb_connection;
    if conn.is_null() {
        return Err("no shell registration connection (db-handle bridge not set)".to_string());
    }
    let def = ScalarDef {
        name: entry.name,
        arguments: entry
            .arguments
            .into_iter()
            .map(|a| convert_loader_logicaltype(a.logical))
            .collect(),
        returns: convert_loader_logicaltype(entry.returns),
        callback_handle: entry.callback_handle,
    };
    clog(&format!(
        "[shell-glue] registering scalar '{}' (callback={}, argc={})",
        def.name,
        def.callback_handle,
        def.arguments.len()
    ));
    register_scalar_function_on_connection(conn, def)
}

unsafe fn register_scalar_function_on_connection(
    connection: duckdb::duckdb_connection,
    def: ScalarDef,
) -> Result<(), String> {
    let function = duckdb::duckdb_create_scalar_function();
    if function.is_null() {
        return Err("duckdb_create_scalar_function returned null".to_string());
    }

    let name_c = CString::new(def.name.as_str())
        .map_err(|_| "function name contains embedded null byte".to_string())?;
    duckdb::duckdb_scalar_function_set_name(function, name_c.as_ptr());

    for logical in &def.arguments {
        let mut logical_type = create_duckdb_logical_type(logical)?;
        duckdb::duckdb_scalar_function_add_parameter(function, logical_type);
        duckdb::duckdb_destroy_logical_type(&mut logical_type);
    }

    let mut return_type = create_duckdb_logical_type(&def.returns)?;
    duckdb::duckdb_scalar_function_set_return_type(function, return_type);
    duckdb::duckdb_destroy_logical_type(&mut return_type);

    duckdb::duckdb_scalar_function_set_function(function, Some(scalar_function_callback));

    let entry_ptr = Box::into_raw(Box::new(def)) as *mut c_void;
    duckdb::duckdb_scalar_function_set_extra_info(
        function,
        entry_ptr,
        Some(scalar_function_entry_destroy),
    );

    let state = duckdb::duckdb_register_scalar_function(connection, function);
    let mut function_mut = function;
    duckdb::duckdb_destroy_scalar_function(&mut function_mut);
    if state != DUCKDB_SUCCESS {
        return Err("duckdb_register_scalar_function failed".to_string());
    }
    Ok(())
}

unsafe extern "C" fn scalar_function_entry_destroy(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr as *mut ScalarDef));
    }
}

unsafe extern "C" fn scalar_function_callback(
    info: duckdb::duckdb_function_info,
    input: duckdb::duckdb_data_chunk,
    output: duckdb::duckdb_vector,
) {
    if let Err(err) = execute_scalar_function(info, input, output) {
        if let Ok(message) = CString::new(err.replace('\0', " ")) {
            duckdb::duckdb_scalar_function_set_error(info, message.as_ptr());
        }
    }
}

unsafe fn execute_scalar_function(
    info: duckdb::duckdb_function_info,
    input: duckdb::duckdb_data_chunk,
    output: duckdb::duckdb_vector,
) -> Result<(), String> {
    let entry_ptr = duckdb::duckdb_scalar_function_get_extra_info(info);
    if entry_ptr.is_null() {
        return Err("scalar function missing dispatcher entry".to_string());
    }
    let def = &*(entry_ptr as *const ScalarDef);

    let row_count = duckdb::duckdb_data_chunk_get_size(input);
    let mut columns = Vec::with_capacity(def.arguments.len());
    for (idx, logical) in def.arguments.iter().enumerate() {
        let vector = duckdb::duckdb_data_chunk_get_vector(input, idx as duckdb::idx_t);
        columns.push((vector, logical.clone()));
    }

    // One batched call-scalar-batch per DataChunk (not per row).
    let mut rows = Vec::with_capacity(row_count as usize);
    for row in 0..row_count {
        let mut args = Vec::with_capacity(columns.len());
        for (vector, logical) in &columns {
            args.push(read_scalar_argument(*vector, logical, row)?);
        }
        rows.push(args);
    }

    let invoke = callback_dispatch::Invokeinfo {
        rowindex: Some(0),
        iswindow: false,
    };
    let results = callback_dispatch::call_scalar_batch(def.callback_handle, rows.as_slice(), invoke)
        .map_err(|err| format_duckerror(&err))?;

    if results.len() as u64 != row_count {
        return Err(format!(
            "scalar batch returned {} values for {} rows",
            results.len(),
            row_count
        ));
    }
    for (row, result) in results.into_iter().enumerate() {
        write_duckvalue_to_vector(output, &def.returns, row as duckdb::idx_t, result)?;
    }
    Ok(())
}

unsafe fn create_duckdb_logical_type(
    logical: &Logicaltype,
) -> Result<duckdb::duckdb_logical_type, String> {
    let type_id = match logical {
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
        other => {
            return Err(format!(
                "shell-glue does not support logical type {other:?} yet"
            ))
        }
    };
    let ty = duckdb::duckdb_create_logical_type(type_id);
    if ty.is_null() {
        Err("duckdb_create_logical_type returned null".to_string())
    } else {
        Ok(ty)
    }
}

unsafe fn duckdb_string_to_vec(string: duckdb::duckdb_string_t) -> Vec<u8> {
    let mut value = string;
    let len = duckdb::duckdb_string_t_length(ptr::read(&value)) as usize;
    let data_ptr = duckdb::duckdb_string_t_data(&mut value as *mut duckdb::duckdb_string_t);
    slice::from_raw_parts(data_ptr as *const u8, len).to_vec()
}

unsafe fn read_scalar_argument(
    vector: duckdb::duckdb_vector,
    logical: &Logicaltype,
    row: duckdb::idx_t,
) -> Result<Duckvalue, String> {
    let validity = duckdb::duckdb_vector_get_validity(vector);
    let is_valid = validity.is_null() || duckdb::duckdb_validity_row_is_valid(validity, row);
    if !is_valid {
        return Ok(Duckvalue::Null);
    }
    let data = duckdb::duckdb_vector_get_data(vector);
    let v = match logical {
        Logicaltype::Boolean => Duckvalue::Boolean(*(data as *const bool).add(row as usize)),
        Logicaltype::Int64 => Duckvalue::Int64(*(data as *const i64).add(row as usize)),
        Logicaltype::Uint64 => Duckvalue::Uint64(*(data as *const u64).add(row as usize)),
        Logicaltype::Float64 => Duckvalue::Float64(*(data as *const f64).add(row as usize)),
        Logicaltype::Int32 => Duckvalue::Int32(*(data as *const i32).add(row as usize)),
        Logicaltype::Int8 => Duckvalue::Int8(*(data as *const i8).add(row as usize)),
        Logicaltype::Int16 => Duckvalue::Int16(*(data as *const i16).add(row as usize)),
        Logicaltype::Uint8 => Duckvalue::Uint8(*(data as *const u8).add(row as usize)),
        Logicaltype::Uint16 => Duckvalue::Uint16(*(data as *const u16).add(row as usize)),
        Logicaltype::Uint32 => Duckvalue::Uint32(*(data as *const u32).add(row as usize)),
        Logicaltype::Float32 => Duckvalue::Float32(*(data as *const f32).add(row as usize)),
        Logicaltype::Timestamp => Duckvalue::Timestamp(*(data as *const i64).add(row as usize)),
        Logicaltype::Timestamptz => Duckvalue::Timestamptz(*(data as *const i64).add(row as usize)),
        Logicaltype::Date => Duckvalue::Date(*(data as *const i32).add(row as usize)),
        Logicaltype::Time => Duckvalue::Time(*(data as *const i64).add(row as usize)),
        Logicaltype::Text => {
            let st = ptr::read((data as *const duckdb::duckdb_string_t).add(row as usize));
            let bytes = duckdb_string_to_vec(st);
            Duckvalue::Text(
                String::from_utf8(bytes).map_err(|_| "text arg not valid UTF-8".to_string())?,
            )
        }
        Logicaltype::Blob => {
            let st = ptr::read((data as *const duckdb::duckdb_string_t).add(row as usize));
            Duckvalue::Blob(duckdb_string_to_vec(st))
        }
        other => return Err(format!("shell-glue cannot read argument type {other:?}")),
    };
    Ok(v)
}

unsafe fn write_duckvalue_to_vector(
    vector: duckdb::duckdb_vector,
    logical: &Logicaltype,
    row: duckdb::idx_t,
    value: Duckvalue,
) -> Result<(), String> {
    let validity = duckdb::duckdb_vector_get_validity(vector);
    match value {
        Duckvalue::Null => {
            duckdb::duckdb_vector_ensure_validity_writable(vector);
            let validity = duckdb::duckdb_vector_get_validity(vector);
            duckdb::duckdb_validity_set_row_invalid(validity, row);
            Ok(())
        }
        Duckvalue::Boolean(x) => {
            if !matches!(logical, Logicaltype::Boolean) {
                return Err("expected boolean result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut bool).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Int64(x) => {
            if !matches!(logical, Logicaltype::Int64) {
                return Err("expected int64 result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut i64).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Uint64(x) => {
            if !matches!(logical, Logicaltype::Uint64) {
                return Err("expected uint64 result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut u64).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Float64(x) => {
            if !matches!(logical, Logicaltype::Float64) {
                return Err("expected float64 result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut f64).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Int32(x) => {
            if !matches!(logical, Logicaltype::Int32) {
                return Err("expected int32 result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut i32).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Int8(x) => {
            if !matches!(logical, Logicaltype::Int8) {
                return Err("expected int8 result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut i8).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Int16(x) => {
            if !matches!(logical, Logicaltype::Int16) {
                return Err("expected int16 result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut i16).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Uint8(x) => {
            if !matches!(logical, Logicaltype::Uint8) {
                return Err("expected uint8 result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut u8).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Uint16(x) => {
            if !matches!(logical, Logicaltype::Uint16) {
                return Err("expected uint16 result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut u16).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Uint32(x) => {
            if !matches!(logical, Logicaltype::Uint32) {
                return Err("expected uint32 result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut u32).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Float32(x) => {
            if !matches!(logical, Logicaltype::Float32) {
                return Err("expected float32 result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut f32).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Timestamp(x) | Duckvalue::Time(x) | Duckvalue::Timestamptz(x) => {
            if !matches!(
                logical,
                Logicaltype::Timestamp | Logicaltype::Time | Logicaltype::Timestamptz
            ) {
                return Err("expected temporal result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut i64).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Date(x) => {
            if !matches!(logical, Logicaltype::Date) {
                return Err("expected date result".to_string());
            }
            *(duckdb::duckdb_vector_get_data(vector) as *mut i32).add(row as usize) = x;
            duckdb::duckdb_validity_set_row_valid(validity, row);
            Ok(())
        }
        Duckvalue::Text(text) => {
            if !matches!(logical, Logicaltype::Text) {
                return Err("expected text result".to_string());
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
                return Err("expected blob result".to_string());
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
        other => Err(format!("shell-glue cannot write result {other:?}")),
    }
}

fn format_duckerror(err: &Duckerror) -> String {
    match err {
        Duckerror::Invalidargument(s) => format!("invalid argument: {s}"),
        Duckerror::Unsupported(s) => format!("unsupported: {s}"),
        Duckerror::Invalidstate(s) => format!("invalid state: {s}"),
        Duckerror::Io(s) => format!("io error: {s}"),
        Duckerror::Internal(s) => format!("internal error: {s}"),
    }
}
