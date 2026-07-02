/* noop_impl.c — implementations of every export in wit/world.wit.
 *
 * All 42 non-free/non-drop exports declared by wit-bindgen. Every entry is
 * a well-typed no-op:
 *   - void-returning: fills *ret with an empty list/none-option
 *   - bool-returning: zero-initializes *ret + returns true (success)
 *
 * Rationale for "success everywhere" rather than "trap everywhere": DuckDB
 * pokes at these interfaces during startup (extension registration probes,
 * pragma list, catalog init) even when no extension is loaded. A trap
 * aborts the whole component; empty-success lets DuckDB run its default
 * paths.
 *
 * Style matches ~/git/openssl-wasm/examples/noop-provider/src/noop_impl.c.
 */

#include "noop.h"
#include <string.h>

/* -----------------------------------------------------------------------
 * Small helper macros
 * ---------------------------------------------------------------------*/

#define ZERO(x) memset((x), 0, sizeof(*(x)))

/* Empty list — every generated list<T> uses {ptr, len} layout. */
#define EMPTY_LIST(ret) do { (ret)->ptr = NULL; (ret)->len = 0; } while (0)

/* Empty string — same {ptr, len} layout. */
#define EMPTY_STRING(s) do { (s)->ptr = NULL; (s)->len = 0; } while (0)

/* =======================================================================
 * duckdb:extension/callback-dispatch — 7 exports
 * All succeed with empty/neutral duckvalue.NULL or empty resultset.
 * =====================================================================*/

bool exports_duckdb_extension_callback_dispatch_call_scalar_batch_col(
    uint32_t handle,
    exports_duckdb_extension_callback_dispatch_list_colvec_t *args,
    exports_duckdb_extension_callback_dispatch_invokeinfo_t *ctx,
    exports_duckdb_extension_callback_dispatch_colvec_t *ret,
    exports_duckdb_extension_callback_dispatch_duckerror_t *err)
{
    (void)handle; (void)args; (void)ctx; (void)err;
    ZERO(ret);
    return true;
}

bool exports_duckdb_extension_callback_dispatch_call_aggregate_col(
    uint32_t handle,
    exports_duckdb_extension_callback_dispatch_list_colvec_t *args,
    exports_duckdb_extension_callback_dispatch_duckvalue_t *ret,
    exports_duckdb_extension_callback_dispatch_duckerror_t *err)
{
    (void)handle; (void)args; (void)err;
    ZERO(ret);
    /* Neutral duckvalue = NULL tag (0). */
    return true;
}

bool exports_duckdb_extension_callback_dispatch_call_cast_col(
    uint32_t handle,
    exports_duckdb_extension_callback_dispatch_colvec_t *arg,
    exports_duckdb_extension_callback_dispatch_colvec_t *ret,
    exports_duckdb_extension_callback_dispatch_duckerror_t *err)
{
    (void)handle; (void)arg; (void)err;
    ZERO(ret);
    return true;
}

bool exports_duckdb_extension_callback_dispatch_call_scalar(
    uint32_t handle,
    exports_duckdb_extension_callback_dispatch_list_duckvalue_t *args,
    exports_duckdb_extension_callback_dispatch_invokeinfo_t *ctx,
    exports_duckdb_extension_callback_dispatch_duckvalue_t *ret,
    exports_duckdb_extension_callback_dispatch_duckerror_t *err)
{
    (void)handle; (void)args; (void)ctx; (void)err;
    ZERO(ret);
    return true;
}

bool exports_duckdb_extension_callback_dispatch_call_table(
    uint32_t handle,
    exports_duckdb_extension_callback_dispatch_list_duckvalue_t *args,
    exports_duckdb_extension_callback_dispatch_resultset_t *ret,
    exports_duckdb_extension_callback_dispatch_duckerror_t *err)
{
    (void)handle; (void)args; (void)err;
    ZERO(ret);
    return true;
}

bool exports_duckdb_extension_callback_dispatch_call_pragma(
    uint32_t handle,
    exports_duckdb_extension_callback_dispatch_list_duckvalue_t *args,
    exports_duckdb_extension_callback_dispatch_option_duckvalue_t *ret,
    exports_duckdb_extension_callback_dispatch_duckerror_t *err)
{
    (void)handle; (void)args; (void)err;
    ret->is_some = false;
    ZERO(&ret->val);
    return true;
}

bool exports_duckdb_extension_callback_dispatch_call_cast(
    uint32_t handle,
    exports_duckdb_extension_callback_dispatch_duckvalue_t *value,
    exports_duckdb_extension_callback_dispatch_duckvalue_t *ret,
    exports_duckdb_extension_callback_dispatch_duckerror_t *err)
{
    (void)handle; (void)value; (void)err;
    ZERO(ret);
    return true;
}

/* =======================================================================
 * duckdb:extension/storage-host — 7 exports (1 void + 6 bool)
 * =====================================================================*/

void exports_duckdb_extension_storage_host_storage_list_types(
    noop_list_string_t *ret)
{
    EMPTY_LIST(ret);
}

bool exports_duckdb_extension_storage_host_storage_attach(
    noop_string_t *dsn, uint32_t *ret,
    exports_duckdb_extension_storage_host_duckerror_t *err)
{
    (void)dsn; (void)err;
    *ret = 0;
    return true;
}

bool exports_duckdb_extension_storage_host_storage_list_tables(
    uint32_t catalog, noop_list_string_t *ret,
    exports_duckdb_extension_storage_host_duckerror_t *err)
{
    (void)catalog; (void)err;
    EMPTY_LIST(ret);
    return true;
}

bool exports_duckdb_extension_storage_host_storage_table_columns(
    uint32_t catalog, noop_string_t *table,
    exports_duckdb_extension_storage_host_list_columndef_t *ret,
    exports_duckdb_extension_storage_host_duckerror_t *err)
{
    (void)catalog; (void)table; (void)err;
    EMPTY_LIST(ret);
    return true;
}

bool exports_duckdb_extension_storage_host_storage_scan_open(
    uint32_t catalog,
    exports_duckdb_extension_storage_host_scan_request_t *request,
    uint32_t *ret,
    exports_duckdb_extension_storage_host_duckerror_t *err)
{
    (void)catalog; (void)request; (void)err;
    *ret = 0;
    return true;
}

bool exports_duckdb_extension_storage_host_storage_scan_next(
    uint32_t scan, uint32_t max_rows,
    exports_duckdb_extension_storage_host_resultset_t *ret,
    exports_duckdb_extension_storage_host_duckerror_t *err)
{
    (void)scan; (void)max_rows; (void)err;
    ZERO(ret);
    return true;
}

bool exports_duckdb_extension_storage_host_storage_scan_close(
    uint32_t scan, bool *ret,
    exports_duckdb_extension_storage_host_duckerror_t *err)
{
    (void)scan; (void)err;
    *ret = true;
    return true;
}

/* =======================================================================
 * duckdb:extension/index-host — 6 exports (1 void + 5 bool)
 * =====================================================================*/

void exports_duckdb_extension_index_host_index_type_list(
    noop_list_string_t *ret)
{
    EMPTY_LIST(ret);
}

bool exports_duckdb_extension_index_host_index_create(
    noop_string_t *type_name, noop_string_t *index_name, uint32_t dims,
    uint32_t *ret, exports_duckdb_extension_index_host_duckerror_t *err)
{
    (void)type_name; (void)index_name; (void)dims; (void)err;
    *ret = 0;
    return true;
}

bool exports_duckdb_extension_index_host_index_append(
    uint32_t handle, noop_list_s64_t *rowids, noop_list_list_f32_t *vectors,
    exports_duckdb_extension_index_host_duckerror_t *err)
{
    (void)handle; (void)rowids; (void)vectors; (void)err;
    return true;
}

bool exports_duckdb_extension_index_host_index_build(
    uint32_t handle, exports_duckdb_extension_index_host_duckerror_t *err)
{
    (void)handle; (void)err;
    return true;
}

bool exports_duckdb_extension_index_host_index_search(
    uint32_t handle, noop_list_f32_t *query, uint32_t k,
    exports_duckdb_extension_index_host_list_index_hit_t *ret,
    exports_duckdb_extension_index_host_duckerror_t *err)
{
    (void)handle; (void)query; (void)k; (void)err;
    EMPTY_LIST(ret);
    return true;
}

bool exports_duckdb_extension_index_host_index_drop(
    uint32_t handle, exports_duckdb_extension_index_host_duckerror_t *err)
{
    (void)handle; (void)err;
    return true;
}

/* =======================================================================
 * duckdb:extension/collation-host / pragma-host / parser-host /
 * optimizer-host — the "list-and-dispatch" quartet.
 * =====================================================================*/

void exports_duckdb_extension_collation_host_collation_list(
    exports_duckdb_extension_collation_host_list_collation_spec_t *ret)
{
    EMPTY_LIST(ret);
}

void exports_duckdb_extension_pragma_host_pragma_list(
    exports_duckdb_extension_pragma_host_list_pragma_spec_t *ret)
{
    EMPTY_LIST(ret);
}

void exports_duckdb_extension_parser_host_parser_list(
    exports_duckdb_extension_parser_host_list_parser_spec_t *ret)
{
    EMPTY_LIST(ret);
}

bool exports_duckdb_extension_parser_host_call_parse(
    uint32_t handle, noop_string_t *query, noop_option_string_t *ret,
    exports_duckdb_extension_parser_host_duckerror_t *err)
{
    (void)handle; (void)query; (void)err;
    ret->is_some = false;
    EMPTY_STRING(&ret->val);
    return true;
}

void exports_duckdb_extension_optimizer_host_optimizer_list(
    exports_duckdb_extension_optimizer_host_list_optimizer_spec_t *ret)
{
    EMPTY_LIST(ret);
}

bool exports_duckdb_extension_optimizer_host_call_optimize(
    uint32_t handle, noop_string_t *plan_json, noop_option_string_t *ret,
    exports_duckdb_extension_optimizer_host_duckerror_t *err)
{
    (void)handle; (void)plan_json; (void)err;
    ret->is_some = false;
    EMPTY_STRING(&ret->val);
    return true;
}

/* =======================================================================
 * duckdb:extension/files-host — 3 exports.
 * Errors here use noop_string_t (not duckerror). We still return
 * success — DuckDB's own file layer is the one duckdb actually uses
 * for local paths; this interface is only invoked when a virtual scheme
 * is registered, and our provider registers none.
 * =====================================================================*/

bool exports_duckdb_extension_files_host_file_open(
    noop_string_t *url,
    exports_duckdb_extension_files_host_file_open_result_t *ret,
    noop_string_t *err)
{
    (void)url;
    ZERO(ret);
    EMPTY_STRING(err);
    return true;
}

bool exports_duckdb_extension_files_host_file_read(
    uint32_t handle, uint64_t offset, uint32_t len,
    noop_list_u8_t *ret, noop_string_t *err)
{
    (void)handle; (void)offset; (void)len;
    EMPTY_LIST(ret);
    EMPTY_STRING(err);
    return true;
}

bool exports_duckdb_extension_files_host_file_close(
    uint32_t handle, noop_string_t *err)
{
    (void)handle;
    EMPTY_STRING(err);
    return true;
}

/* =======================================================================
 * duckdb:extension/table-stream-host — 4 exports (1 void + 3 bool).
 * =====================================================================*/

void exports_duckdb_extension_table_stream_host_filterable_table_list(
    exports_duckdb_extension_table_stream_host_list_filterable_table_t *ret)
{
    EMPTY_LIST(ret);
}

bool exports_duckdb_extension_table_stream_host_ts_open_filtered(
    uint32_t handle,
    exports_duckdb_extension_table_stream_host_list_duckvalue_t *args,
    noop_list_u32_t *projection,
    exports_duckdb_extension_table_stream_host_list_ts_filter_t *filters,
    exports_duckdb_extension_table_stream_host_ts_open_result_t *ret,
    exports_duckdb_extension_table_stream_host_duckerror_t *err)
{
    (void)handle; (void)args; (void)projection; (void)filters; (void)err;
    ZERO(ret);
    return true;
}

bool exports_duckdb_extension_table_stream_host_ts_next(
    uint32_t handle, uint32_t cursor, uint32_t max_rows,
    exports_duckdb_extension_table_stream_host_resultset_t *ret,
    exports_duckdb_extension_table_stream_host_duckerror_t *err)
{
    (void)handle; (void)cursor; (void)max_rows; (void)err;
    ZERO(ret);
    return true;
}

bool exports_duckdb_extension_table_stream_host_ts_close(
    uint32_t handle, uint32_t cursor, bool *ret,
    exports_duckdb_extension_table_stream_host_duckerror_t *err)
{
    (void)handle; (void)cursor; (void)err;
    *ret = true;
    return true;
}

/* =======================================================================
 * duckdb:component/host-extension-loader — 1 export.
 * Returning false = "no extension loaded" — DuckDB behaves as if the
 * requested extension didn't register anything.
 * =====================================================================*/

bool exports_duckdb_component_host_extension_loader_request_load(
    noop_string_t *name)
{
    (void)name;
    return false;
}

/* =======================================================================
 * duckdb:component/extension-loader-hooks — 1 export.
 * =====================================================================*/

void exports_duckdb_component_extension_loader_hooks_get_pending_registrations(
    exports_duckdb_component_extension_loader_hooks_pending_registrations_t *ret)
{
    /* pending_registrations is a struct-of-empty-lists. Zeroing it gives
     * every list ptr=NULL/len=0, which is a valid canonical-ABI empty. */
    ZERO(ret);
}

/* =======================================================================
 * tvm:memory/manager — 5 exports.
 * TVM = "tegmentum virtual memory"; the core uses it for buffer handoff.
 * Noop keeps region IDs and handles at zero — DuckDB's paths that use
 * TVM (arrow-ipc export, spill) fall back to in-process copies.
 * =====================================================================*/

bool exports_tvm_memory_manager_create_region(
    exports_tvm_memory_manager_region_kind_t kind, uint32_t capacity,
    uint16_t *ret, exports_tvm_memory_manager_tvm_error_t *err)
{
    (void)kind; (void)capacity; (void)err;
    *ret = 0;
    return true;
}

bool exports_tvm_memory_manager_destroy_region(
    uint16_t region_id, exports_tvm_memory_manager_tvm_error_t *err)
{
    (void)region_id; (void)err;
    return true;
}

bool exports_tvm_memory_manager_alloc(
    uint16_t region_id, uint32_t size,
    exports_tvm_memory_manager_handle_t *ret,
    exports_tvm_memory_manager_tvm_error_t *err)
{
    (void)region_id; (void)size; (void)err;
    ZERO(ret);
    return true;
}

bool exports_tvm_memory_manager_dealloc(
    exports_tvm_memory_manager_handle_t *ptr,
    exports_tvm_memory_manager_tvm_error_t *err)
{
    (void)ptr; (void)err;
    return true;
}

bool exports_tvm_memory_manager_describe_region(
    uint16_t region_id,
    exports_tvm_memory_manager_region_info_t *ret,
    exports_tvm_memory_manager_tvm_error_t *err)
{
    (void)region_id; (void)err;
    ZERO(ret);
    return true;
}

/* =======================================================================
 * tvm:memory/bytes — 2 exports.
 * =====================================================================*/

bool exports_tvm_memory_bytes_read(
    exports_tvm_memory_bytes_handle_t *ptr, uint32_t len,
    noop_list_u8_t *ret, exports_tvm_memory_bytes_tvm_error_t *err)
{
    (void)ptr; (void)len; (void)err;
    EMPTY_LIST(ret);
    return true;
}

bool exports_tvm_memory_bytes_write(
    exports_tvm_memory_bytes_handle_t *ptr, noop_list_u8_t *data,
    exports_tvm_memory_bytes_tvm_error_t *err)
{
    (void)ptr; (void)data; (void)err;
    return true;
}
