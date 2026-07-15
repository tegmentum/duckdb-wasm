//===----------------------------------------------------------------------===//
// wasm_storage_bridge.h
//
// C ABI between the C++ wasm StorageExtension (wasm_storage.cpp) and the Rust
// core (core/src/lib.rs), which routes each call to the host-provided
// `duckdb:extension/storage-host` import and on to the backing component's
// `storage-dispatch` export.
//
// Conventions:
//   * `wasm_storage_attach` returns a component-side catalog handle, 0 == error.
//   * `wasm_storage_list_tables` / `wasm_storage_table_columns` return a
//     malloc'd, NUL-terminated string the caller MUST free with
//     `wasm_storage_free`; NULL signals an error. Tables are '\n'-joined names;
//     columns are '\n'-joined `name\t<duckdb_type_code>` lines (the code is a
//     `duckdb_type` enum value: BOOLEAN=1, BIGINT=5, UBIGINT=9, DOUBLE=11,
//     VARCHAR=17, BLOB=18).
//   * `wasm_storage_last_error` returns the most recent error message (owned by
//     the core; valid until the next bridge call).
//===----------------------------------------------------------------------===//
#ifndef WASM_STORAGE_BRIDGE_H
#define WASM_STORAGE_BRIDGE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

uint32_t wasm_storage_attach(const char *dsn);
char *wasm_storage_list_tables(uint32_t catalog);
char *wasm_storage_table_columns(uint32_t catalog, const char *table);
void wasm_storage_free(char *ptr);
const char *wasm_storage_last_error(void);

//===----------------------------------------------------------------------===//
// M2b scan surface: engine-driven projection + filter pushdown.
//===----------------------------------------------------------------------===//

// Compare-op codes, mirroring storage-host.wit's `compare-op` enum order.
#define WASM_SCAN_OP_EQ 0
#define WASM_SCAN_OP_NE 1
#define WASM_SCAN_OP_LT 2
#define WASM_SCAN_OP_LE 3
#define WASM_SCAN_OP_GT 4
#define WASM_SCAN_OP_GE 5
#define WASM_SCAN_OP_IS_NULL 6
#define WASM_SCAN_OP_IS_NOT_NULL 7

// Value-type tags for the filter's constant. is-null / is-not-null carry no
// value (use WASM_SCAN_VAL_NONE).
#define WASM_SCAN_VAL_NONE 0
#define WASM_SCAN_VAL_BOOLEAN 1
#define WASM_SCAN_VAL_INT64 2
#define WASM_SCAN_VAL_FLOAT64 3
#define WASM_SCAN_VAL_TEXT 4

// One pushed-down predicate crossing the C ABI. `column` indexes the table's
// FULL column list. The value is a small tagged union: only the field selected
// by `value_type` is meaningful. `text` (for WASM_SCAN_VAL_TEXT) is a borrowed,
// NUL-terminated pointer valid for the duration of the scan-open call.
typedef struct WasmScanFilter {
	uint32_t column;
	uint8_t op;         // WASM_SCAN_OP_*
	uint8_t value_type; // WASM_SCAN_VAL_*
	int64_t i64;        // WASM_SCAN_VAL_INT64 / WASM_SCAN_VAL_BOOLEAN (0/1)
	double f64;         // WASM_SCAN_VAL_FLOAT64
	const char *text;   // WASM_SCAN_VAL_TEXT (NUL-terminated, borrowed)
} WasmScanFilter;

// Open a scan cursor over `(catalog, table)` honoring the projection (real
// table column indices, in emit order; nproj==0 => all columns) and filters.
// `limit < 0` means no limit. When `wants_rowid` is non-zero the guest is
// asked to emit a stable per-row s64 rowid as the FINAL cell of each row
// returned by scan-next (in addition to the projected cells); the caller
// (scan-fill) then routes those rowid values to the DuckDB output slots
// whose column_id was `COLUMN_IDENTIFIER_ROW_ID`. `rowid_slots` /
// `nrowid_slots` names those OUTPUT-vector positions (in emit order); may be
// NULL / 0 iff `wants_rowid == 0`. Returns a scan handle, or 0 on error
// (message in wasm_storage_last_error).
uint32_t wasm_storage_scan_open(uint32_t catalog, const char *table, const uint32_t *projection,
                                uint32_t nproj, const WasmScanFilter *filters, uint32_t nfilt,
                                int64_t limit, uint8_t wants_rowid,
                                const uint32_t *rowid_slots, uint32_t nrowid_slots);

// Pull the next batch of rows into `chunk` (a `duckdb_data_chunk` raw handle).
// Columns are filled in projection order matching the table function's output
// types. Returns true if rows were written, false at EOF (chunk size set 0).
// On error returns false and sets wasm_storage_last_error (caller should check).
bool wasm_storage_scan_fill(uint32_t scan, void *chunk);

// Close + free a scan cursor.
void wasm_storage_scan_close(uint32_t scan);

//===----------------------------------------------------------------------===//
// M2c WRITE surface: transactions + DDL + DML.
//
// The C++ WasmTransactionManager / WasmSchemaEntry::CreateTable /
// WasmPhysical{Insert,Update,Delete} call these extern-C fns; each routes to
// the host-provided `duckdb:extension/storage-host` write imports, which the
// host forwards to the writable storage component's `storage-write-dispatch`
// export (transactions + DDL + DML — mirror of storage-dispatch on the read
// side).
//
// C ABI conventions (mirroring the scan surface above):
//   * `wasm_storage_write_begin_transaction` returns the component-side
//     transaction handle, 0 on error (message in `wasm_storage_last_error`).
//   * `wasm_storage_write_{commit,rollback}_transaction` return 0 on success,
//     -1 on error.
//   * `wasm_storage_write_create_table` returns 0 on success, -1 on error.
//     `cols` is a heap-borrowed `WasmWriteColumn` array of length `ncols`; each
//     entry names one column and its `duckdb_type` code (mirroring the
//     enumeration bridge's `name\t<typecode>` line format).
//   * `wasm_storage_write_{insert,delete,update}_rows` return the count of
//     rows affected (>=0) or -1 on error. DataChunk marshalling uses the
//     tagged-value shape below (`WasmWriteValue`); the caller flattens the
//     input chunk into row-major cells before dispatch.
//===----------------------------------------------------------------------===//

// Column-definition entry for CREATE TABLE. `name` is a borrowed C string
// valid for the duration of the call; `type_code` is the `duckdb_type` enum
// value (see the `WasmTypeCodeToLogical` switch in wasm_storage.cpp).
typedef struct WasmWriteColumn {
	const char *name;
	uint32_t type_code;
} WasmWriteColumn;

// Value-type tags for one DML cell. Mirrors the storage-host `duckvalue`
// variants the writer bridge accepts; unshippable arms surface as
// WASM_WRITE_VAL_NONE (interpreted as SQL NULL).
#define WASM_WRITE_VAL_NONE 0
#define WASM_WRITE_VAL_BOOLEAN 1
#define WASM_WRITE_VAL_INT64 2
#define WASM_WRITE_VAL_FLOAT64 3
#define WASM_WRITE_VAL_TEXT 4
#define WASM_WRITE_VAL_BLOB 5

// One DML cell crossing the C ABI. `text` (for TEXT) is borrowed, NUL-
// terminated, valid for the duration of the call. `blob` is borrowed for the
// call; `blob_len` is meaningful only for WASM_WRITE_VAL_BLOB.
typedef struct WasmWriteValue {
	uint8_t value_type; // WASM_WRITE_VAL_*
	int64_t i64;        // WASM_WRITE_VAL_INT64 / WASM_WRITE_VAL_BOOLEAN (0/1)
	double f64;         // WASM_WRITE_VAL_FLOAT64
	const char *text;   // WASM_WRITE_VAL_TEXT
	const uint8_t *blob; // WASM_WRITE_VAL_BLOB
	uint32_t blob_len;  // WASM_WRITE_VAL_BLOB
} WasmWriteValue;

// Open a component-side transaction on `catalog`; returns a txn handle, or 0
// on error (message in `wasm_storage_last_error`).
uint32_t wasm_storage_write_begin_transaction(uint32_t catalog);

// Commit / rollback an open transaction. 0 on success, -1 on error.
int32_t wasm_storage_write_commit_transaction(uint32_t txn);
int32_t wasm_storage_write_rollback_transaction(uint32_t txn);

// CREATE TABLE. `cols` is `ncols` entries. 0 on success, -1 on error.
int32_t wasm_storage_write_create_table(uint32_t txn, const char *table,
                                        const WasmWriteColumn *cols, uint32_t ncols);

// Append rows. `values` is `nrows * ncols` cells in row-major order (i.e. row
// r's cells are `values[r * ncols .. (r + 1) * ncols]`). Returns the number of
// rows inserted (>=0), or -1 on error.
int64_t wasm_storage_write_insert_rows(uint32_t txn, const char *table,
                                       const WasmWriteValue *values,
                                       uint32_t nrows, uint32_t ncols);

// Delete rows by row-id. Returns rows deleted (>=0), or -1 on error.
int64_t wasm_storage_write_delete_rows(uint32_t txn, const char *table,
                                       const int64_t *rowids, uint32_t nrowids);

// Update rows by row-id. `values` is `nrows * ncols` PARTIAL-ROW cells in
// row-major order, parallel to `rowids` (which is `nrows` long). `ncols` is
// the update-set width (== length of `updated_columns`) — the schema-index
// list of the columns being SET, provided by DuckDB's LogicalUpdate::columns
// captured at plan time. `values[r*ncols + c]` is the new value of schema
// column `updated_columns[c]` on the row identified by `rowids[r]`. Returns
// rows updated (>=0), or -1 on error.
int64_t wasm_storage_write_update_rows(uint32_t txn, const char *table,
                                       const int64_t *rowids,
                                       const uint32_t *updated_columns,
                                       const WasmWriteValue *values,
                                       uint32_t nrows, uint32_t ncols);

#ifdef __cplusplus
}
#endif

#endif // WASM_STORAGE_BRIDGE_H
