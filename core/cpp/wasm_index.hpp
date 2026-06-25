//===----------------------------------------------------------------------===//
// wasm_index.hpp
//
// Shared declaration of WasmBoundIndex so BOTH the build shim (wasm_index.cpp)
// and the optimizer rule (wasm_index_optimizer.cpp) can see it: the build path
// constructs it (and stashes the component-side `wasm_handle`), the optimizer
// path finds it on a table's index list and reads that handle to drive a kNN
// search through the wasm_index_search bridge (Item 3 / M2b).
//===----------------------------------------------------------------------===//
#ifndef WASM_INDEX_HPP
#define WASM_INDEX_HPP

#include "duckdb/execution/index/bound_index.hpp"
#include "duckdb/storage/table_io_manager.hpp"

#include <cstdint>

namespace duckdb {

//! The custom INDEX TYPE name registered by wasm_register_index_type and matched
//! by the optimizer rule.
static constexpr const char *WASM_HNSW_INDEX_TYPE = "wasm_hnsw";

//! The catalog-resident handle for a wasm-backed (HNSW) index. The heavy graph
//! lives in the component, keyed by `wasm_handle`; this object is the DuckDB-side
//! BoundIndex. M2a populates it via the build pipeline; M2b's optimizer reads
//! `wasm_handle` + `dims` to search the SAME built index.
class WasmBoundIndex : public BoundIndex {
public:
	WasmBoundIndex(const string &name, const string &index_type, IndexConstraintType index_constraint_type,
	               const vector<column_t> &column_ids, TableIOManager &table_io_manager,
	               const vector<unique_ptr<Expression>> &unbound_expressions, AttachedDatabase &db, uint32_t handle,
	               uint32_t dims_p = 0)
	    : BoundIndex(name, index_type, index_constraint_type, column_ids, table_io_manager, unbound_expressions, db),
	      wasm_handle(handle), dims(dims_p) {
	}

	//! The component-side index-handle (from wasm_index_create); 0 == unbuilt.
	uint32_t wasm_handle = 0;
	//! The FLOAT[N] dimensionality of the key (N), captured at build for search.
	uint32_t dims = 0;

	ErrorData Append(IndexLock &l, DataChunk &chunk, Vector &row_ids) override {
		return ErrorData {};
	}
	void CommitDrop(IndexLock &index_lock) override;
	void Delete(IndexLock &state, DataChunk &entries, Vector &row_identifiers) override {
	}
	ErrorData Insert(IndexLock &l, DataChunk &chunk, Vector &row_ids) override {
		return ErrorData {};
	}
	bool MergeIndexes(IndexLock &state, BoundIndex &other_index) override {
		return true;
	}
	void Vacuum(IndexLock &l) override {
	}
	idx_t GetInMemorySize(IndexLock &state) override {
		return 0;
	}
	void Verify(IndexLock &l) override {
	}
	string ToString(IndexLock &l, bool display_ascii) override {
		return name;
	}
	void VerifyAllocations(IndexLock &l) override {
	}
	string GetConstraintViolationMessage(VerifyExistenceType verify_type, idx_t failed_index,
	                                      DataChunk &input) override {
		return "wasm index: constraint violation";
	}
};

//! Registers the M2b optimizer rule that auto-rewrites top-k distance queries
//! into a wasm-index scan. Defined in wasm_index_optimizer.cpp; called from
//! wasm_register_index_type so it is installed once per database alongside the
//! index type.
void wasm_register_index_optimizer(DatabaseInstance &instance);

} // namespace duckdb

#endif // WASM_INDEX_HPP
