//===----------------------------------------------------------------------===//
// wasm_index_bridge.h
//
// C ABI between the C++ custom-index shim (wasm_index.cpp) and the Rust core
// (core/src/lib.rs), which routes each call to the host-provided
// `duckdb:extension/index-host` import and on to the backing index component's
// `index-dispatch` export (Item 3 / M2a).
//
// Lifecycle is build-then-search (in memory for M2a):
//   wasm_index_create(type, name, dims) -> handle   (0 == error)
//   wasm_index_append(handle, rowids, n, vectors_flat, dims) -> 0 ok / -1 err
//   wasm_index_build(handle) -> 0 ok / -1 err
//   wasm_index_drop(handle)  -> 0 ok / -1 err
//
// Vectors cross the ABI FLATTENED: `vectors_flat` is n_rows*dims contiguous
// f32 (row-major). On error the C++ side reads wasm_index_last_error().
// Search is NOT bridged here: the explicit kNN entry point is a component-side
// table function (hnsw_search), so the core never calls index-search.
//===----------------------------------------------------------------------===//
#ifndef WASM_INDEX_BRIDGE_H
#define WASM_INDEX_BRIDGE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

uint32_t wasm_index_create(const char *type_name, const char *index_name, uint32_t dims);
int32_t wasm_index_append(uint32_t handle, const int64_t *rowids, uint32_t n_rows,
                          const float *vectors_flat, uint32_t dims);
int32_t wasm_index_build(uint32_t handle);
int32_t wasm_index_drop(uint32_t handle);

// M2b (optimizer auto-rewrite): kNN search over the built index. `query` is
// `dims` contiguous f32; on success writes up to `k` rowids into `out_rowids`
// (caller-allocated, length >= k) and returns the count written (0..=k), or -1
// on error (message in wasm_index_last_error). The optimizer rule calls this at
// optimize time to turn `ORDER BY array_distance(col, const) LIMIT k` into a
// table scan restricted to the index's k nearest rowids.
int32_t wasm_index_search(uint32_t handle, const float *query, uint32_t dims, uint32_t k,
                          int64_t *out_rowids);

const char *wasm_index_last_error(void);

#ifdef __cplusplus
}
#endif

#endif // WASM_INDEX_BRIDGE_H
