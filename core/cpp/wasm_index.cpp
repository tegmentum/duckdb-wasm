//===----------------------------------------------------------------------===//
// wasm_index.cpp
//
// Custom-index REAL build + search (remaining-wit-capabilities-plan Item 3 / M2a).
//
// A WasmBoundIndex : BoundIndex registers a custom INDEX TYPE (e.g. "wasm_hnsw")
// on the live DatabaseInstance. `CREATE INDEX ... USING <type>` routes to the
// duckdb 1.5.x generic index-build framework (the `build_*` callbacks on
// IndexType: bind -> global-init -> local-init -> sink -> combine -> finalize),
// which DuckDB drives over a SCAN -> PROJECTION -> FILTER pipeline it builds for
// us. We DON'T use the `create_plan` escape hatch: the generic framework already
// splits each chunk into a key_chunk (the FLOAT[N] vector) and a row_chunk (the
// ROW_TYPE rowid), so build_sink receives exactly what we forward to the wasm
// index component.
//
// The actual HNSW build + search live in a wasm COMPONENT (hnswfns) behind the
// `index-host` / `index-dispatch` WIT boundary; this TU is just the C++ shim
// that turns the DuckDB build pipeline into bridge calls (wasm_index_create /
// _append / _build), mirroring the storage shim (wasm_storage.cpp).
//
//   build_global_init:  wasm_index_create(type, index_name, dims) -> handle
//   build_sink:         extract (rowid, FLOAT[N]) -> wasm_index_append(handle,..)
//   build_finalize:     wasm_index_build(handle); return the WasmBoundIndex
//
// Explicit kNN search is a component-side TABLE FUNCTION (hnsw_search), keyed by
// index NAME, so it reaches the same built index this pipeline populated -- the
// core never calls index-search. (The optimizer auto-rewrite is deferred: M2b.)
//
// Compiled in-core (DUCKDB_BUILD_LIBRARY) with the EXACT wasi-sdk flags used for
// wasm_storage.cpp (see core/build.rs build_wasm_cpp).
//===----------------------------------------------------------------------===//

#include "duckdb.hpp"
#include "duckdb.h"

#include "duckdb/execution/index/bound_index.hpp"
#include "duckdb/execution/index/index_type.hpp"
#include "duckdb/execution/index/index_type_set.hpp"
#include "duckdb/common/exception.hpp"
#include "duckdb/common/types/data_chunk.hpp"
#include "duckdb/common/types/vector.hpp"
#include "duckdb/catalog/catalog_entry/duck_table_entry.hpp"
#include "duckdb/storage/table_io_manager.hpp"
#include "duckdb/main/capi/capi_internal.hpp"
#include "duckdb/main/database.hpp"
#include "duckdb/main/config.hpp"

// wasm_hnsw_index_scan TableFunction (M2b full rewrite) -- mirrors vss
// src/hnsw/hnsw_index_scan.cpp for this duckdb version.
#include "duckdb/function/table_function.hpp"
#include "duckdb/storage/data_table.hpp"
#include "duckdb/storage/table/scan_state.hpp"
#include "duckdb/storage/table/column_segment.hpp"
#include "duckdb/storage/storage_index.hpp"
#include "duckdb/storage/statistics/base_statistics.hpp"
#include "duckdb/storage/statistics/node_statistics.hpp"
#include "duckdb/transaction/duck_transaction.hpp"
#include "duckdb/planner/operator/logical_get.hpp"

#include "wasm_index_bridge.h"
#include "wasm_index.hpp"

#include <atomic>
#include <cstdio>
#include <string>
#include <vector>

namespace duckdb {

//===----------------------------------------------------------------------===//
// WasmBoundIndex
//
// The catalog-resident index instance returned by build_finalize. The class
// itself lives in wasm_index.hpp so the M2b optimizer rule
// (wasm_index_optimizer.cpp) can find it on a table's index list and read its
// `wasm_handle` + `dims` to drive a kNN search. Only CommitDrop needs the bridge,
// so it is defined here (the rest are inline no-ops in the header).
//===----------------------------------------------------------------------===//

void WasmBoundIndex::CommitDrop(IndexLock &index_lock) {
	if (wasm_handle != 0) {
		wasm_index_drop(wasm_handle);
		wasm_handle = 0;
	}
}

//===----------------------------------------------------------------------===//
// Build framework callbacks (mirror src/execution/index/art/art_index.cpp).
//
// DuckDB drives: build_bind -> build_global_init -> build_local_init ->
// build_sink (per chunk) -> build_combine -> build_finalize. The generic
// PhysicalCreateIndex hands build_sink a key_chunk (FLOAT[N]) + row_chunk
// (ROW_TYPE rowid), already projected + null-filtered.
//===----------------------------------------------------------------------===//

namespace {

//! Carries the registered USING type name into the build callbacks. The
//! IndexType (and thus its callbacks) is registered per type name, but the
//! callbacks are plain C function pointers with no captured state, so we stash
//! the type name in the IndexTypeInfo and read it back at global-init.
struct WasmIndexTypeInfo : public IndexTypeInfo {
	explicit WasmIndexTypeInfo(string type_name_p) : type_name(std::move(type_name_p)) {
	}
	string type_name;
};

struct WasmIndexBindData : public IndexBuildBindData {
	// nothing to bind for M2a
};

unique_ptr<IndexBuildBindData> WasmBuildBind(IndexBuildBindInput &input) {
	if (input.expressions.size() != 1) {
		throw BinderException("wasm index: exactly one key column is supported");
	}
	auto &arr_type = input.expressions[0]->return_type;
	if (arr_type.id() != LogicalTypeId::ARRAY) {
		throw BinderException("wasm index keys must be of type FLOAT[N]");
	}
	return make_uniq<WasmIndexBindData>();
}

//! Global state owns the WasmBoundIndex (returned at finalize) + the dims read
//! from the key array type. wasm_index_create is called HERE (once), so the
//! component allocates the builder before the first sink batch.
struct WasmIndexGlobalState : public IndexBuildGlobalState {
	unique_ptr<WasmBoundIndex> global_index;
	uint32_t handle = 0;
	idx_t dims = 0;
};

unique_ptr<IndexBuildGlobalState> WasmBuildGlobalInit(IndexBuildInitGlobalStateInput &input) {
	auto state = make_uniq<WasmIndexGlobalState>();

	// The indexed key is FLOAT[N]; N = ArrayType size of the (single) expression.
	auto &arr_type = input.expressions[0]->return_type;
	idx_t dims = ArrayType::GetSize(arr_type);
	state->dims = dims;

	// The USING type name. M2a registers a single custom type ("wasm_hnsw") that
	// routes to the one index backend; the component keys its state by index
	// NAME, so the type name is informational. The build callbacks are plain C
	// function pointers (no captured type), so we use the canonical name here.
	string type_name = "wasm_hnsw";

	auto &storage = input.table.GetStorage();

	// Allocate the component-side builder. The index is keyed by NAME in the
	// component so the hnsw_search table function reaches the SAME index.
	uint32_t handle = wasm_index_create(type_name.c_str(), input.info.index_name.c_str(), (uint32_t)dims);
	if (handle == 0) {
		throw InternalException("wasm index create failed: %s", wasm_index_last_error());
	}
	state->handle = handle;

	state->global_index = make_uniq<WasmBoundIndex>(input.info.index_name, type_name, input.info.constraint_type,
	                                                input.storage_ids, TableIOManager::Get(storage), input.expressions,
	                                                storage.db, handle, (uint32_t)dims);

	return std::move(state);
}

struct WasmIndexLocalState : public IndexBuildLocalState {
	// M2a: the build is single-pass into the one component-side index (no thread
	// merge), so the local state is empty.
};

unique_ptr<IndexBuildLocalState> WasmBuildLocalInit(IndexBuildInitLocalStateInput &input) {
	return make_uniq<WasmIndexLocalState>();
}

//! Extract (rowid, FLOAT[N] vector) from the projected chunks and append to the
//! component-side builder. key_chunk.data[0] is the FLOAT[N] ARRAY; its child
//! vector holds the floats contiguously (after Flatten the framework already
//! flattened the input). row_chunk.data[0] is the ROW_TYPE rowid.
void WasmBuildSink(IndexBuildSinkInput &input, DataChunk &key_chunk, DataChunk &row_chunk) {
	auto &gstate = input.global_state.Cast<WasmIndexGlobalState>();
	auto count = key_chunk.size();
	if (count == 0) {
		return;
	}

	auto &array_vec = key_chunk.data[0];
	const idx_t array_size = ArrayType::GetSize(array_vec.GetType());
	if (array_size != gstate.dims) {
		throw InternalException("wasm index: key array size %llu != index dims %llu", (unsigned long long)array_size,
		                        (unsigned long long)gstate.dims);
	}

	// Flatten so the child floats + rowids are contiguous flat arrays.
	array_vec.Flatten(count);
	auto &child_vec = ArrayVector::GetEntry(array_vec);
	child_vec.Flatten(count * array_size);
	auto &rowid_vec = row_chunk.data[0];
	rowid_vec.Flatten(count);

	const float *child_data = FlatVector::GetData<float>(child_vec);
	const int64_t *rowid_data = FlatVector::GetData<int64_t>(rowid_vec);

	// Build contiguous rowid + flattened vector buffers for the bridge. The
	// array child is already row-major (count * array_size), matching the
	// bridge's `vectors_flat` layout exactly, so we can pass it through.
	std::vector<int64_t> rowids(rowid_data, rowid_data + count);

	int32_t rc = wasm_index_append(gstate.handle, rowids.data(), (uint32_t)count, child_data, (uint32_t)array_size);
	if (rc != 0) {
		throw InternalException("wasm index append failed: %s", wasm_index_last_error());
	}
}

void WasmBuildCombine(IndexBuildCombineInput &input) {
	// Single component-side index; nothing to merge.
}

//! Finalize the component-side build and hand the catalog the WasmBoundIndex.
unique_ptr<BoundIndex> WasmBuildFinalize(IndexBuildFinalizeInput &input) {
	auto &gstate = input.global_state.Cast<WasmIndexGlobalState>();
	int32_t rc = wasm_index_build(gstate.handle);
	if (rc != 0) {
		throw InternalException("wasm index build failed: %s", wasm_index_last_error());
	}
	return std::move(gstate.global_index);
}

//! create_instance: re-materialize a WasmBoundIndex (e.g. on catalog load). For
//! M2a there is no persistence, so this builds an unbuilt (handle 0) stub.
unique_ptr<BoundIndex> WasmCreateIndexInstance(CreateIndexInput &input) {
	return make_uniq<WasmBoundIndex>(input.name, std::string(WASM_HNSW_INDEX_TYPE), input.constraint_type,
	                                 input.column_ids, input.table_io_manager, input.unbound_expressions, input.db, 0,
	                                 0);
}

} // namespace

//===----------------------------------------------------------------------===//
// wasm_hnsw_index_scan TABLE FUNCTION
//
// vss-style physical index scan. The optimizer swaps a matched seq_scan GET's
// `function` to this and sets bind_data = WasmIndexScanBindData. We fire the
// component search once at init_global (rowids already sorted nearest-first by
// instant-distance) and fetch the table rows for those rowids in execute,
// preserving distance order. No outer TOP_N / sort needed.
//===----------------------------------------------------------------------===//

namespace {

BindInfo WasmIndexScanBindInfo(const optional_ptr<FunctionData> bind_data_p) {
	auto &bind_data = bind_data_p->Cast<WasmIndexScanBindData>();
	return BindInfo(bind_data.table);
}

struct WasmIndexScanGlobalState : public GlobalTableFunctionState {
	ColumnFetchState fetch_state;
	vector<StorageIndex> column_ids;

	// The k nearest rowids from the wasm index, IN DISTANCE ORDER, + a cursor.
	vector<int64_t> rowids;
	idx_t cursor = 0;

	// For projection-pushdown (filter-column removal) path.
	DataChunk all_columns;
	vector<idx_t> projection_ids;
};

unique_ptr<GlobalTableFunctionState> WasmIndexScanInitGlobal(ClientContext &context, TableFunctionInitInput &input) {
	auto &bind_data = input.bind_data->Cast<WasmIndexScanBindData>();
	auto result = make_uniq<WasmIndexScanGlobalState>();

	// Map the requested (logical) column ids to storage column ids, exactly like
	// vss's HNSWIndexScanInitGlobal.
	result->column_ids.reserve(input.column_ids.size());
	for (auto &id : input.column_ids) {
		storage_t col_id = id;
		if (id != DConstants::INVALID_INDEX) {
			col_id = bind_data.table.GetColumn(LogicalIndex(id)).StorageOid();
		}
		result->column_ids.emplace_back(col_id);
	}

	// FIRE the index once: ask the component for the k nearest rowids (sorted
	// nearest-first by instant-distance). Store the ordered list + cursor.
	const idx_t k = bind_data.limit;
	result->rowids.resize(k);
	int32_t n = wasm_index_search(bind_data.wasm_handle, bind_data.query.data(), bind_data.dims, (uint32_t)k,
	                              result->rowids.data());
	if (n < 0) {
		throw InternalException("wasm index scan: search failed: %s", wasm_index_last_error());
	}
	result->rowids.resize((idx_t)n);
	fprintf(stderr, "[wasm_index] index-scan fired (handle=%u k=%llu hits=%d)\n", bind_data.wasm_handle,
	        (unsigned long long)k, n);

	if (!input.CanRemoveFilterColumns()) {
		return std::move(result);
	}

	// Projection-pushdown path: scan into all_columns, then project.
	result->projection_ids = input.projection_ids;
	auto &duck_table = bind_data.table.Cast<DuckTableEntry>();
	const auto &columns = duck_table.GetColumns();
	vector<LogicalType> scanned_types;
	for (const auto &col_idx : input.column_indexes) {
		if (col_idx.IsRowIdColumn()) {
			scanned_types.emplace_back(LogicalType::ROW_TYPE);
		} else {
			scanned_types.push_back(columns.GetColumn(col_idx.ToLogical()).Type());
		}
	}
	result->all_columns.Initialize(context, scanned_types);
	return std::move(result);
}

void WasmIndexScanExecute(ClientContext &context, TableFunctionInput &data_p, DataChunk &output) {
	auto &bind_data = data_p.bind_data->Cast<WasmIndexScanBindData>();
	auto &state = data_p.global_state->Cast<WasmIndexScanGlobalState>();
	auto &transaction = DuckTransaction::Get(context, bind_data.table.catalog);

	// Emit the next chunk of (at most STANDARD_VECTOR_SIZE) rowids in order.
	const idx_t remaining = state.rowids.size() - state.cursor;
	if (remaining == 0) {
		output.SetCardinality(0);
		return;
	}
	const idx_t count = MinValue<idx_t>(remaining, STANDARD_VECTOR_SIZE);

	// Build a ROW_TYPE vector of the rowids for this chunk (preserves distance
	// order: row i of the output corresponds to the i-th nearest neighbour).
	Vector row_ids(LogicalType::ROW_TYPE, count);
	auto row_id_data = FlatVector::GetData<row_t>(row_ids);
	for (idx_t i = 0; i < count; i++) {
		row_id_data[i] = (row_t)state.rowids[state.cursor + i];
	}
	state.cursor += count;

	// Fetch the table rows by rowid (vss uses the exact same DataTable::Fetch).
	if (state.projection_ids.empty()) {
		bind_data.table.GetStorage().Fetch(transaction, output, state.column_ids, row_ids, count, state.fetch_state);
		return;
	}
	state.all_columns.Reset();
	bind_data.table.GetStorage().Fetch(transaction, state.all_columns, state.column_ids, row_ids, count,
	                                   state.fetch_state);
	output.ReferenceColumns(state.all_columns, state.projection_ids);
}

unique_ptr<BaseStatistics> WasmIndexScanStatistics(ClientContext &context, const FunctionData *bind_data_p,
                                                   column_t column_id) {
	auto &bind_data = bind_data_p->Cast<WasmIndexScanBindData>();
	return bind_data.table.GetStatistics(context, column_id);
}

unique_ptr<NodeStatistics> WasmIndexScanCardinality(ClientContext &context, const FunctionData *bind_data_p) {
	auto &bind_data = bind_data_p->Cast<WasmIndexScanBindData>();
	return make_uniq<NodeStatistics>(bind_data.limit, bind_data.limit);
}

InsertionOrderPreservingMap<string> WasmIndexScanToString(TableFunctionToStringInput &input) {
	InsertionOrderPreservingMap<string> result;
	auto &bind_data = input.bind_data->Cast<WasmIndexScanBindData>();
	result["Table"] = bind_data.table.name;
	result["wasm HNSW index handle"] = std::to_string(bind_data.wasm_handle);
	return result;
}

} // namespace

TableFunction WasmIndexScanFunction::GetFunction() {
	TableFunction func("wasm_hnsw_index_scan", {}, WasmIndexScanExecute);
	func.init_local = nullptr;
	func.init_global = WasmIndexScanInitGlobal;
	func.statistics = WasmIndexScanStatistics;
	func.cardinality = WasmIndexScanCardinality;
	func.pushdown_complex_filter = nullptr;
	func.to_string = WasmIndexScanToString;
	func.table_scan_progress = nullptr;
	func.projection_pushdown = true;
	func.filter_pushdown = false;
	func.get_bind_info = WasmIndexScanBindInfo;
	return func;
}

} // namespace duckdb

//! Registers a custom index type named `type_name`. The build framework
//! callbacks turn `CREATE INDEX ... USING <type_name>` into wasm_index_create /
//! _append / _build bridge calls. Mid-session safe: a dup-name check
//! (FindByName) makes repeated calls harmless.
extern "C" void wasm_register_index_type(duckdb_database db, const char *type_name) {
	if (!db || !type_name) {
		return;
	}
	try {
		auto wrapper = reinterpret_cast<duckdb::DatabaseWrapper *>(db);
		if (!wrapper || !wrapper->database) {
			return;
		}
		auto &instance = *wrapper->database->instance;
		auto &config = duckdb::DBConfig::GetConfig(instance);
		auto &index_types = config.GetIndexTypes();
		if (index_types.FindByName(type_name)) {
			return; // already present (built-in or prior registration)
		}
		// M2b: install the optimizer rule that auto-rewrites top-k distance
		// queries into a wasm-index scan. Done here (once, when the type is first
		// registered) so the rule is present before any matching query binds.
		duckdb::wasm_register_index_optimizer(instance);
		duckdb::IndexType index_type;
		index_type.name = std::string(type_name);
		index_type.create_instance = duckdb::WasmCreateIndexInstance;
		index_type.build_bind = duckdb::WasmBuildBind;
		index_type.build_global_init = duckdb::WasmBuildGlobalInit;
		index_type.build_local_init = duckdb::WasmBuildLocalInit;
		index_type.build_sink = duckdb::WasmBuildSink;
		index_type.build_combine = duckdb::WasmBuildCombine;
		index_type.build_finalize = duckdb::WasmBuildFinalize;
		index_type.index_info = duckdb::make_shared_ptr<duckdb::WasmIndexTypeInfo>(std::string(type_name));
		index_types.RegisterIndexType(index_type);
		fprintf(stderr, "[wasm_index] registered custom index type '%s'\n", type_name);
	} catch (const std::exception &e) {
		fprintf(stderr, "wasm_register_index_type failed: %s\n", e.what());
	} catch (...) {
		fprintf(stderr, "wasm_register_index_type failed: unknown error\n");
	}
}
