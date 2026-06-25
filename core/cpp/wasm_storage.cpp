//===----------------------------------------------------------------------===//
// wasm_storage.cpp
//
// M2a: read-only foreign-catalog StorageExtension for the wasm core.
//
// `ATTACH 'file.sqlite' (TYPE sqlitewasm) AS db;` dispatches here. This TU
// subclasses the DuckDB-internal Catalog / SchemaCatalogEntry / TableCatalogEntry
// / TransactionManager (modeled exactly on sqlite_scanner's storage/* classes)
// to enumerate the foreign DB's schema. The metadata round-trips to the
// sqlitewasm WIT component through the extern-C bridge (wasm_storage_*, defined
// in Rust core/src/lib.rs), which routes to the host's storage-host import.
//
// SCAN is NOT implemented yet (M2b): WasmTableEntry::GetScanFunction throws
// NotImplementedException.
//
// Compiled in-core (DUCKDB_BUILD_LIBRARY) with the exact wasi-sdk flags
// extracted from sqlite_scanner's build (see core/build.rs).
//===----------------------------------------------------------------------===//

#include "duckdb.hpp"
#include "duckdb.h"

#include "duckdb/storage/storage_extension.hpp"
#include "duckdb/parser/parsed_data/attach_info.hpp"
#include "duckdb/parser/parsed_data/create_schema_info.hpp"
#include "duckdb/parser/parsed_data/create_table_info.hpp"
#include "duckdb/main/capi/capi_internal.hpp"
#include "duckdb/main/database.hpp"
#include "duckdb/main/attached_database.hpp"
#include "duckdb/main/config.hpp"
#include "duckdb/catalog/catalog.hpp"
#include "duckdb/catalog/catalog_entry/schema_catalog_entry.hpp"
#include "duckdb/catalog/catalog_entry/table_catalog_entry.hpp"
#include "duckdb/storage/database_size.hpp"
#include "duckdb/storage/table_storage_info.hpp"
#include "duckdb/transaction/transaction.hpp"
#include "duckdb/transaction/transaction_manager.hpp"
#include "duckdb/common/exception.hpp"
#include "duckdb/common/case_insensitive_map.hpp"
#include "duckdb/common/reference_map.hpp"
#include "duckdb/common/mutex.hpp"
#include "duckdb/function/table_function.hpp"
#include "duckdb/planner/table_filter.hpp"
#include "duckdb/planner/filter/constant_filter.hpp"
#include "duckdb/common/types/value.hpp"
#include "duckdb/common/enums/expression_type.hpp"

#include "wasm_storage_bridge.h"

#include <string>
#include <vector>

// extern-C bridge (implemented in Rust, core/src/lib.rs). Routes to the
// host-provided storage-host import -> sqlitewasm component storage-dispatch.
// The enumeration + scan-bridge prototypes live in wasm_storage_bridge.h.

namespace duckdb {

//===----------------------------------------------------------------------===//
// helpers
//===----------------------------------------------------------------------===//

static std::string WasmStorageLastError() {
	const char *msg = wasm_storage_last_error();
	return msg ? std::string(msg) : std::string("unknown wasm storage error");
}

//! Maps a duckdb_type enum code (returned by the bridge) to a LogicalType.
static LogicalType WasmTypeCodeToLogical(uint32_t code) {
	// Codes are the duckdb_type enum values produced by the Rust bridge's
	// storage_logicaltype_to_code; keep this switch in lock-step with it so the
	// full rich type set (not just the original 6) round-trips into a storage
	// table's declared column types.
	switch (code) {
	case 1: // DUCKDB_TYPE_BOOLEAN
		return LogicalType::BOOLEAN;
	case 2: // DUCKDB_TYPE_TINYINT
		return LogicalType::TINYINT;
	case 3: // DUCKDB_TYPE_SMALLINT
		return LogicalType::SMALLINT;
	case 4: // DUCKDB_TYPE_INTEGER
		return LogicalType::INTEGER;
	case 5: // DUCKDB_TYPE_BIGINT
		return LogicalType::BIGINT;
	case 6: // DUCKDB_TYPE_UTINYINT
		return LogicalType::UTINYINT;
	case 7: // DUCKDB_TYPE_USMALLINT
		return LogicalType::USMALLINT;
	case 8: // DUCKDB_TYPE_UINTEGER
		return LogicalType::UINTEGER;
	case 9: // DUCKDB_TYPE_UBIGINT
		return LogicalType::UBIGINT;
	case 10: // DUCKDB_TYPE_FLOAT
		return LogicalType::FLOAT;
	case 11: // DUCKDB_TYPE_DOUBLE
		return LogicalType::DOUBLE;
	case 12: // DUCKDB_TYPE_TIMESTAMP
		return LogicalType::TIMESTAMP;
	case 13: // DUCKDB_TYPE_DATE
		return LogicalType::DATE;
	case 14: // DUCKDB_TYPE_TIME
		return LogicalType::TIME;
	case 15: // DUCKDB_TYPE_INTERVAL
		return LogicalType::INTERVAL;
	case 17: // DUCKDB_TYPE_VARCHAR
		return LogicalType::VARCHAR;
	case 18: // DUCKDB_TYPE_BLOB
		return LogicalType::BLOB;
	case 19: // DUCKDB_TYPE_DECIMAL -- the bridge code can't carry width/scale, so
		// declare a wide default; the actual values are re-read with full
		// precision via the underlying parquet/delta readers.
		return LogicalType::DECIMAL(38, 9);
	case 24: // DUCKDB_TYPE_LIST -- escape-hatch best-effort: the bridge code can't
		// carry the child type, so default to LIST(VARCHAR).
		return LogicalType::LIST(LogicalType::VARCHAR);
	case 27: // DUCKDB_TYPE_UUID
		return LogicalType::UUID;
	case 31: // DUCKDB_TYPE_TIMESTAMP_TZ
		return LogicalType::TIMESTAMP_TZ;
	default:
		return LogicalType::VARCHAR;
	}
}

//! Splits a '\n'-joined bridge string into its lines (empty input -> empty).
static vector<std::string> WasmSplitLines(const char *raw) {
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

//===----------------------------------------------------------------------===//
// forward declarations
//===----------------------------------------------------------------------===//

class WasmCatalog;
class WasmSchemaEntry;
class WasmTableEntry;
class WasmTransaction;
class WasmTransactionManager;

//===----------------------------------------------------------------------===//
// WasmTableEntry
//===----------------------------------------------------------------------===//

class WasmTableEntry : public TableCatalogEntry {
public:
	WasmTableEntry(Catalog &catalog, SchemaCatalogEntry &schema, CreateTableInfo &info)
	    : TableCatalogEntry(catalog, schema, info) {
	}

	unique_ptr<BaseStatistics> GetStatistics(ClientContext &context, column_t column_id) override {
		return nullptr;
	}

	TableFunction GetScanFunction(ClientContext &context, unique_ptr<FunctionData> &bind_data) override;

	TableStorageInfo GetStorageInfo(ClientContext &context) override {
		TableStorageInfo info;
		return info;
	}
};

//===----------------------------------------------------------------------===//
// WasmSchemaEntry
//===----------------------------------------------------------------------===//

class WasmSchemaEntry : public SchemaCatalogEntry {
public:
	WasmSchemaEntry(Catalog &catalog, CreateSchemaInfo &info) : SchemaCatalogEntry(catalog, info) {
	}

	//! Lazily builds + caches WasmTableEntry instances by name.
	optional_ptr<CatalogEntry> GetOrLoadTable(const string &table_name);

	// --- read-only: all mutators throw ---
	optional_ptr<CatalogEntry> CreateTable(CatalogTransaction transaction, BoundCreateTableInfo &info) override {
		throw BinderException("wasm storage is read-only: cannot create tables");
	}
	optional_ptr<CatalogEntry> CreateFunction(CatalogTransaction transaction, CreateFunctionInfo &info) override {
		throw BinderException("wasm storage is read-only");
	}
	optional_ptr<CatalogEntry> CreateIndex(CatalogTransaction transaction, CreateIndexInfo &info,
	                                       TableCatalogEntry &table) override {
		throw BinderException("wasm storage is read-only");
	}
	optional_ptr<CatalogEntry> CreateView(CatalogTransaction transaction, CreateViewInfo &info) override {
		throw BinderException("wasm storage is read-only");
	}
	optional_ptr<CatalogEntry> CreateSequence(CatalogTransaction transaction, CreateSequenceInfo &info) override {
		throw BinderException("wasm storage is read-only");
	}
	optional_ptr<CatalogEntry> CreateTableFunction(CatalogTransaction transaction,
	                                                CreateTableFunctionInfo &info) override {
		throw BinderException("wasm storage is read-only");
	}
	optional_ptr<CatalogEntry> CreateCopyFunction(CatalogTransaction transaction,
	                                               CreateCopyFunctionInfo &info) override {
		throw BinderException("wasm storage is read-only");
	}
	optional_ptr<CatalogEntry> CreatePragmaFunction(CatalogTransaction transaction,
	                                                 CreatePragmaFunctionInfo &info) override {
		throw BinderException("wasm storage is read-only");
	}
	optional_ptr<CatalogEntry> CreateCollation(CatalogTransaction transaction, CreateCollationInfo &info) override {
		throw BinderException("wasm storage is read-only");
	}
	optional_ptr<CatalogEntry> CreateType(CatalogTransaction transaction, CreateTypeInfo &info) override {
		throw BinderException("wasm storage is read-only");
	}
	void Alter(CatalogTransaction transaction, AlterInfo &info) override {
		throw BinderException("wasm storage is read-only");
	}
	void DropEntry(ClientContext &context, DropInfo &info) override {
		throw BinderException("wasm storage is read-only");
	}

	void Scan(ClientContext &context, CatalogType type,
	          const std::function<void(CatalogEntry &)> &callback) override;
	void Scan(CatalogType type, const std::function<void(CatalogEntry &)> &callback) override {
		throw InternalException("wasm storage: committed-only Scan is unsupported");
	}

	optional_ptr<CatalogEntry> LookupEntry(CatalogTransaction transaction,
	                                       const EntryLookupInfo &lookup_info) override;

private:
	mutex entry_lock;
	case_insensitive_map_t<unique_ptr<CatalogEntry>> tables;
};

//===----------------------------------------------------------------------===//
// WasmCatalog
//===----------------------------------------------------------------------===//

class WasmCatalog : public Catalog {
public:
	WasmCatalog(AttachedDatabase &db_p, const string &path)
	    : Catalog(db_p), path(path), catalog_handle(0) {
		catalog_handle = wasm_storage_attach(path.c_str());
		if (catalog_handle == 0) {
			throw IOException("wasm storage attach failed for '%s': %s", path, WasmStorageLastError());
		}
	}

	void Initialize(bool load_builtin) override {
		CreateSchemaInfo info;
		info.schema = DEFAULT_SCHEMA;
		main_schema = make_uniq<WasmSchemaEntry>(*this, info);
	}

	string GetCatalogType() override {
		return "sqlitewasm";
	}

	uint32_t GetCatalogHandle() const {
		return catalog_handle;
	}

	WasmSchemaEntry &GetMainSchema() {
		return *main_schema;
	}

	optional_ptr<CatalogEntry> CreateSchema(CatalogTransaction transaction, CreateSchemaInfo &info) override {
		throw BinderException("wasm storage does not support creating schemas");
	}

	void ScanSchemas(ClientContext &context, std::function<void(SchemaCatalogEntry &)> callback) override {
		callback(*main_schema);
	}

	optional_ptr<SchemaCatalogEntry> LookupSchema(CatalogTransaction transaction,
	                                              const EntryLookupInfo &schema_lookup,
	                                              OnEntryNotFound if_not_found) override {
		auto &schema_name = schema_lookup.GetEntryName();
		if (schema_name == DEFAULT_SCHEMA || schema_name == INVALID_SCHEMA || schema_name.empty()) {
			return main_schema.get();
		}
		if (if_not_found == OnEntryNotFound::RETURN_NULL) {
			return nullptr;
		}
		throw BinderException("wasm storage databases only have a single schema - \"%s\"",
		                      std::string(DEFAULT_SCHEMA));
	}

	PhysicalOperator &PlanCreateTableAs(ClientContext &context, PhysicalPlanGenerator &planner, LogicalCreateTable &op,
	                                    PhysicalOperator &plan) override {
		throw NotImplementedException("wasm storage is read-only: CREATE TABLE AS not supported");
	}
	PhysicalOperator &PlanInsert(ClientContext &context, PhysicalPlanGenerator &planner, LogicalInsert &op,
	                             optional_ptr<PhysicalOperator> plan) override {
		throw NotImplementedException("wasm storage is read-only: INSERT not supported");
	}
	PhysicalOperator &PlanDelete(ClientContext &context, PhysicalPlanGenerator &planner, LogicalDelete &op,
	                             PhysicalOperator &plan) override {
		throw NotImplementedException("wasm storage is read-only: DELETE not supported");
	}
	PhysicalOperator &PlanUpdate(ClientContext &context, PhysicalPlanGenerator &planner, LogicalUpdate &op,
	                             PhysicalOperator &plan) override {
		throw NotImplementedException("wasm storage is read-only: UPDATE not supported");
	}
	unique_ptr<LogicalOperator> BindCreateIndex(Binder &binder, CreateStatement &stmt, TableCatalogEntry &table,
	                                            unique_ptr<LogicalOperator> plan) override {
		throw NotImplementedException("wasm storage is read-only: CREATE INDEX not supported");
	}

	DatabaseSize GetDatabaseSize(ClientContext &context) override {
		DatabaseSize result;
		result.total_blocks = 0;
		result.block_size = 0;
		result.free_blocks = 0;
		result.used_blocks = 0;
		result.bytes = 0;
		result.wal_size = idx_t(-1);
		return result;
	}

	bool InMemory() override {
		return false;
	}

	string GetDBPath() override {
		return path;
	}

	string path;

private:
	void DropSchema(ClientContext &context, DropInfo &info) override {
		throw BinderException("wasm storage does not support dropping schemas");
	}

	uint32_t catalog_handle;
	unique_ptr<WasmSchemaEntry> main_schema;
};

//===----------------------------------------------------------------------===//
// WasmSchemaEntry method bodies (need WasmCatalog complete)
//===----------------------------------------------------------------------===//

optional_ptr<CatalogEntry> WasmSchemaEntry::GetOrLoadTable(const string &table_name) {
	lock_guard<mutex> guard(entry_lock);
	auto it = tables.find(table_name);
	if (it != tables.end()) {
		return it->second.get();
	}

	auto &wasm_catalog = catalog.Cast<WasmCatalog>();
	char *raw = wasm_storage_table_columns(wasm_catalog.GetCatalogHandle(), table_name.c_str());
	if (!raw) {
		// Unknown table (or error). Treat as not-found so the binder reports cleanly.
		return nullptr;
	}
	std::string columns_blob(raw);
	wasm_storage_free(raw);

	CreateTableInfo info(*this, table_name);
	for (auto &line : WasmSplitLines(columns_blob.c_str())) {
		auto tab = line.find('\t');
		if (tab == std::string::npos) {
			continue;
		}
		std::string col_name = line.substr(0, tab);
		uint32_t code = static_cast<uint32_t>(std::stoul(line.substr(tab + 1)));
		ColumnDefinition col(col_name, WasmTypeCodeToLogical(code));
		info.columns.AddColumn(std::move(col));
	}
	if (info.columns.LogicalColumnCount() == 0) {
		return nullptr;
	}

	auto entry = make_uniq<WasmTableEntry>(catalog, *this, info);
	auto entry_ptr = entry.get();
	tables[table_name] = std::move(entry);
	return entry_ptr;
}

void WasmSchemaEntry::Scan(ClientContext &context, CatalogType type,
                           const std::function<void(CatalogEntry &)> &callback) {
	if (type != CatalogType::TABLE_ENTRY) {
		return;
	}
	auto &wasm_catalog = catalog.Cast<WasmCatalog>();
	char *raw = wasm_storage_list_tables(wasm_catalog.GetCatalogHandle());
	if (!raw) {
		throw IOException("wasm storage list-tables failed: %s", WasmStorageLastError());
	}
	std::string tables_blob(raw);
	wasm_storage_free(raw);

	for (auto &table_name : WasmSplitLines(tables_blob.c_str())) {
		auto entry = GetOrLoadTable(table_name);
		if (entry) {
			callback(*entry);
		}
	}
}

optional_ptr<CatalogEntry> WasmSchemaEntry::LookupEntry(CatalogTransaction transaction,
                                                        const EntryLookupInfo &lookup_info) {
	switch (lookup_info.GetCatalogType()) {
	case CatalogType::TABLE_ENTRY:
		return GetOrLoadTable(lookup_info.GetEntryName());
	default:
		return nullptr;
	}
}

//===----------------------------------------------------------------------===//
// WasmTransaction
//===----------------------------------------------------------------------===//

class WasmTransaction : public Transaction {
public:
	WasmTransaction(TransactionManager &manager, ClientContext &context) : Transaction(manager, context) {
	}
};

//===----------------------------------------------------------------------===//
// WasmTransactionManager
//===----------------------------------------------------------------------===//

class WasmTransactionManager : public TransactionManager {
public:
	WasmTransactionManager(AttachedDatabase &db_p, WasmCatalog &wasm_catalog)
	    : TransactionManager(db_p), wasm_catalog(wasm_catalog) {
	}

	Transaction &StartTransaction(ClientContext &context) override {
		auto transaction = make_uniq<WasmTransaction>(*this, context);
		auto &result = *transaction;
		lock_guard<mutex> l(transaction_lock);
		transactions[result] = std::move(transaction);
		return result;
	}

	ErrorData CommitTransaction(ClientContext &context, Transaction &transaction) override {
		lock_guard<mutex> l(transaction_lock);
		transactions.erase(transaction);
		return ErrorData();
	}

	void RollbackTransaction(Transaction &transaction) override {
		lock_guard<mutex> l(transaction_lock);
		transactions.erase(transaction);
	}

	void Checkpoint(ClientContext &context, bool force = false) override {
		// read-only: nothing to checkpoint.
	}

private:
	WasmCatalog &wasm_catalog;
	mutex transaction_lock;
	reference_map_t<Transaction, unique_ptr<WasmTransaction>> transactions;
};

//===----------------------------------------------------------------------===//
// StorageExtension wiring
//===----------------------------------------------------------------------===//

static unique_ptr<Catalog> WasmAttach(optional_ptr<StorageExtensionInfo> storage_info, ClientContext &context,
                                      AttachedDatabase &db, const string &name, AttachInfo &info,
                                      AttachOptions &options) {
	return make_uniq<WasmCatalog>(db, info.path);
}

static unique_ptr<TransactionManager> WasmCreateTransactionManager(optional_ptr<StorageExtensionInfo> storage_info,
                                                                   AttachedDatabase &db, Catalog &catalog) {
	auto &wasm_catalog = catalog.Cast<WasmCatalog>();
	return make_uniq<WasmTransactionManager>(db, wasm_catalog);
}

struct WasmStorageExtension : public StorageExtension {
	WasmStorageExtension() {
		attach = WasmAttach;
		create_transaction_manager = WasmCreateTransactionManager;
	}
};

//===----------------------------------------------------------------------===//
// M2b scan: a real TableFunction with engine-driven projection + filter pushdown.
//
// bind:     produce names+types from the WasmTableEntry's ColumnList; stash the
//           catalog handle + table name + full column type list.
// init:     read input.column_ids (projection) + input.filters (a TableFilterSet);
//           convert to the bridge's scan-request and open a component-side scan.
// function: pull rows from the component into the output DataChunk until EOF.
//===----------------------------------------------------------------------===//

//! Shared scan descriptor stashed on the TableFunction (function_info) so bind
//! can recover the catalog handle + table column list. Lives independently of
//! the per-bind FunctionData.
struct WasmScanInfo : public TableFunctionInfo {
	uint32_t catalog_handle = 0;
	string table_name;
	vector<string> names;
	vector<LogicalType> types;
};

//! Per-bind data: the table's catalog handle + name + column list, used by init
//! to build the scan-request and by function to know the projected column types.
struct WasmScanBindData : public TableFunctionData {
	uint32_t catalog_handle = 0;
	string table_name;
	vector<string> names;
	vector<LogicalType> types;
};

struct WasmScanGlobalState : public GlobalTableFunctionState {
	//! Component-side scan handle (0 == none / exhausted-at-open).
	uint32_t scan_handle = 0;
	bool finished = false;

	~WasmScanGlobalState() override {
		if (scan_handle != 0) {
			wasm_storage_scan_close(scan_handle);
			scan_handle = 0;
		}
	}

	idx_t MaxThreads() const override {
		return 1;
	}
};

//! Resolve a projected column position (an index INTO column_ids) back to the
//! real table column index, skipping virtual/rowid columns.
static bool WasmResolveTableColumn(const vector<column_t> &column_ids, idx_t projected_pos,
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

//! Map a DuckDB comparison ExpressionType to a bridge compare-op code.
//! Returns false for comparisons we don't push.
static bool WasmMapCompareOp(ExpressionType type, uint8_t &out_op) {
	switch (type) {
	case ExpressionType::COMPARE_EQUAL:
		out_op = WASM_SCAN_OP_EQ;
		return true;
	case ExpressionType::COMPARE_NOTEQUAL:
		out_op = WASM_SCAN_OP_NE;
		return true;
	case ExpressionType::COMPARE_LESSTHAN:
		out_op = WASM_SCAN_OP_LT;
		return true;
	case ExpressionType::COMPARE_LESSTHANOREQUALTO:
		out_op = WASM_SCAN_OP_LE;
		return true;
	case ExpressionType::COMPARE_GREATERTHAN:
		out_op = WASM_SCAN_OP_GT;
		return true;
	case ExpressionType::COMPARE_GREATERTHANOREQUALTO:
		out_op = WASM_SCAN_OP_GE;
		return true;
	default:
		return false;
	}
}

//! Fill a WasmScanFilter's tagged value from a DuckDB Value. `text_storage`
//! keeps any VARCHAR alive for the scan-open call. Returns false on a type we
//! don't ship (caller skips the predicate).
static bool WasmFillFilterValue(const Value &constant, WasmScanFilter &out, string &text_storage) {
	if (constant.IsNull()) {
		// A NULL constant in a comparison never matches; skip (best-effort).
		return false;
	}
	switch (constant.type().id()) {
	case LogicalTypeId::BOOLEAN:
		out.value_type = WASM_SCAN_VAL_BOOLEAN;
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
		out.value_type = WASM_SCAN_VAL_INT64;
		out.i64 = constant.GetValue<int64_t>();
		return true;
	case LogicalTypeId::FLOAT:
	case LogicalTypeId::DOUBLE:
		out.value_type = WASM_SCAN_VAL_FLOAT64;
		out.f64 = constant.GetValue<double>();
		return true;
	case LogicalTypeId::VARCHAR:
		text_storage = StringValue::Get(constant);
		out.value_type = WASM_SCAN_VAL_TEXT;
		out.text = text_storage.c_str();
		return true;
	default:
		return false;
	}
}

static unique_ptr<FunctionData> WasmScanBind(ClientContext &context, TableFunctionBindInput &input,
                                             vector<LogicalType> &return_types, vector<string> &names) {
	// The bind data is supplied pre-built (via TableFunction::function_info /
	// the named-parameter channel is unused); we instead read it from the
	// extra_info we stash on the TableFunction. DuckDB calls bind with the
	// table's own column list already available through the bind_data we set up
	// in GetScanFunction, so here we just echo names/types from that.
	auto &info = input.info->Cast<WasmScanInfo>();
	for (idx_t i = 0; i < info.names.size(); i++) {
		names.push_back(info.names[i]);
		return_types.push_back(info.types[i]);
	}
	// Return a copy as the function's bind data so init/function can read it.
	auto result = make_uniq<WasmScanBindData>();
	result->catalog_handle = info.catalog_handle;
	result->table_name = info.table_name;
	result->names = info.names;
	result->types = info.types;
	return std::move(result);
}

static unique_ptr<GlobalTableFunctionState> WasmScanInitGlobal(ClientContext &context,
                                                               TableFunctionInitInput &input) {
	auto &bind_data = input.bind_data->Cast<WasmScanBindData>();
	auto state = make_uniq<WasmScanGlobalState>();

	// Projection: column_ids in emit order; map to real table column indices,
	// dropping virtual/rowid columns.
	vector<uint32_t> projection;
	projection.reserve(input.column_ids.size());
	for (idx_t i = 0; i < input.column_ids.size(); i++) {
		uint32_t real_col;
		if (WasmResolveTableColumn(input.column_ids, i, real_col)) {
			projection.push_back(real_col);
		}
	}

	// Filters: input.filters maps (index INTO column_ids) -> TableFilter. Resolve
	// the key back to the real table column index before shipping.
	vector<WasmScanFilter> filters;
	// VARCHAR constants must outlive the scan-open call; keep them parked here.
	vector<string> text_storage;
	if (input.filters) {
		// Pre-size text_storage so c_str() pointers stay stable as we push.
		text_storage.resize(input.filters->filters.size());
		idx_t text_slot = 0;
		for (auto &entry : input.filters->filters) {
			idx_t projected_pos = entry.first;
			uint32_t real_col;
			if (!WasmResolveTableColumn(input.column_ids, projected_pos, real_col)) {
				continue;
			}
			auto &table_filter = *entry.second;
			WasmScanFilter f;
			f.column = real_col;
			f.op = WASM_SCAN_OP_EQ;
			f.value_type = WASM_SCAN_VAL_NONE;
			f.i64 = 0;
			f.f64 = 0.0;
			f.text = nullptr;

			switch (table_filter.filter_type) {
			case TableFilterType::CONSTANT_COMPARISON: {
				auto &cf = table_filter.Cast<ConstantFilter>();
				if (!WasmMapCompareOp(cf.comparison_type, f.op)) {
					continue; // unhandled comparison; DuckDB re-applies
				}
				if (!WasmFillFilterValue(cf.constant, f, text_storage[text_slot])) {
					continue; // unshippable constant type; skip
				}
				text_slot++;
				filters.push_back(f);
				break;
			}
			case TableFilterType::IS_NULL:
				f.op = WASM_SCAN_OP_IS_NULL;
				f.value_type = WASM_SCAN_VAL_NONE;
				filters.push_back(f);
				break;
			case TableFilterType::IS_NOT_NULL:
				f.op = WASM_SCAN_OP_IS_NOT_NULL;
				f.value_type = WASM_SCAN_VAL_NONE;
				filters.push_back(f);
				break;
			default:
				// CONJUNCTION_AND/OR, IN, etc.: skip (best-effort, re-applied).
				break;
			}
		}
	}

	const uint32_t *proj_ptr = projection.empty() ? nullptr : projection.data();
	const WasmScanFilter *filt_ptr = filters.empty() ? nullptr : filters.data();
	uint32_t scan = wasm_storage_scan_open(bind_data.catalog_handle, bind_data.table_name.c_str(),
	                                        proj_ptr, static_cast<uint32_t>(projection.size()),
	                                        filt_ptr, static_cast<uint32_t>(filters.size()),
	                                        /*limit=*/-1);
	if (scan == 0) {
		throw IOException("wasm storage scan-open failed for '%s': %s", bind_data.table_name,
		                  WasmStorageLastError());
	}
	state->scan_handle = scan;
	return std::move(state);
}

static void WasmScanFunction(ClientContext &context, TableFunctionInput &data, DataChunk &output) {
	auto &gstate = data.global_state->Cast<WasmScanGlobalState>();
	if (gstate.finished || gstate.scan_handle == 0) {
		output.SetCardinality(0);
		return;
	}
	auto chunk_handle = reinterpret_cast<void *>(&output);
	bool has_rows = wasm_storage_scan_fill(gstate.scan_handle, chunk_handle);
	if (!has_rows) {
		// EOF: scan-fill already set the chunk size to 0. Surface any error.
		const char *err = wasm_storage_last_error();
		if (err && err[0] != '\0') {
			gstate.finished = true;
			throw IOException("wasm storage scan-fill failed: %s", std::string(err));
		}
		gstate.finished = true;
		output.SetCardinality(0);
	}
}

TableFunction WasmTableEntry::GetScanFunction(ClientContext &context, unique_ptr<FunctionData> &bind_data) {
	auto &wasm_catalog = catalog.Cast<WasmCatalog>();

	// Capture THIS table's column list (real order) for both the stashed
	// function-info (drives bind) and the per-bind data (drives init/function).
	auto info = make_shared_ptr<WasmScanInfo>();
	info->catalog_handle = wasm_catalog.GetCatalogHandle();
	info->table_name = name;
	auto &cols = GetColumns();
	for (auto &col : cols.Logical()) {
		info->names.push_back(col.Name());
		info->types.push_back(col.Type());
	}

	auto data = make_uniq<WasmScanBindData>();
	data->catalog_handle = info->catalog_handle;
	data->table_name = info->table_name;
	data->names = info->names;
	data->types = info->types;

	TableFunction function("wasm_storage_scan", {}, WasmScanFunction, WasmScanBind, WasmScanInitGlobal);
	function.projection_pushdown = true;
	function.filter_pushdown = true;
	function.function_info = std::move(info);

	bind_data = std::move(data);
	return function;
}

} // namespace duckdb

//! Registers the wasm StorageExtension for `type_name` on the given database.
extern "C" void wasm_register_storage_extension(duckdb_database db, const char *type_name) {
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
		duckdb::StorageExtension::Register(config, std::string(type_name),
		                                   duckdb::make_shared_ptr<duckdb::WasmStorageExtension>());
	} catch (const std::exception &e) {
		fprintf(stderr, "wasm_register_storage_extension failed: %s\n", e.what());
	} catch (...) {
		fprintf(stderr, "wasm_register_storage_extension failed: unknown error\n");
	}
}
