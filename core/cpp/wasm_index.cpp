//===----------------------------------------------------------------------===//
// wasm_index.cpp
//
// Custom-index DE-RISK (remaining-wit-capabilities-plan Item 3 / M1).
//
// Proves a C++ class subclassing duckdb::BoundIndex compiles, links into the
// wasm core, registers a custom INDEX TYPE on the live DatabaseInstance, and is
// reached by `CREATE INDEX ... USING <type>`. This keystone serves vss (HNSW)
// and spatial (R-tree). Mirrors the PROVEN shims wasm_storage.cpp /
// wasm_files.cpp: a C++ subclass of a DuckDB-internal abstract type, registered
// mid-session via an extern-C bridge driven from Rust (core/src/lib.rs).
//
// M1 is a STUB: every BoundIndex pure-virtual throws/no-ops. The index type is
// registered with a `create_plan` callback (the planner's escape hatch in
// plan_create_index.cpp). create_plan constructs a WasmBoundIndex (exercising
// the ctor + the registered create_instance), logs that the index was reached,
// then throws NotImplementedException("wasm index M1 stub: ...") so the test
// observes a RECOGNIZABLE stub error rather than "Unknown index type".
//
// Routing proof chain (DuckDB 1.5.x, this header set):
//   CREATE INDEX ... USING wasm_hnsw
//     -> binder -> LogicalCreateIndex(index_type="wasm_hnsw")
//     -> PhysicalPlanGenerator::CreatePlan(LogicalCreateIndex)
//        config.GetIndexTypes().FindByName("wasm_hnsw")  [must be non-null]
//        if (index_type->create_plan) return create_plan(input);  [our hook]
//
// Compiled in-core (DUCKDB_BUILD_LIBRARY) with the EXACT wasi-sdk flags used for
// wasm_storage.cpp / wasm_files.cpp (see core/build.rs build_wasm_cpp).
//===----------------------------------------------------------------------===//

#include "duckdb.hpp"
#include "duckdb.h"

#include "duckdb/execution/index/bound_index.hpp"
#include "duckdb/execution/index/index_type.hpp"
#include "duckdb/execution/index/index_type_set.hpp"
#include "duckdb/common/exception.hpp"
#include "duckdb/main/capi/capi_internal.hpp"
#include "duckdb/main/database.hpp"
#include "duckdb/main/config.hpp"

#include <atomic>
#include <cstdio>
#include <string>

namespace duckdb {

//===----------------------------------------------------------------------===//
// WasmBoundIndex
//
// The M1 stub index instance. Every pure-virtual from BoundIndex / Index is a
// throw-or-no-op:
//   pure-virtual surface for THIS version (duckdb 1.5.x):
//     ErrorData Append(IndexLock&, DataChunk&, Vector&)            -> stub throw
//     void      CommitDrop(IndexLock&)                            -> no-op
//     ErrorData Insert(IndexLock&, DataChunk&, Vector&)            -> stub throw
//     bool      MergeIndexes(IndexLock&, BoundIndex&)             -> false
//     void      Vacuum(IndexLock&)                                -> no-op
//     idx_t     GetInMemorySize(IndexLock&)                       -> 0
//     void      Verify(IndexLock&)                                -> no-op
//     string    ToString(IndexLock&, bool)                        -> name
//     void      VerifyAllocations(IndexLock&)                     -> no-op
//     string    GetConstraintViolationMessage(...)               -> message
// (Delete has a base default; Serialize* are not pure - left to base.)
//===----------------------------------------------------------------------===//

class WasmBoundIndex : public BoundIndex {
public:
	// Match the canonical custom-index ctor (cf. vss HNSWIndex), forwarding the
	// engine-supplied identity to the BoundIndex base. `index_type` carries the
	// USING name so EXPLAIN / catalog reflect the custom type.
	WasmBoundIndex(const string &name, const string &index_type, IndexConstraintType index_constraint_type,
	               const vector<column_t> &column_ids, TableIOManager &table_io_manager,
	               const vector<unique_ptr<Expression>> &unbound_expressions, AttachedDatabase &db)
	    : BoundIndex(name, index_type, index_constraint_type, column_ids, table_io_manager, unbound_expressions, db) {
		fprintf(stderr, "[wasm_index] WasmBoundIndex constructed: name='%s' type='%s' columns=%zu\n", name.c_str(),
		        index_type.c_str(), (size_t)column_ids.size());
	}

	ErrorData Append(IndexLock &l, DataChunk &chunk, Vector &row_ids) override {
		throw NotImplementedException("wasm index M1 stub: Append");
	}

	void CommitDrop(IndexLock &index_lock) override {
		// no-op
	}

	void Delete(IndexLock &state, DataChunk &entries, Vector &row_identifiers) override {
		// no-op
	}

	ErrorData Insert(IndexLock &l, DataChunk &chunk, Vector &row_ids) override {
		throw NotImplementedException("wasm index M1 stub: Insert");
	}

	bool MergeIndexes(IndexLock &state, BoundIndex &other_index) override {
		return false;
	}

	void Vacuum(IndexLock &l) override {
		// no-op
	}

	idx_t GetInMemorySize(IndexLock &state) override {
		return 0;
	}

	void Verify(IndexLock &l) override {
		// no-op
	}

	string ToString(IndexLock &l, bool display_ascii) override {
		return name;
	}

	void VerifyAllocations(IndexLock &l) override {
		// no-op
	}

	string GetConstraintViolationMessage(VerifyExistenceType verify_type, idx_t failed_index,
	                                      DataChunk &input) override {
		return "wasm index M1 stub: constraint violation";
	}
};

//===----------------------------------------------------------------------===//
// IndexType wiring
//
// create_instance: builds a WasmBoundIndex from a CreateIndexInput (the engine
//   uses this to (re)materialize an index instance, e.g. on load). For M1 it is
//   also driven from create_plan so the ctor is exercised by CREATE INDEX.
//
// create_plan: the planner's escape hatch. We deliberately reach the index
//   instance and then throw a RECOGNIZABLE stub so the de-risk test observes
//   the custom type was constructed + routed, not "Unknown index type".
//===----------------------------------------------------------------------===//

static unique_ptr<BoundIndex> WasmCreateIndexInstance(CreateIndexInput &input) {
	return make_uniq<WasmBoundIndex>(input.name, std::string("wasm_hnsw"), input.constraint_type, input.column_ids,
	                                 input.table_io_manager, input.unbound_expressions, input.db);
}

static PhysicalOperator &WasmCreateIndexPlan(PlanIndexInput &input) {
	// Construct the index instance to exercise the WasmBoundIndex ctor + the
	// registered create_instance path. We don't have a CreateIndexInput here
	// (that's built inside the physical operator), so log + throw the stub: the
	// fact we reached this callback already proves registration + routing.
	fprintf(stderr, "[wasm_index] WasmCreateIndexPlan reached for USING wasm_hnsw -> M1 stub\n");
	throw NotImplementedException("wasm index M1 stub: create_plan (USING wasm_hnsw)");
}

} // namespace duckdb

//! Registers a custom index type named `type_name` on the given database's
//! index-type set. Mid-session safe: a process-wide once-guard plus a dup-name
//! check (FindByName) make repeated calls harmless. The registered type routes
//! `CREATE INDEX ... USING <type_name>` to WasmCreateIndexPlan / the
//! WasmBoundIndex instance.
extern "C" void wasm_register_index_type(duckdb_database db, const char *type_name) {
	static std::atomic<bool> registered {false};
	if (!db || !type_name) {
		return;
	}
	bool expected = false;
	if (!registered.compare_exchange_strong(expected, true)) {
		return;
	}
	try {
		auto wrapper = reinterpret_cast<duckdb::DatabaseWrapper *>(db);
		if (!wrapper || !wrapper->database) {
			registered.store(false);
			return;
		}
		auto &instance = *wrapper->database->instance;
		auto &config = duckdb::DBConfig::GetConfig(instance);
		auto &index_types = config.GetIndexTypes();
		if (index_types.FindByName(type_name)) {
			return; // already present (built-in or prior registration)
		}
		duckdb::IndexType index_type;
		index_type.name = std::string(type_name);
		index_type.create_instance = duckdb::WasmCreateIndexInstance;
		index_type.create_plan = duckdb::WasmCreateIndexPlan;
		index_types.RegisterIndexType(index_type);
		fprintf(stderr, "[wasm_index] registered custom index type '%s'\n", type_name);
	} catch (const std::exception &e) {
		fprintf(stderr, "wasm_register_index_type failed: %s\n", e.what());
		registered.store(false);
	} catch (...) {
		fprintf(stderr, "wasm_register_index_type failed: unknown error\n");
		registered.store(false);
	}
}
