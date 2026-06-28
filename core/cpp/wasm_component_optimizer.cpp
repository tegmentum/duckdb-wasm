//===----------------------------------------------------------------------===//
// wasm_component_optimizer.cpp
//
// 2.3.0 / v3: a component-driven OptimizerExtension. Generalizes the hard-coded
// wasm_index_optimizer reroute into a rule the optimizer COMPONENT decides:
//
//   1. flatten the bound logical plan into a NEUTRAL JSON descriptor (operator
//      type names + parent links + the table name for GETs -- NOT a by-value
//      LogicalOperator tree; the WIT recursion wall),
//   2. offer it to every declared optimizer rule via the wasm_optimizer_rewrite
//      bridge (-> optimizer-host -> the component's optimizer-dispatch.call-optimize),
//   3. if a rule returns a string->SQL REWRITE (the rewrite-query directive),
//      re-plan that SQL with a fresh Parser+Planner and replace the plan in place.
//
// The plan crosses the boundary as JSON text; nothing DuckDB-internal leaks by
// value (the v3 leak invariant). Compiled in-core with the wasi-sdk flags via
// build.rs, linking the prebuilt libduckdb-wasi.a internal C++ API.
//===----------------------------------------------------------------------===//

#include "duckdb.hpp"
#include "duckdb.h"

#include "duckdb/main/capi/capi_internal.hpp"
#include "duckdb/optimizer/optimizer_extension.hpp"
#include "duckdb/main/config.hpp"
#include "duckdb/main/client_context.hpp"
#include "duckdb/main/database.hpp"

#include "duckdb/parser/parser.hpp"
#include "duckdb/planner/planner.hpp"
#include "duckdb/planner/logical_operator.hpp"
#include "duckdb/planner/operator/logical_get.hpp"
#include "duckdb/catalog/catalog_entry/table_catalog_entry.hpp"

#include "wasm_optimizer_bridge.h"

#include <cstdio>
#include <string>

namespace duckdb {

namespace {

void JsonEscape(const string &in, std::string &out) {
	for (char c : in) {
		if (c == '"' || c == '\\') {
			out += '\\';
			out += c;
		} else if (c == '\n') {
			out += "\\n";
		} else {
			out += c;
		}
	}
}

void FlattenPlan(LogicalOperator &op, int &next_id, int parent, std::string &out, bool &first) {
	int my_id = next_id++;
	if (!first) {
		out += ",";
	}
	first = false;
	out += "{\"id\":" + std::to_string(my_id) + ",\"op\":\"";
	JsonEscape(op.GetName(), out);
	out += "\",\"parent\":" + std::to_string(parent);
	if (op.type == LogicalOperatorType::LOGICAL_GET) {
		auto &get = op.Cast<LogicalGet>();
		auto table = get.GetTable();
		if (table) {
			out += ",\"table\":\"";
			JsonEscape(table->name, out);
			out += "\"";
		}
	}
	out += "}";
	for (auto &child : op.children) {
		FlattenPlan(*child, next_id, my_id, out, first);
	}
}

} // namespace

//! The component-driven optimizer rule. Not in an anonymous namespace so the
//! extern "C" registration below (outside the namespace) can reference it.
class WasmComponentOptimizer : public OptimizerExtension {
public:
	WasmComponentOptimizer() {
		optimize_function = Optimize;
	}
	static void Optimize(OptimizerExtensionInput &input, unique_ptr<LogicalOperator> &plan) {
		if (!plan) {
			return;
		}
		// 1. flatten the plan to neutral JSON.
		std::string json = "[";
		int next_id = 0;
		bool first = true;
		FlattenPlan(*plan, next_id, -1, json, first);
		json += "]";

		// 2. offer it to the declared component rules.
		char *rewrite = wasm_optimizer_rewrite(json.c_str());
		if (!rewrite) {
			return; // no rule claimed it
		}
		std::string sql(rewrite);
		wasm_optimizer_free(rewrite);
		if (sql.empty()) {
			return;
		}

		// 3. re-plan the rewrite SQL and replace the plan in place.
		try {
			Parser parser;
			parser.ParseQuery(sql);
			if (parser.statements.empty()) {
				return;
			}
			Planner planner(input.context);
			planner.CreatePlan(std::move(parser.statements[0]));
			if (planner.plan) {
				fprintf(stderr, "[wasm_optimizer] component rule rewrote the plan via SQL: %s\n", sql.c_str());
				plan = std::move(planner.plan);
			}
		} catch (std::exception &e) {
			fprintf(stderr, "[wasm_optimizer] rewrite re-plan failed (%s); keeping original plan\n", e.what());
		}
	}
};

} // namespace duckdb

//! Install the component-driven optimizer rule on `db`. Idempotent: a process-wide
//! guard avoids stacking duplicate rules (the registration runs per query when
//! optimizer rules exist).
extern "C" void wasm_register_component_optimizer(duckdb_database db) {
	if (!db) {
		return;
	}
	static bool registered = false;
	if (registered) {
		return;
	}
	try {
		auto wrapper = reinterpret_cast<duckdb::DatabaseWrapper *>(db);
		if (!wrapper || !wrapper->database) {
			return;
		}
		auto &instance = *wrapper->database->instance;
		auto &config = duckdb::DBConfig::GetConfig(instance);
		duckdb::OptimizerExtension::Register(config, duckdb::WasmComponentOptimizer());
		registered = true;
		fprintf(stderr, "[wasm_optimizer] registered component optimizer rule\n");
	} catch (...) {
		// best-effort; leave unregistered on failure
	}
}
