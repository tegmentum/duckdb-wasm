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
// `limit < 0` means no limit. Returns a scan handle, or 0 on error (message in
// wasm_storage_last_error).
uint32_t wasm_storage_scan_open(uint32_t catalog, const char *table, const uint32_t *projection,
                                uint32_t nproj, const WasmScanFilter *filters, uint32_t nfilt,
                                int64_t limit);

// Pull the next batch of rows into `chunk` (a `duckdb_data_chunk` raw handle).
// Columns are filled in projection order matching the table function's output
// types. Returns true if rows were written, false at EOF (chunk size set 0).
// On error returns false and sets wasm_storage_last_error (caller should check).
bool wasm_storage_scan_fill(uint32_t scan, void *chunk);

// Close + free a scan cursor.
void wasm_storage_scan_close(uint32_t scan);

#ifdef __cplusplus
}
#endif

#endif // WASM_STORAGE_BRIDGE_H
