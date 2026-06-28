//===----------------------------------------------------------------------===//
// wasm_table_stream_bridge.h
//
// C ABI between the C++ streaming TableFunction (wasm_table_stream.cpp) and the
// Rust core (core/src/lib.rs), which routes each call to the host-provided
// `duckdb:extension/table-stream-host` import and on to the owning component's
// `table-stream-dispatch` export (call-table-open-filtered / next / close).
//
// This is the streaming + FILTER-PUSHDOWN analogue of wasm_storage_bridge.h, for
// a NAMED table function (with bound argument values) rather than an ATTACH-ed
// catalog. The 3.1.0 additive minor's end-to-end core shim.
//===----------------------------------------------------------------------===//
#ifndef WASM_TABLE_STREAM_BRIDGE_H
#define WASM_TABLE_STREAM_BRIDGE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// Compare-op codes, mirroring table-stream-host.wit's `ts-filter-op` enum order.
#define WASM_TS_OP_EQ 0
#define WASM_TS_OP_NE 1
#define WASM_TS_OP_LT 2
#define WASM_TS_OP_LE 3
#define WASM_TS_OP_GT 4
#define WASM_TS_OP_GE 5
#define WASM_TS_OP_IS_IN 6
#define WASM_TS_OP_IS_NULL 7
#define WASM_TS_OP_IS_NOT_NULL 8

// Value-type tags for a tagged constant (filter operand or bound argument).
#define WASM_TS_VAL_NONE 0
#define WASM_TS_VAL_BOOLEAN 1
#define WASM_TS_VAL_INT64 2
#define WASM_TS_VAL_FLOAT64 3
#define WASM_TS_VAL_TEXT 4

// A tagged scalar value crossing the C ABI (bound arg, or a filter operand).
// Only the field selected by `value_type` is meaningful. `text` is a borrowed,
// NUL-terminated pointer valid for the duration of the open call.
typedef struct WasmTsValue {
	uint8_t value_type; // WASM_TS_VAL_*
	int64_t i64;        // INT64 / BOOLEAN (0/1)
	double f64;         // FLOAT64
	const char *text;   // TEXT (NUL-terminated, borrowed)
} WasmTsValue;

// One pushed-down predicate. `column` indexes the EMITTED (post-projection)
// schema. A scalar comparator carries exactly one value in [values, nvalues);
// is-null / is-not-null carry zero. (is-in is declared but the C++ shim does not
// flatten IN today; the engine re-applies it.)
typedef struct WasmTsFilter {
	uint32_t column;
	uint8_t op;           // WASM_TS_OP_*
	const WasmTsValue *values;
	uint32_t nvalues;
} WasmTsFilter;

// Register a streaming + filter-pushdown TableFunction named `name` (global
// callback `handle`). `arg_type_codes` is a comma-joined list of duckdb_type
// codes for the positional arguments (may be empty). `cols_spec` is a
// '\n'-joined list of `name\t<duckdb_type_code>` lines for the emitted columns.
// Idempotent per (db, name). Returns 0 on success, non-zero on error.
int32_t wasm_register_filterable_table_function(void *db, const char *name, uint32_t handle,
                                                const char *arg_type_codes, const char *cols_spec);

// Open a streaming cursor for table fn `handle` with the bound `args`, the
// `projection` (emitted column indices in order; nproj==0 => all), and the
// conjunctive `filters`. Returns a cursor handle, or 0 on error (message in
// wasm_table_stream_last_error).
uint32_t wasm_table_stream_open(uint32_t handle, const WasmTsValue *args, uint32_t nargs,
                                const uint32_t *projection, uint32_t nproj,
                                const WasmTsFilter *filters, uint32_t nfilt);

// Pull the next batch into `chunk` (a `duckdb_data_chunk` raw handle). Returns
// true if rows were written, false at EOF (chunk size set 0) or on error (with
// wasm_table_stream_last_error set).
bool wasm_table_stream_fill(uint32_t handle, uint32_t cursor, void *chunk);

// Close + free a streaming cursor.
void wasm_table_stream_close(uint32_t handle, uint32_t cursor);

// Most recent error (owned by the core; valid until the next bridge call).
const char *wasm_table_stream_last_error(void);

#ifdef __cplusplus
}
#endif

#endif // WASM_TABLE_STREAM_BRIDGE_H
