//===----------------------------------------------------------------------===//
// wasm_index_optimizer.cpp
//
// Item 3 / M2b: the keystone. An OptimizerExtension rule in the wasm core that
// auto-rewrites
//
//     SELECT ... FROM t ORDER BY array_distance(col, <const vec>) LIMIT k
//
// into a table scan restricted to the k nearest row-ids returned by the custom
// wasm HNSW index on `col` -- so a PLAIN query uses the index with NO explicit
// hnsw_search() call.
//
// MECHANISM (mirrors vss's src/hnsw/hnsw_optimize_scan.cpp for THIS duckdb
// version). DuckDB collapses `ORDER BY ... LIMIT k` into a LOGICAL_TOP_N before
// the optimizer extensions run, so the rule matches:
//
//     LOGICAL_TOP_N  (single ASC order, order expr = BOUND_COLUMN_REF #p)
//       └─ LOGICAL_PROJECTION  (expressions[p] = array_distance(<col>, <const>))
//            └─ LOGICAL_GET  (seq_scan on a DuckTable that has a wasm_hnsw index on <col>)
//
// On a match it:
//   1. extracts the indexed column, the constant query vector and k,
//   2. finds the WasmBoundIndex on that table+column (its `wasm_handle`),
//   3. calls wasm_index_search(handle, query, k) -> the k nearest row-ids
//      (this is where the index FIRES; the host forwards to the component's
//      index-search over the SAME built map the build pipeline populated),
//   4. REWRITES the plan: pushes a rowid IN (<those row-ids>) table-filter into
//      the GET and removes the redundant TOP_N. The seq scan then visits only
//      the index-selected rows; the surviving projection emits the result.
//
// REWRITE LEVEL: proof-of-mechanism reroute (NOT a brand-new physical operator).
// The search runs at optimize time and its k row-ids become a rowid table-filter
// on the existing seq_scan -- correct results, the index demonstrably fires (log
// line + EXPLAIN shows the rowid IN filter and NO sort), and no full-scan+sort.
// A full physical index-scan operator (vss-style swap of get.function to a custom
// TableFunction whose init_global runs the search and whose execute fetches rows
// by rowid) is the deeper alternative; see the report.
//
// Compiled in-core with the same wasi-sdk flags as wasm_index.cpp (build.rs).
//===----------------------------------------------------------------------===//

#include "duckdb.hpp"

#include "duckdb/optimizer/optimizer.hpp"
#include "duckdb/optimizer/optimizer_extension.hpp"
#include "duckdb/main/database.hpp"
#include "duckdb/main/config.hpp"
#include "duckdb/main/client_context.hpp"

#include "duckdb/planner/operator/logical_top_n.hpp"
#include "duckdb/planner/operator/logical_projection.hpp"
#include "duckdb/planner/operator/logical_get.hpp"

#include "duckdb/planner/expression/bound_columnref_expression.hpp"
#include "duckdb/planner/expression/bound_function_expression.hpp"
#include "duckdb/planner/expression/bound_constant_expression.hpp"

#include "duckdb/catalog/catalog_entry/duck_table_entry.hpp"
#include "duckdb/storage/data_table.hpp"
#include "duckdb/storage/table/data_table_info.hpp"
#include "duckdb/storage/table/table_index_list.hpp"

#include "duckdb/common/types/value.hpp"

#include "wasm_index_bridge.h"
#include "wasm_index.hpp"

#include <cstdio>
#include <vector>

namespace duckdb {

namespace {

//! The ASC distance functions we route through the index. array_distance is the
//! L2 metric the M2a component builds with; the cosine / inner-product variants
//! are accepted too (the component's instant-distance map is L2, so they will
//! search the same map -- correctness for those is best-effort, but the match is
//! cheap and the success path is array_distance).
bool IsDistanceFunctionName(const string &name) {
	return name == "array_distance" || name == "array_cosine_distance" || name == "array_inner_product" ||
	       name == "array_negative_inner_product";
}

//! Pull (col_ref, const_value) out of a binary distance BoundFunctionExpression,
//! in either argument order. Returns false unless exactly one child is a
//! BOUND_COLUMN_REF and the other a VALUE_CONSTANT.
bool MatchDistanceArgs(BoundFunctionExpression &func, BoundColumnRefExpression *&out_col, const Value *&out_const) {
	if (func.children.size() != 2) {
		return false;
	}
	auto &a = func.children[0];
	auto &b = func.children[1];

	BoundColumnRefExpression *col = nullptr;
	const Value *constant = nullptr;
	if (a->type == ExpressionType::BOUND_COLUMN_REF && b->type == ExpressionType::VALUE_CONSTANT) {
		col = &a->Cast<BoundColumnRefExpression>();
		constant = &b->Cast<BoundConstantExpression>().value;
	} else if (b->type == ExpressionType::BOUND_COLUMN_REF && a->type == ExpressionType::VALUE_CONSTANT) {
		col = &b->Cast<BoundColumnRefExpression>();
		constant = &a->Cast<BoundConstantExpression>().value;
	} else {
		return false;
	}
	out_col = col;
	out_const = constant;
	return true;
}

class WasmIndexScanOptimizer : public OptimizerExtension {
public:
	WasmIndexScanOptimizer() {
		optimize_function = Optimize;
	}

	static bool TryOptimize(ClientContext &context, unique_ptr<LogicalOperator> &plan) {
		auto &op = *plan;

		// 1. TOP_N (the collapsed ORDER BY ... LIMIT k), single ASC order key.
		if (op.type != LogicalOperatorType::LOGICAL_TOP_N) {
			return false;
		}
		auto &top_n = op.Cast<LogicalTopN>();
		if (top_n.orders.size() != 1 || top_n.offset != 0) {
			return false;
		}
		const auto &order = top_n.orders[0];
		if (order.type != OrderType::ASCENDING) {
			return false;
		}
		if (order.expression->type != ExpressionType::BOUND_COLUMN_REF) {
			return false;
		}
		const auto &order_ref = order.expression->Cast<BoundColumnRefExpression>();

		// 2. child must be a PROJECTION; the order key references one of its exprs.
		if (top_n.children.size() != 1 ||
		    top_n.children.front()->type != LogicalOperatorType::LOGICAL_PROJECTION) {
			return false;
		}
		auto &projection = top_n.children.front()->Cast<LogicalProjection>();
		const auto proj_index = order_ref.binding.column_index;
		if (proj_index >= projection.expressions.size()) {
			return false;
		}
		auto &proj_expr = projection.expressions[proj_index];

		// 3. the referenced projection expr must be a distance function.
		if (proj_expr->type != ExpressionType::BOUND_FUNCTION) {
			return false;
		}
		auto &dist_func = proj_expr->Cast<BoundFunctionExpression>();
		if (!IsDistanceFunctionName(dist_func.function.name)) {
			return false;
		}
		BoundColumnRefExpression *col_ref = nullptr;
		const Value *const_vec = nullptr;
		if (!MatchDistanceArgs(dist_func, col_ref, const_vec)) {
			return false;
		}

		// 4. projection must sit directly on a seq_scan GET.
		if (projection.children.size() != 1 ||
		    projection.children.front()->type != LogicalOperatorType::LOGICAL_GET) {
			return false;
		}
		auto &get_ptr = projection.children.front();
		auto &get = get_ptr->Cast<LogicalGet>();
		if (get.function.name != "seq_scan") {
			return false;
		}
		if (!get.table_filters.filters.empty()) {
			// keep it simple: don't combine with pushed-down filters in M2b.
			return false;
		}
		auto table = get.GetTable();
		if (!table || !table->IsDuckTable()) {
			return false;
		}

		// The distance col-ref's column_index is the position into the GET's
		// selected column list; map it to the real (storage) table column id.
		const auto &get_columns = get.GetColumnIds();
		if (col_ref->binding.column_index >= get_columns.size()) {
			return false;
		}
		const column_t indexed_table_column = get_columns[col_ref->binding.column_index].GetPrimaryIndex();

		// 5. find the WasmBoundIndex on this table whose key column matches.
		auto &table_info = *table->GetStorage().GetDataTableInfo();
		WasmBoundIndex *wasm_index = nullptr;
		for (auto &index : table_info.GetIndexes().Indexes()) {
			if (index.GetIndexType() != WASM_HNSW_INDEX_TYPE) {
				continue;
			}
			const auto &index_cols = index.GetColumnIds();
			if (index_cols.size() != 1 || index_cols[0] != indexed_table_column) {
				continue;
			}
			wasm_index = &index.Cast<WasmBoundIndex>();
			break;
		}
		if (!wasm_index || wasm_index->wasm_handle == 0) {
			return false;
		}

		// 6. extract the constant query vector (FLOAT[N]) and k.
		if (const_vec->IsNull()) {
			return false;
		}
		const auto &vec_children = ArrayValue::GetChildren(*const_vec);
		const uint32_t dims = wasm_index->dims;
		if (dims == 0 || vec_children.size() != dims) {
			return false;
		}
		std::vector<float> query(dims);
		for (uint32_t i = 0; i < dims; i++) {
			query[i] = vec_children[i].GetValue<float>();
		}
		const idx_t k = (idx_t)top_n.limit;
		if (k == 0) {
			return false;
		}
		// The wasm index returns at most STANDARD_VECTOR_SIZE rows per fetch chunk;
		// like vss, keep the swap simple by requiring k to fit a single vector.
		if (k >= STANDARD_VECTOR_SIZE) {
			return false;
		}
		if (!get.table_filters.filters.empty()) {
			// the index scan does not support pushed-down table filters; M2b match
			// already required none, so this is just a guard.
			return false;
		}

		// 7. REWRITE (vss-style): swap the GET's table function to the wasm HNSW
		//    index scan + carry a WasmIndexScanBindData. The index scan's
		//    init_global FIRES the search and returns the k nearest rows IN
		//    DISTANCE ORDER (nearest-first); execute fetches them by rowid. So the
		//    plan becomes a clean index scan with NO outer sort and NO rowid filter.
		fprintf(stderr, "[wasm_index] optimizer swapped seq_scan -> wasm_hnsw_index_scan (handle=%u k=%llu)\n",
		        wasm_index->wasm_handle, (unsigned long long)k);

		auto &duck_table = table->Cast<DuckTableEntry>();
		auto bind_data = make_uniq<WasmIndexScanBindData>(duck_table, wasm_index->wasm_handle, dims, k, std::move(query));

		get.function = WasmIndexScanFunction::GetFunction();
		auto cardinality = get.function.cardinality(context, bind_data.get());
		get.has_estimated_cardinality = cardinality->has_estimated_cardinality;
		get.estimated_cardinality = cardinality->estimated_cardinality;
		get.bind_data = std::move(bind_data);

		// Drop the TOP_N: the index scan already returns exactly k rows in distance
		// order. The PROJECTION (which computes the output columns, including the
		// distance expr) becomes the new subtree root -- correct results, no resort.
		plan = std::move(top_n.children[0]);
		return true;
	}

	static bool OptimizeChildren(ClientContext &context, unique_ptr<LogicalOperator> &plan) {
		auto ok = TryOptimize(context, plan);
		for (auto &child : plan->children) {
			ok = OptimizeChildren(context, child) || ok;
		}
		return ok;
	}

	static void Optimize(OptimizerExtensionInput &input, unique_ptr<LogicalOperator> &plan) {
		OptimizeChildren(input.context, plan);
	}
};

} // namespace

//! Install the M2b optimizer rule on `instance`. Idempotent: a process-wide guard
//! avoids stacking duplicate rules (wasm_register_index_type runs per query).
void wasm_register_index_optimizer(DatabaseInstance &instance) {
	static bool registered = false;
	if (registered) {
		return;
	}
	registered = true;
	auto &config = DBConfig::GetConfig(instance);
	OptimizerExtension::Register(config, WasmIndexScanOptimizer());
	fprintf(stderr, "[wasm_index] registered top-k distance optimizer rule\n");
}

} // namespace duckdb
