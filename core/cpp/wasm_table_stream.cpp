//===----------------------------------------------------------------------===//
// wasm_table_stream.cpp
//
// A real DuckDB streaming TableFunction (filter_pushdown = true) for a component
// that declared a filterable table function via the 3.1.0 additive
// `table-stream.register-filterable-table` marker. The 3.1.0 end-to-end core
// shim: the SQL WHERE's pushed TableFilter set is mapped to the neutral,
// by-value-safe `ts-filter` descriptor (column index + op + constant) and driven
// to the owning component's `table-stream-dispatch.call-table-open-filtered`
// through the Rust core's `table-stream-host` import.
//
// Mirrors wasm_storage.cpp's scan TableFunction (the proven filter-pushdown
// bridge), but for a NAMED function with bound argument values rather than an
// ATTACH-ed catalog. Registration uses the system catalog (CreateTableFunction,
// IGNORE_ON_CONFLICT) -- ExtensionUtil was removed upstream (PR 17772).
//===----------------------------------------------------------------------===//

#include "duckdb.hpp"
#include "duckdb.h"

#include "duckdb/main/capi/capi_internal.hpp"
#include "duckdb/main/database.hpp"
#include "duckdb/catalog/catalog.hpp"
#include "duckdb/catalog/catalog_transaction.hpp"
#include "duckdb/parser/parsed_data/create_table_function_info.hpp"
#include "duckdb/function/table_function.hpp"
#include "duckdb/function/function_set.hpp"
#include "duckdb/planner/table_filter.hpp"
#include "duckdb/planner/filter/constant_filter.hpp"
#include "duckdb/planner/filter/conjunction_filter.hpp"
#include "duckdb/planner/filter/in_filter.hpp"
#include "duckdb/common/types/value.hpp"
#include "duckdb/common/enums/expression_type.hpp"
#include "duckdb/common/enums/on_create_conflict.hpp"
#include "duckdb/common/exception.hpp"

#include "wasm_table_stream_bridge.h"

#include <cstdio>
#include <functional>
#include <string>
#include <vector>

namespace duckdb {

//===----------------------------------------------------------------------===//
// helpers
//===----------------------------------------------------------------------===//

static std::string WasmTsLastError() {
	const char *msg = wasm_table_stream_last_error();
	return msg ? std::string(msg) : std::string("unknown wasm table-stream error");
}

//! duckdb_type enum code (from the Rust bridge's storage_logicaltype_to_code) ->
//! LogicalType. Kept in lock-step with that mapping.
static LogicalType WasmTsTypeCodeToLogical(uint32_t code) {
	switch (code) {
	case 1:
		return LogicalType::BOOLEAN;
	case 2:
		return LogicalType::TINYINT;
	case 3:
		return LogicalType::SMALLINT;
	case 4:
		return LogicalType::INTEGER;
	case 5:
		return LogicalType::BIGINT;
	case 6:
		return LogicalType::UTINYINT;
	case 7:
		return LogicalType::USMALLINT;
	case 8:
		return LogicalType::UINTEGER;
	case 9:
		return LogicalType::UBIGINT;
	case 10:
		return LogicalType::FLOAT;
	case 11:
		return LogicalType::DOUBLE;
	case 12:
		return LogicalType::TIMESTAMP;
	case 13:
		return LogicalType::DATE;
	case 14:
		return LogicalType::TIME;
	case 15:
		return LogicalType::INTERVAL;
	case 17:
		return LogicalType::VARCHAR;
	case 18:
		return LogicalType::BLOB;
	case 19:
		return LogicalType::DECIMAL(38, 9);
	case 24:
		return LogicalType::LIST(LogicalType::VARCHAR);
	case 27:
		return LogicalType::UUID;
	case 31:
		return LogicalType::TIMESTAMP_TZ;
	default:
		return LogicalType::VARCHAR;
	}
}

static vector<std::string> WasmTsSplitLines(const char *raw) {
	vector<std::string> out;
	if (!raw) {
		return out;
	}
	std::string s(raw);
	if (s.empty()) {
		return out;
	}
	size_t start = 0;
	while (true) {
		size_t pos = s.find('\n', start);
		if (pos == std::string::npos) {
			out.push_back(s.substr(start));
			break;
		}
		out.push_back(s.substr(start, pos - start));
		start = pos + 1;
	}
	return out;
}

static vector<std::string> WasmTsSplitComma(const char *raw) {
	vector<std::string> out;
	if (!raw) {
		return out;
	}
	std::string s(raw);
	if (s.empty()) {
		return out;
	}
	size_t start = 0;
	while (true) {
		size_t pos = s.find(',', start);
		if (pos == std::string::npos) {
			out.push_back(s.substr(start));
			break;
		}
		out.push_back(s.substr(start, pos - start));
		start = pos + 1;
	}
	return out;
}

//! Map a DuckDB comparison ExpressionType to a bridge ts-op code.
static bool WasmTsMapCompareOp(ExpressionType type, uint8_t &out_op) {
	switch (type) {
	case ExpressionType::COMPARE_EQUAL:
		out_op = WASM_TS_OP_EQ;
		return true;
	case ExpressionType::COMPARE_NOTEQUAL:
		out_op = WASM_TS_OP_NE;
		return true;
	case ExpressionType::COMPARE_LESSTHAN:
		out_op = WASM_TS_OP_LT;
		return true;
	case ExpressionType::COMPARE_LESSTHANOREQUALTO:
		out_op = WASM_TS_OP_LE;
		return true;
	case ExpressionType::COMPARE_GREATERTHAN:
		out_op = WASM_TS_OP_GT;
		return true;
	case ExpressionType::COMPARE_GREATERTHANOREQUALTO:
		out_op = WASM_TS_OP_GE;
		return true;
	default:
		return false;
	}
}

//! Fill a WasmTsValue from a DuckDB Value; `text_storage` keeps any VARCHAR alive
//! for the open call. Returns false on a type we don't ship.
static bool WasmTsFillValue(const Value &constant, WasmTsValue &out, std::string &text_storage) {
	out.value_type = WASM_TS_VAL_NONE;
	out.i64 = 0;
	out.f64 = 0.0;
	out.text = nullptr;
	if (constant.IsNull()) {
		return false;
	}
	switch (constant.type().id()) {
	case LogicalTypeId::BOOLEAN:
		out.value_type = WASM_TS_VAL_BOOLEAN;
		out.i64 = BooleanValue::Get(constant) ? 1 : 0;
		return true;
	case LogicalTypeId::TINYINT:
	case LogicalTypeId::SMALLINT:
	case LogicalTypeId::INTEGER:
	case LogicalTypeId::BIGINT:
	case LogicalTypeId::UTINYINT:
	case LogicalTypeId::USMALLINT:
	case LogicalTypeId::UINTEGER:
	case LogicalTypeId::UBIGINT:
		out.value_type = WASM_TS_VAL_INT64;
		out.i64 = constant.GetValue<int64_t>();
		return true;
	case LogicalTypeId::FLOAT:
	case LogicalTypeId::DOUBLE:
		out.value_type = WASM_TS_VAL_FLOAT64;
		out.f64 = constant.GetValue<double>();
		return true;
	case LogicalTypeId::VARCHAR:
		text_storage = StringValue::Get(constant);
		out.value_type = WASM_TS_VAL_TEXT;
		out.text = text_storage.c_str();
		return true;
	default:
		return false;
	}
}

//===----------------------------------------------------------------------===//
// function_info + per-bind / global state
//===----------------------------------------------------------------------===//

//! Stashed on the TableFunction (function_info): the global callback handle + the
//! emitted column schema.
struct WasmTsInfo : public TableFunctionInfo {
	uint32_t handle = 0;
	vector<string> names;
	vector<LogicalType> types;
};

//! One bound argument value, owned (text kept alive through init's open call).
struct WasmTsArg {
	uint8_t value_type = WASM_TS_VAL_NONE;
	int64_t i64 = 0;
	double f64 = 0.0;
	std::string text;
};

struct WasmTsBindData : public TableFunctionData {
	uint32_t handle = 0;
	vector<string> names;
	vector<LogicalType> types;
	vector<WasmTsArg> args;
};

struct WasmTsGlobalState : public GlobalTableFunctionState {
	uint32_t handle = 0;
	uint32_t cursor = 0;
	bool finished = false;

	~WasmTsGlobalState() override {
		if (cursor != 0) {
			wasm_table_stream_close(handle, cursor);
			cursor = 0;
		}
	}

	idx_t MaxThreads() const override {
		return 1;
	}
};

//! Resolve a projected position (index INTO column_ids) to the real column index,
//! skipping virtual/rowid columns.
static bool WasmTsResolveColumn(const vector<column_t> &column_ids, idx_t projected_pos,
                                uint32_t &out_real_column) {
	if (projected_pos >= column_ids.size()) {
		return false;
	}
	column_t cid = column_ids[projected_pos];
	if (cid == COLUMN_IDENTIFIER_ROW_ID) {
		return false;
	}
	out_real_column = static_cast<uint32_t>(cid);
	return true;
}

//===----------------------------------------------------------------------===//
// bind / init / function
//===----------------------------------------------------------------------===//

static unique_ptr<FunctionData> WasmTsBind(ClientContext &context, TableFunctionBindInput &input,
                                           vector<LogicalType> &return_types, vector<string> &names) {
	auto &info = input.info->Cast<WasmTsInfo>();
	for (idx_t i = 0; i < info.names.size(); i++) {
		names.push_back(info.names[i]);
		return_types.push_back(info.types[i]);
	}
	auto result = make_uniq<WasmTsBindData>();
	result->handle = info.handle;
	result->names = info.names;
	result->types = info.types;
	// Capture the bound positional argument values (literals like numstream(10)).
	for (auto &val : input.inputs) {
		WasmTsArg a;
		std::string text;
		WasmTsValue tagged;
		if (WasmTsFillValue(val, tagged, text)) {
			a.value_type = tagged.value_type;
			a.i64 = tagged.i64;
			a.f64 = tagged.f64;
			a.text = text;
		}
		result->args.push_back(std::move(a));
	}
	return std::move(result);
}

static unique_ptr<GlobalTableFunctionState> WasmTsInitGlobal(ClientContext &context,
                                                             TableFunctionInitInput &input) {
	auto &bind_data = input.bind_data->Cast<WasmTsBindData>();
	auto state = make_uniq<WasmTsGlobalState>();
	state->handle = bind_data.handle;

	// Bound argument values -> tagged C values.
	vector<WasmTsValue> args;
	args.reserve(bind_data.args.size());
	for (auto &a : bind_data.args) {
		WasmTsValue v;
		v.value_type = a.value_type;
		v.i64 = a.i64;
		v.f64 = a.f64;
		v.text = (a.value_type == WASM_TS_VAL_TEXT) ? a.text.c_str() : nullptr;
		args.push_back(v);
	}

	// Projection: column_ids in emit order -> real column indices.
	vector<uint32_t> projection;
	projection.reserve(input.column_ids.size());
	for (idx_t i = 0; i < input.column_ids.size(); i++) {
		uint32_t real_col;
		if (WasmTsResolveColumn(input.column_ids, i, real_col)) {
			projection.push_back(real_col);
		}
	}

	// Filters: input.filters maps (index INTO column_ids) -> TableFilter. Resolve
	// the key to the real column index and flatten the (possibly conjunctive)
	// filter tree to the neutral by-value descriptor.
	//
	// CORRECTNESS: DuckDB REMOVES the above-scan FILTER for predicates it pushes
	// here, so the scan MUST apply every pushed clause. We therefore recurse
	// through CONJUNCTION_AND (so `v > 2 AND v <= 6` ships as two AND-ed clauses
	// rather than being silently dropped) and translate IN. OPTIONAL / DYNAMIC /
	// BLOOM are optimization-only (DuckDB re-checks the real predicate elsewhere)
	// and OR / arbitrary-expression filters are not pushed into this set.
	struct TsClause {
		uint32_t column = 0;
		uint8_t op = WASM_TS_OP_EQ;
		vector<WasmTsValue> operands; // 0 (null checks) / 1 (comparison) / N (IN)
		vector<std::string> texts;    // owns any VARCHAR operands' storage
	};
	vector<TsClause> clauses;

	std::function<void(uint32_t, const TableFilter &)> collect =
	    [&](uint32_t real_col, const TableFilter &tf) {
		    switch (tf.filter_type) {
		    case TableFilterType::CONSTANT_COMPARISON: {
			    auto &cf = tf.Cast<ConstantFilter>();
			    TsClause c;
			    c.column = real_col;
			    if (!WasmTsMapCompareOp(cf.comparison_type, c.op)) {
				    return;
			    }
			    c.texts.resize(1);
			    WasmTsValue v;
			    if (!WasmTsFillValue(cf.constant, v, c.texts[0])) {
				    return;
			    }
			    c.operands.push_back(v);
			    clauses.push_back(std::move(c));
			    break;
		    }
		    case TableFilterType::IS_NULL: {
			    TsClause c;
			    c.column = real_col;
			    c.op = WASM_TS_OP_IS_NULL;
			    clauses.push_back(std::move(c));
			    break;
		    }
		    case TableFilterType::IS_NOT_NULL: {
			    TsClause c;
			    c.column = real_col;
			    c.op = WASM_TS_OP_IS_NOT_NULL;
			    clauses.push_back(std::move(c));
			    break;
		    }
		    case TableFilterType::CONJUNCTION_AND: {
			    auto &conj = tf.Cast<ConjunctionAndFilter>();
			    for (auto &child : conj.child_filters) {
				    collect(real_col, *child);
			    }
			    break;
		    }
		    case TableFilterType::IN_FILTER: {
			    auto &inf = tf.Cast<InFilter>();
			    TsClause c;
			    c.column = real_col;
			    c.op = WASM_TS_OP_IS_IN;
			    c.texts.resize(inf.values.size());
			    bool ok = true;
			    idx_t i = 0;
			    for (auto &val : inf.values) {
				    WasmTsValue v;
				    if (!WasmTsFillValue(val, v, c.texts[i])) {
					    ok = false;
					    break;
				    }
				    c.operands.push_back(v);
				    i++;
			    }
			    if (ok && !c.operands.empty()) {
				    clauses.push_back(std::move(c));
			    }
			    break;
		    }
		    default:
			    break;
		    }
	    };

	if (input.filters) {
		for (auto &entry : input.filters->filters) {
			idx_t projected_pos = entry.first;
			uint32_t real_col;
			if (!WasmTsResolveColumn(input.column_ids, projected_pos, real_col)) {
				continue;
			}
			collect(real_col, *entry.second);
		}
	}

	// Flatten the owned clauses into a contiguous operand pool + WasmTsFilter
	// array with STABLE pointers: the pool is reserved to the exact operand count
	// (no reallocation), and each VARCHAR operand's `text` is (re-)derived from
	// the owning clause's `texts` -- which now live in the stable `clauses` vector
	// and outlive the open call below.
	idx_t total_operands = 0;
	for (auto &c : clauses) {
		total_operands += c.operands.size();
	}
	vector<WasmTsValue> operand_pool;
	operand_pool.reserve(total_operands);
	vector<WasmTsFilter> filters;
	filters.reserve(clauses.size());
	for (auto &c : clauses) {
		WasmTsFilter f;
		f.column = c.column;
		f.op = c.op;
		f.nvalues = static_cast<uint32_t>(c.operands.size());
		f.values = c.operands.empty() ? nullptr : operand_pool.data() + operand_pool.size();
		for (idx_t i = 0; i < c.operands.size(); i++) {
			WasmTsValue v = c.operands[i];
			if (v.value_type == WASM_TS_VAL_TEXT) {
				v.text = c.texts[i].c_str();
			}
			operand_pool.push_back(v);
		}
		filters.push_back(f);
	}

	const WasmTsValue *args_ptr = args.empty() ? nullptr : args.data();
	const uint32_t *proj_ptr = projection.empty() ? nullptr : projection.data();
	const WasmTsFilter *filt_ptr = filters.empty() ? nullptr : filters.data();

	uint32_t cursor = wasm_table_stream_open(bind_data.handle, args_ptr,
	                                         static_cast<uint32_t>(args.size()), proj_ptr,
	                                         static_cast<uint32_t>(projection.size()), filt_ptr,
	                                         static_cast<uint32_t>(filters.size()));
	if (cursor == 0) {
		throw IOException("wasm table-stream open failed: %s", WasmTsLastError());
	}
	state->cursor = cursor;
	return std::move(state);
}

static void WasmTsFunction(ClientContext &context, TableFunctionInput &data, DataChunk &output) {
	auto &gstate = data.global_state->Cast<WasmTsGlobalState>();
	if (gstate.finished || gstate.cursor == 0) {
		output.SetCardinality(0);
		return;
	}
	auto chunk_handle = reinterpret_cast<void *>(&output);
	bool has_rows = wasm_table_stream_fill(gstate.handle, gstate.cursor, chunk_handle);
	if (!has_rows) {
		const char *err = wasm_table_stream_last_error();
		if (err && err[0] != '\0') {
			gstate.finished = true;
			throw IOException("wasm table-stream fill failed: %s", std::string(err));
		}
		gstate.finished = true;
		output.SetCardinality(0);
	}
}

} // namespace duckdb

//===----------------------------------------------------------------------===//
// registration (C ABI, called from the Rust core's sync_filterable_tables)
//===----------------------------------------------------------------------===//

extern "C" int32_t wasm_register_filterable_table_function(void *db, const char *name, uint32_t handle,
                                                           const char *arg_type_codes,
                                                           const char *cols_spec) {
	using namespace duckdb;
	if (!db || !name) {
		return 1;
	}
	try {
		auto wrapper = reinterpret_cast<DatabaseWrapper *>(db);
		if (!wrapper || !wrapper->database) {
			return 1;
		}
		auto &instance = *wrapper->database->instance;

		// Argument types (positional).
		vector<LogicalType> arg_types;
		for (auto &code : WasmTsSplitComma(arg_type_codes)) {
			if (code.empty()) {
				continue;
			}
			arg_types.push_back(WasmTsTypeCodeToLogical(static_cast<uint32_t>(std::stoul(code))));
		}

		// Emitted column schema: '\n'-joined `name\t<code>` lines.
		auto info = make_shared_ptr<WasmTsInfo>();
		info->handle = handle;
		for (auto &line : WasmTsSplitLines(cols_spec)) {
			size_t tab = line.find('\t');
			if (tab == std::string::npos) {
				continue;
			}
			std::string col_name = line.substr(0, tab);
			uint32_t code = static_cast<uint32_t>(std::stoul(line.substr(tab + 1)));
			info->names.push_back(col_name);
			info->types.push_back(WasmTsTypeCodeToLogical(code));
		}

		std::string fn_name(name);
		TableFunction tf(fn_name, arg_types, WasmTsFunction, WasmTsBind, WasmTsInitGlobal);
		tf.projection_pushdown = true;
		tf.filter_pushdown = true;
		tf.function_info = info;

		CreateTableFunctionInfo create_info(tf);
		create_info.on_conflict = OnCreateConflict::IGNORE_ON_CONFLICT;

		auto &system_catalog = Catalog::GetSystemCatalog(instance);
		auto transaction = CatalogTransaction::GetSystemTransaction(instance);
		system_catalog.CreateTableFunction(transaction, create_info);
		return 0;
	} catch (const std::exception &e) {
		fprintf(stderr, "wasm_register_filterable_table_function failed: %s\n", e.what());
		return 1;
	} catch (...) {
		fprintf(stderr, "wasm_register_filterable_table_function failed: unknown error\n");
		return 1;
	}
}
