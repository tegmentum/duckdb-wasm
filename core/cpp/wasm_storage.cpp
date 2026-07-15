//===----------------------------------------------------------------------===//
// wasm_storage.cpp
//
// M2a: read-only foreign-catalog StorageExtension for the wasm core.
// M2b: engine-driven projection + filter-pushdown scan (below).
// M2c: WRITE surface — transactions + DDL + DML routed to the writable
//      storage component's `storage-write-dispatch` export through the
//      `storage-host` write imports.
//
// `ATTACH 'file.sqlite' (TYPE sqlitewasm) AS db;` dispatches here. This TU
// subclasses the DuckDB-internal Catalog / SchemaCatalogEntry / TableCatalogEntry
// / TransactionManager (modeled exactly on sqlite_scanner's storage/* classes)
// to enumerate the foreign DB's schema. The metadata round-trips to the
// sqlitewasm WIT component through the extern-C bridge (wasm_storage_*, defined
// in Rust core/src/lib.rs), which routes to the host's storage-host import.
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
#include "duckdb/common/types/data_chunk.hpp"
#include "duckdb/common/vector_operations/vector_operations.hpp"
#include "duckdb/execution/physical_operator.hpp"
#include "duckdb/execution/physical_plan_generator.hpp"
#include "duckdb/planner/operator/logical_insert.hpp"
#include "duckdb/planner/operator/logical_update.hpp"
#include "duckdb/planner/operator/logical_delete.hpp"
#include "duckdb/planner/operator/logical_create_table.hpp"
#include "duckdb/planner/parsed_data/bound_create_table_info.hpp"
#include "duckdb/planner/expression/bound_reference_expression.hpp"
#include "duckdb/parser/column_definition.hpp"
#include "duckdb/parser/column_list.hpp"

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

//! Inverse of WasmTypeCodeToLogical -- narrow a LogicalType back to the
//! `duckdb_type` enum code the bridge uses. Reads only the top-level id (rich
//! types collapse to a base code, mirroring how the read side reports them).
static uint32_t WasmLogicalToTypeCode(const LogicalType &type) {
	switch (type.id()) {
	case LogicalTypeId::BOOLEAN:
		return 1;
	case LogicalTypeId::TINYINT:
		return 2;
	case LogicalTypeId::SMALLINT:
		return 3;
	case LogicalTypeId::INTEGER:
		return 4;
	case LogicalTypeId::BIGINT:
		return 5;
	case LogicalTypeId::UTINYINT:
		return 6;
	case LogicalTypeId::USMALLINT:
		return 7;
	case LogicalTypeId::UINTEGER:
		return 8;
	case LogicalTypeId::UBIGINT:
		return 9;
	case LogicalTypeId::FLOAT:
		return 10;
	case LogicalTypeId::DOUBLE:
		return 11;
	case LogicalTypeId::TIMESTAMP:
		return 12;
	case LogicalTypeId::DATE:
		return 13;
	case LogicalTypeId::TIME:
		return 14;
	case LogicalTypeId::INTERVAL:
		return 15;
	case LogicalTypeId::VARCHAR:
		return 17;
	case LogicalTypeId::BLOB:
		return 18;
	case LogicalTypeId::DECIMAL:
		return 19;
	case LogicalTypeId::UUID:
		return 27;
	case LogicalTypeId::TIMESTAMP_TZ:
		return 31;
	default:
		// Fallback to VARCHAR — the Rust side's storage_code_to_logicaltype
		// widens unknown codes to Text as well, so this round-trips cleanly.
		return 17;
	}
}

//! Marshals one flat-vector cell at row `r` into a bridge write-value cell.
//! `text_storage` and `blob_storage` keep the borrowed pointers alive for the
//! duration of the enclosing bridge call. Fields not selected by value_type
//! are left inert (zero-initialized by the caller).
static void WasmMarshalWriteCell(Vector &vec, idx_t r, WasmWriteValue &out,
                                 std::string &text_storage, std::string &blob_storage) {
	auto &validity = FlatVector::Validity(vec);
	if (!validity.RowIsValid(r)) {
		out.value_type = WASM_WRITE_VAL_NONE;
		return;
	}
	auto &type = vec.GetType();
	switch (type.id()) {
	case LogicalTypeId::BOOLEAN: {
		auto data = FlatVector::GetData<bool>(vec);
		out.value_type = WASM_WRITE_VAL_BOOLEAN;
		out.i64 = data[r] ? 1 : 0;
		return;
	}
	case LogicalTypeId::TINYINT: {
		auto data = FlatVector::GetData<int8_t>(vec);
		out.value_type = WASM_WRITE_VAL_INT64;
		out.i64 = static_cast<int64_t>(data[r]);
		return;
	}
	case LogicalTypeId::SMALLINT: {
		auto data = FlatVector::GetData<int16_t>(vec);
		out.value_type = WASM_WRITE_VAL_INT64;
		out.i64 = static_cast<int64_t>(data[r]);
		return;
	}
	case LogicalTypeId::INTEGER: {
		auto data = FlatVector::GetData<int32_t>(vec);
		out.value_type = WASM_WRITE_VAL_INT64;
		out.i64 = static_cast<int64_t>(data[r]);
		return;
	}
	case LogicalTypeId::BIGINT: {
		auto data = FlatVector::GetData<int64_t>(vec);
		out.value_type = WASM_WRITE_VAL_INT64;
		out.i64 = data[r];
		return;
	}
	case LogicalTypeId::UTINYINT: {
		auto data = FlatVector::GetData<uint8_t>(vec);
		out.value_type = WASM_WRITE_VAL_INT64;
		out.i64 = static_cast<int64_t>(data[r]);
		return;
	}
	case LogicalTypeId::USMALLINT: {
		auto data = FlatVector::GetData<uint16_t>(vec);
		out.value_type = WASM_WRITE_VAL_INT64;
		out.i64 = static_cast<int64_t>(data[r]);
		return;
	}
	case LogicalTypeId::UINTEGER: {
		auto data = FlatVector::GetData<uint32_t>(vec);
		out.value_type = WASM_WRITE_VAL_INT64;
		out.i64 = static_cast<int64_t>(data[r]);
		return;
	}
	case LogicalTypeId::UBIGINT: {
		auto data = FlatVector::GetData<uint64_t>(vec);
		out.value_type = WASM_WRITE_VAL_INT64;
		// Best-effort narrow; large values are truncated (the tag is signed).
		out.i64 = static_cast<int64_t>(data[r]);
		return;
	}
	case LogicalTypeId::FLOAT: {
		auto data = FlatVector::GetData<float>(vec);
		out.value_type = WASM_WRITE_VAL_FLOAT64;
		out.f64 = static_cast<double>(data[r]);
		return;
	}
	case LogicalTypeId::DOUBLE: {
		auto data = FlatVector::GetData<double>(vec);
		out.value_type = WASM_WRITE_VAL_FLOAT64;
		out.f64 = data[r];
		return;
	}
	case LogicalTypeId::VARCHAR: {
		auto data = FlatVector::GetData<string_t>(vec);
		text_storage = data[r].GetString();
		out.value_type = WASM_WRITE_VAL_TEXT;
		out.text = text_storage.c_str();
		return;
	}
	case LogicalTypeId::BLOB: {
		auto data = FlatVector::GetData<string_t>(vec);
		blob_storage.assign(data[r].GetDataUnsafe(), data[r].GetSize());
		out.value_type = WASM_WRITE_VAL_BLOB;
		out.blob = reinterpret_cast<const uint8_t *>(blob_storage.data());
		out.blob_len = static_cast<uint32_t>(blob_storage.size());
		return;
	}
	default:
		// Fallback: render the value as TEXT via the Value ToString path.
		// Covers DATE / TIMESTAMP / DECIMAL / UUID etc. -- the writer
		// component can re-parse them from the string form.
		text_storage = vec.GetValue(r).ToString();
		out.value_type = WASM_WRITE_VAL_TEXT;
		out.text = text_storage.c_str();
		return;
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

	// --- write: CreateTable routes to the bridge; other DDL is still stubbed ---
	optional_ptr<CatalogEntry> CreateTable(CatalogTransaction transaction, BoundCreateTableInfo &info) override;
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

	//! Adopts a locally-built WasmTableEntry (used by CreateTable after the
	//! bridge acknowledges the CREATE). Idempotent by name.
	CatalogEntry &InsertTable(unique_ptr<CatalogEntry> entry, const string &table_name);

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
		throw NotImplementedException("wasm storage: CREATE TABLE AS not supported yet");
	}
	PhysicalOperator &PlanInsert(ClientContext &context, PhysicalPlanGenerator &planner, LogicalInsert &op,
	                             optional_ptr<PhysicalOperator> plan) override;
	PhysicalOperator &PlanDelete(ClientContext &context, PhysicalPlanGenerator &planner, LogicalDelete &op,
	                             PhysicalOperator &plan) override;
	PhysicalOperator &PlanUpdate(ClientContext &context, PhysicalPlanGenerator &planner, LogicalUpdate &op,
	                             PhysicalOperator &plan) override;
	unique_ptr<LogicalOperator> BindCreateIndex(Binder &binder, CreateStatement &stmt, TableCatalogEntry &table,
	                                            unique_ptr<LogicalOperator> plan) override {
		throw NotImplementedException("wasm storage: CREATE INDEX not supported yet");
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
//
// Holds the component-side transaction handle returned by
// `wasm_storage_write_begin_transaction`. The physical operators pull the
// handle out via `WasmTransaction::Get(context, catalog)` and pass it to
// insert/update/delete/create-table on every Sink call.
//===----------------------------------------------------------------------===//

class WasmTransaction : public Transaction {
public:
	WasmTransaction(TransactionManager &manager, ClientContext &context, uint32_t catalog_handle_p)
	    : Transaction(manager, context), catalog_handle(catalog_handle_p), txn_handle(0), started(false) {
	}

	//! Lazily open the component-side transaction on first write.
	uint32_t EnsureStarted() {
		if (!started) {
			txn_handle = wasm_storage_write_begin_transaction(catalog_handle);
			if (txn_handle == 0) {
				throw IOException("wasm storage begin-transaction failed: %s", WasmStorageLastError());
			}
			started = true;
		}
		return txn_handle;
	}

	uint32_t TxnHandle() const {
		return txn_handle;
	}

	bool IsStarted() const {
		return started;
	}

	uint32_t CatalogHandle() const {
		return catalog_handle;
	}

	static WasmTransaction &Get(ClientContext &context, Catalog &catalog) {
		return Transaction::Get(context, catalog).Cast<WasmTransaction>();
	}

private:
	uint32_t catalog_handle;
	uint32_t txn_handle;
	bool started;
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
		auto transaction = make_uniq<WasmTransaction>(*this, context, wasm_catalog.GetCatalogHandle());
		auto &result = *transaction;
		lock_guard<mutex> l(transaction_lock);
		transactions[result] = std::move(transaction);
		return result;
	}

	ErrorData CommitTransaction(ClientContext &context, Transaction &transaction) override {
		auto &wtxn = transaction.Cast<WasmTransaction>();
		if (wtxn.IsStarted()) {
			int rc = wasm_storage_write_commit_transaction(wtxn.TxnHandle());
			if (rc != 0) {
				lock_guard<mutex> l(transaction_lock);
				transactions.erase(transaction);
				return ErrorData(ExceptionType::IO,
				                 std::string("wasm storage commit failed: ") + WasmStorageLastError());
			}
		}
		lock_guard<mutex> l(transaction_lock);
		transactions.erase(transaction);
		return ErrorData();
	}

	void RollbackTransaction(Transaction &transaction) override {
		auto &wtxn = transaction.Cast<WasmTransaction>();
		if (wtxn.IsStarted()) {
			// Best-effort: log & drop on error (RollbackTransaction has no
			// error channel).
			int rc = wasm_storage_write_rollback_transaction(wtxn.TxnHandle());
			if (rc != 0) {
				fprintf(stderr, "wasm storage rollback failed: %s\n", WasmStorageLastError().c_str());
			}
		}
		lock_guard<mutex> l(transaction_lock);
		transactions.erase(transaction);
	}

	void Checkpoint(ClientContext &context, bool force = false) override {
		// The writable component owns its own persistence; the wasm host
		// exposes no checkpoint hook, so this is a no-op.
	}

private:
	WasmCatalog &wasm_catalog;
	mutex transaction_lock;
	reference_map_t<Transaction, unique_ptr<WasmTransaction>> transactions;
};

//===----------------------------------------------------------------------===//
// WasmSchemaEntry::CreateTable + InsertTable
//
// The bridge's `wasm_storage_write_create_table` opens (or reuses) the
// component-side transaction on `catalog`, forwards the CREATE, and returns
// success. On success we materialize a WasmTableEntry so subsequent reads /
// writes can bind to it without waiting for the next storage-list-tables
// refresh.
//===----------------------------------------------------------------------===//

CatalogEntry &WasmSchemaEntry::InsertTable(unique_ptr<CatalogEntry> entry, const string &table_name) {
	lock_guard<mutex> guard(entry_lock);
	auto &ref = *entry;
	tables[table_name] = std::move(entry);
	return ref;
}

optional_ptr<CatalogEntry> WasmSchemaEntry::CreateTable(CatalogTransaction transaction,
                                                        BoundCreateTableInfo &info) {
	auto &wasm_catalog = catalog.Cast<WasmCatalog>();
	auto &base_info = info.Base();

	// Reject options we can't honor. The write bridge accepts a plain CREATE.
	if (base_info.on_conflict == OnCreateConflict::REPLACE_ON_CONFLICT) {
		throw NotImplementedException("wasm storage: CREATE OR REPLACE TABLE not supported yet");
	}

	// Ensure a component-side transaction exists (the ExtensionInstance
	// trampolines require an open txn handle).
	if (!transaction.transaction) {
		throw InternalException("wasm storage: CreateTable requires a transaction");
	}
	auto &wtxn = transaction.transaction->Cast<WasmTransaction>();
	uint32_t txn = wtxn.EnsureStarted();

	// Marshal the column list into the bridge's WasmWriteColumn array.
	vector<WasmWriteColumn> cols;
	vector<std::string> name_storage;
	cols.reserve(base_info.columns.LogicalColumnCount());
	name_storage.reserve(base_info.columns.LogicalColumnCount());
	for (idx_t i = 0; i < base_info.columns.LogicalColumnCount(); i++) {
		auto &col = base_info.columns.GetColumn(LogicalIndex(i));
		name_storage.emplace_back(col.GetName());
		WasmWriteColumn entry;
		entry.name = name_storage.back().c_str();
		entry.type_code = WasmLogicalToTypeCode(col.GetType());
		cols.push_back(entry);
	}

	int rc = wasm_storage_write_create_table(txn, base_info.table.c_str(),
	                                          cols.empty() ? nullptr : cols.data(),
	                                          static_cast<uint32_t>(cols.size()));
	if (rc != 0) {
		throw IOException("wasm storage create-table failed for '%s': %s", base_info.table,
		                  WasmStorageLastError());
	}

	// Materialize a WasmTableEntry mirroring the columns we shipped.
	CreateTableInfo materialized(*this, base_info.table);
	for (idx_t i = 0; i < base_info.columns.LogicalColumnCount(); i++) {
		auto &col = base_info.columns.GetColumn(LogicalIndex(i));
		ColumnDefinition new_col(col.GetName(), col.GetType());
		materialized.columns.AddColumn(std::move(new_col));
	}
	auto table_entry = make_uniq<WasmTableEntry>(catalog, *this, materialized);
	auto &inserted = InsertTable(std::move(table_entry), base_info.table);
	return optional_ptr<CatalogEntry>(&inserted);
}

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
	//! Owning WasmTableEntry (as TableCatalogEntry, since WasmTableEntry is not
	//! yet complete at this file scope point). Threaded through WasmScanBind
	//! into WasmScanBindData::table so LogicalGet::GetTable() (via the
	//! `get_bind_info` callback) can resolve the base table during
	//! UPDATE/DELETE binding. Modeled on sqlite_scanner (SQLiteTableEntry sets
	//! `result->table = this;` in GetScanFunction; SqliteBindInfo echoes it).
	optional_ptr<TableCatalogEntry> table;
};

//! Per-bind data: the table's catalog handle + name + column list, used by init
//! to build the scan-request and by function to know the projected column types.
struct WasmScanBindData : public TableFunctionData {
	uint32_t catalog_handle = 0;
	string table_name;
	vector<string> names;
	vector<LogicalType> types;
	//! See WasmScanInfo::table.
	optional_ptr<TableCatalogEntry> table;
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
//! real table column index. Returns TRUE and writes the real column to
//! `out_real_column` for a normal column; returns FALSE for a rowid slot
//! (COLUMN_IDENTIFIER_ROW_ID) — the caller records those OUTPUT-vector
//! positions separately and forwards them to the bridge as `rowid_slots`,
//! so the guest's trailing rowid values are routed to the right DuckDB slot
//! at scan-fill time.
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

//! True iff `projected_pos` in `column_ids` names the virtual rowid column
//! (COLUMN_IDENTIFIER_ROW_ID). Complements `WasmResolveTableColumn`.
static bool WasmIsRowidSlot(const vector<column_t> &column_ids, idx_t projected_pos) {
	if (projected_pos >= column_ids.size()) {
		return false;
	}
	return column_ids[projected_pos] == COLUMN_IDENTIFIER_ROW_ID;
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
	// Thread the base-table pointer through so `get_bind_info` (below) can
	// surface it to LogicalGet::GetTable() during UPDATE/DELETE binding.
	result->table = info.table;
	return std::move(result);
}

//! Mirrors sqlite_scanner::SqliteBindInfo (sqlite_scanner.cpp:333). DuckDB's
//! LogicalGet::GetTable() looks up the base TableCatalogEntry through
//! `function.get_bind_info(bind_data).table`; without this callback the
//! Binder's UPDATE/DELETE resolution ("Can only update/delete base table")
//! rejects the plan because the LogicalGet's table function is our custom
//! `wasm_storage_scan` rather than DuckDB's built-in seq scan.
static BindInfo WasmScanBindInfo(const optional_ptr<FunctionData> bind_data_p) {
	BindInfo info(ScanType::EXTERNAL);
	if (bind_data_p) {
		auto &bind_data = bind_data_p->Cast<WasmScanBindData>();
		info.table = bind_data.table;
	}
	return info;
}

static unique_ptr<GlobalTableFunctionState> WasmScanInitGlobal(ClientContext &context,
                                                               TableFunctionInitInput &input) {
	auto &bind_data = input.bind_data->Cast<WasmScanBindData>();
	auto state = make_uniq<WasmScanGlobalState>();

	// Projection: column_ids in emit order; map to real table column indices,
	// and CAPTURE the OUTPUT-vector positions of rowid slots separately (so the
	// bridge can be told to ask the guest for rowids AND we can route the
	// guest's trailing rowid cell(s) into those DuckDB output vectors at
	// scan-fill time). `projection` stays exclusively real table column
	// indices; `rowid_slots` records the parallel-indexed positions in the
	// caller-side DataChunk that must receive rowid values.
	vector<uint32_t> projection;
	vector<uint32_t> rowid_slots;
	projection.reserve(input.column_ids.size());
	for (idx_t i = 0; i < input.column_ids.size(); i++) {
		uint32_t real_col;
		if (WasmResolveTableColumn(input.column_ids, i, real_col)) {
			projection.push_back(real_col);
		} else if (WasmIsRowidSlot(input.column_ids, i)) {
			rowid_slots.push_back(static_cast<uint32_t>(i));
		}
	}
	const bool wants_rowid = !rowid_slots.empty();

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
	const uint32_t *rowid_slots_ptr = rowid_slots.empty() ? nullptr : rowid_slots.data();
	uint32_t scan = wasm_storage_scan_open(bind_data.catalog_handle, bind_data.table_name.c_str(),
	                                        proj_ptr, static_cast<uint32_t>(projection.size()),
	                                        filt_ptr, static_cast<uint32_t>(filters.size()),
	                                        /*limit=*/-1,
	                                        wants_rowid ? 1 : 0,
	                                        rowid_slots_ptr,
	                                        static_cast<uint32_t>(rowid_slots.size()));
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
	// Stash the base TableCatalogEntry so LogicalGet::GetTable() can resolve
	// it through the `get_bind_info` callback (see WasmScanBindInfo above).
	// Required for UPDATE/DELETE binding.
	info->table = this;
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
	data->table = this;

	TableFunction function("wasm_storage_scan", {}, WasmScanFunction, WasmScanBind, WasmScanInitGlobal);
	function.projection_pushdown = true;
	function.filter_pushdown = true;
	function.get_bind_info = WasmScanBindInfo;
	function.function_info = std::move(info);

	bind_data = std::move(data);
	return function;
}

//===----------------------------------------------------------------------===//
// M2c write physical operators.
//
// Modeled on sqlite_scanner's SQLite{Insert,Update,Delete} (build/duckdb-wasi/
// _deps/sqlite_scanner_extension_fc-src/src/storage/sqlite_{insert,update,
// delete}.cpp). Each subclasses PhysicalOperator, consumes DataChunks in
// Sink(), and reports the total row count in GetData(). The Sink path pulls
// the txn handle out of the WasmTransaction attached to the ClientContext and
// forwards each row batch to the bridge as row-major tagged cells; DuckDB
// serializes writes per catalog, so no cross-thread state is needed.
//===----------------------------------------------------------------------===//

//! Global sink state shared by all three operators. Holds the resolved
//! WasmTableEntry pointer and a running row-affected counter surfaced in
//! GetData().
class WasmWriteGlobalState : public GlobalSinkState {
public:
	explicit WasmWriteGlobalState(WasmTableEntry &table_p) : table(table_p), affected(0) {
	}

	WasmTableEntry &table;
	idx_t affected;
};

//===----------------------------------------------------------------------===//
// WasmPhysicalInsert
//===----------------------------------------------------------------------===//

class WasmPhysicalInsert : public PhysicalOperator {
public:
	WasmPhysicalInsert(PhysicalPlan &physical_plan, LogicalOperator &op, TableCatalogEntry &table)
	    : PhysicalOperator(physical_plan, PhysicalOperatorType::EXTENSION, op.types, 1), table(table) {
	}

	// --- Sink interface ---
	unique_ptr<GlobalSinkState> GetGlobalSinkState(ClientContext &context) const override {
		return make_uniq<WasmWriteGlobalState>(table.Cast<WasmTableEntry>());
	}

	SinkResultType Sink(ExecutionContext &context, DataChunk &chunk, OperatorSinkInput &input) const override {
		auto &gstate = input.global_state.Cast<WasmWriteGlobalState>();
		if (chunk.size() == 0) {
			return SinkResultType::NEED_MORE_INPUT;
		}
		chunk.Flatten();

		auto &wcatalog = gstate.table.catalog.Cast<WasmCatalog>();
		auto &wtxn = WasmTransaction::Get(context.client, gstate.table.catalog);
		uint32_t txn = wtxn.EnsureStarted();
		(void)wcatalog; // txn already carries the catalog handle

		idx_t nrows = chunk.size();
		idx_t ncols = chunk.ColumnCount();
		if (ncols == 0) {
			// No columns to insert -- still report an affected count so
			// GetData() returns the right total.
			gstate.affected += nrows;
			return SinkResultType::NEED_MORE_INPUT;
		}

		vector<WasmWriteValue> flat(nrows * ncols);
		// Text/blob cell contents must outlive the bridge call; park them here.
		vector<std::string> text_park(nrows * ncols);
		vector<std::string> blob_park(nrows * ncols);
		for (idx_t r = 0; r < nrows; r++) {
			for (idx_t c = 0; c < ncols; c++) {
				idx_t slot = r * ncols + c;
				flat[slot] = WasmWriteValue{};
				WasmMarshalWriteCell(chunk.data[c], r, flat[slot], text_park[slot], blob_park[slot]);
			}
		}

		int64_t rc = wasm_storage_write_insert_rows(txn, gstate.table.name.c_str(), flat.data(),
		                                             static_cast<uint32_t>(nrows),
		                                             static_cast<uint32_t>(ncols));
		if (rc < 0) {
			throw IOException("wasm storage insert-rows failed for '%s': %s", gstate.table.name,
			                  WasmStorageLastError());
		}
		gstate.affected += static_cast<idx_t>(rc);
		return SinkResultType::NEED_MORE_INPUT;
	}

	// --- Source interface (produces the "N rows inserted" tuple) ---
	SourceResultType GetDataInternal(ExecutionContext &context, DataChunk &chunk,
	                                 OperatorSourceInput &input) const override {
		auto &gstate = sink_state->Cast<WasmWriteGlobalState>();
		chunk.SetCardinality(1);
		chunk.SetValue(0, 0, Value::BIGINT(static_cast<int64_t>(gstate.affected)));
		return SourceResultType::FINISHED;
	}

	bool IsSink() const override {
		return true;
	}
	bool IsSource() const override {
		return true;
	}

	string GetName() const override {
		return "WASM_INSERT";
	}

	InsertionOrderPreservingMap<string> ParamsToString() const override {
		InsertionOrderPreservingMap<string> result;
		result["Table Name"] = table.name;
		return result;
	}

private:
	TableCatalogEntry &table;
};

//===----------------------------------------------------------------------===//
// WasmPhysicalDelete
//===----------------------------------------------------------------------===//

class WasmPhysicalDelete : public PhysicalOperator {
public:
	WasmPhysicalDelete(PhysicalPlan &physical_plan, LogicalOperator &op, TableCatalogEntry &table, idx_t rowid_index_p)
	    : PhysicalOperator(physical_plan, PhysicalOperatorType::EXTENSION, op.types, 1), table(table),
	      rowid_index(rowid_index_p) {
	}

	unique_ptr<GlobalSinkState> GetGlobalSinkState(ClientContext &context) const override {
		return make_uniq<WasmWriteGlobalState>(table.Cast<WasmTableEntry>());
	}

	SinkResultType Sink(ExecutionContext &context, DataChunk &chunk, OperatorSinkInput &input) const override {
		auto &gstate = input.global_state.Cast<WasmWriteGlobalState>();
		if (chunk.size() == 0) {
			return SinkResultType::NEED_MORE_INPUT;
		}
		chunk.Flatten();

		auto &wtxn = WasmTransaction::Get(context.client, gstate.table.catalog);
		uint32_t txn = wtxn.EnsureStarted();

		auto &rowid_vec = chunk.data[rowid_index];
		auto rowid_data = FlatVector::GetData<int64_t>(rowid_vec);
		vector<int64_t> rowids(rowid_data, rowid_data + chunk.size());

		int64_t rc = wasm_storage_write_delete_rows(txn, gstate.table.name.c_str(), rowids.data(),
		                                             static_cast<uint32_t>(rowids.size()));
		if (rc < 0) {
			throw IOException("wasm storage delete-rows failed for '%s': %s", gstate.table.name,
			                  WasmStorageLastError());
		}
		gstate.affected += static_cast<idx_t>(rc);
		return SinkResultType::NEED_MORE_INPUT;
	}

	SourceResultType GetDataInternal(ExecutionContext &context, DataChunk &chunk,
	                                 OperatorSourceInput &input) const override {
		auto &gstate = sink_state->Cast<WasmWriteGlobalState>();
		chunk.SetCardinality(1);
		chunk.SetValue(0, 0, Value::BIGINT(static_cast<int64_t>(gstate.affected)));
		return SourceResultType::FINISHED;
	}

	bool IsSink() const override {
		return true;
	}
	bool IsSource() const override {
		return true;
	}

	string GetName() const override {
		return "WASM_DELETE";
	}

	InsertionOrderPreservingMap<string> ParamsToString() const override {
		InsertionOrderPreservingMap<string> result;
		result["Table Name"] = table.name;
		return result;
	}

private:
	TableCatalogEntry &table;
	idx_t rowid_index;
};

//===----------------------------------------------------------------------===//
// WasmPhysicalUpdate
//
// DuckDB feeds UPDATE via a plan whose child chunks contain:
//   * update_columns[0..N)  the new column values, in the order of
//                            LogicalUpdate::columns (physical/schema indices,
//                            captured here at plan time and forwarded as the
//                            `updated_columns` bridge arg).
//   * chunk.data[ChildTypes.size()-1]  the rowid column (last).
// We ship each row as `rowid` + the SET cells; the writer bridge maps each
// row's cells to the target columns using `updated_columns`.
//===----------------------------------------------------------------------===//

class WasmPhysicalUpdate : public PhysicalOperator {
public:
	WasmPhysicalUpdate(PhysicalPlan &physical_plan, LogicalOperator &op, TableCatalogEntry &table,
	                   vector<PhysicalIndex> columns_p)
	    : PhysicalOperator(physical_plan, PhysicalOperatorType::EXTENSION, op.types, 1), table(table),
	      columns(std::move(columns_p)) {
	}

	unique_ptr<GlobalSinkState> GetGlobalSinkState(ClientContext &context) const override {
		return make_uniq<WasmWriteGlobalState>(table.Cast<WasmTableEntry>());
	}

	SinkResultType Sink(ExecutionContext &context, DataChunk &chunk, OperatorSinkInput &input) const override {
		auto &gstate = input.global_state.Cast<WasmWriteGlobalState>();
		if (chunk.size() == 0) {
			return SinkResultType::NEED_MORE_INPUT;
		}
		chunk.Flatten();

		auto &wtxn = WasmTransaction::Get(context.client, gstate.table.catalog);
		uint32_t txn = wtxn.EnsureStarted();

		idx_t nrows = chunk.size();
		idx_t ncols = chunk.ColumnCount();
		if (ncols == 0) {
			return SinkResultType::NEED_MORE_INPUT;
		}
		// Rowid is the last column; update columns precede it. The number of
		// SET cells is `ncols - 1` and must match `columns.size()` (== the
		// LogicalUpdate::columns list captured at plan time). Marshal the
		// PhysicalIndex list into a flat schema-index vector for the bridge.
		idx_t update_cols = ncols - 1;
		if (update_cols != columns.size()) {
			throw InternalException(
			    "wasm storage update: chunk update-set width %llu != plan columns %llu",
			    static_cast<uint64_t>(update_cols), static_cast<uint64_t>(columns.size()));
		}
		auto &rowid_vec = chunk.data[ncols - 1];
		auto rowid_data = FlatVector::GetData<int64_t>(rowid_vec);
		vector<int64_t> rowids(rowid_data, rowid_data + nrows);

		vector<uint32_t> updated_columns;
		updated_columns.reserve(update_cols);
		for (idx_t c = 0; c < update_cols; c++) {
			updated_columns.push_back(static_cast<uint32_t>(columns[c].index));
		}

		vector<WasmWriteValue> flat(nrows * update_cols);
		vector<std::string> text_park(nrows * update_cols);
		vector<std::string> blob_park(nrows * update_cols);
		for (idx_t r = 0; r < nrows; r++) {
			for (idx_t c = 0; c < update_cols; c++) {
				idx_t slot = r * update_cols + c;
				flat[slot] = WasmWriteValue{};
				WasmMarshalWriteCell(chunk.data[c], r, flat[slot], text_park[slot], blob_park[slot]);
			}
		}

		int64_t rc = wasm_storage_write_update_rows(txn, gstate.table.name.c_str(), rowids.data(),
		                                             update_cols == 0 ? nullptr : updated_columns.data(),
		                                             update_cols == 0 ? nullptr : flat.data(),
		                                             static_cast<uint32_t>(nrows),
		                                             static_cast<uint32_t>(update_cols));
		if (rc < 0) {
			throw IOException("wasm storage update-rows failed for '%s': %s", gstate.table.name,
			                  WasmStorageLastError());
		}
		gstate.affected += static_cast<idx_t>(rc);
		return SinkResultType::NEED_MORE_INPUT;
	}

	SourceResultType GetDataInternal(ExecutionContext &context, DataChunk &chunk,
	                                 OperatorSourceInput &input) const override {
		auto &gstate = sink_state->Cast<WasmWriteGlobalState>();
		chunk.SetCardinality(1);
		chunk.SetValue(0, 0, Value::BIGINT(static_cast<int64_t>(gstate.affected)));
		return SourceResultType::FINISHED;
	}

	bool IsSink() const override {
		return true;
	}
	bool IsSource() const override {
		return true;
	}

	string GetName() const override {
		return "WASM_UPDATE";
	}

	InsertionOrderPreservingMap<string> ParamsToString() const override {
		InsertionOrderPreservingMap<string> result;
		result["Table Name"] = table.name;
		return result;
	}

private:
	TableCatalogEntry &table;
	vector<PhysicalIndex> columns;
};

//===----------------------------------------------------------------------===//
// WasmCatalog::Plan{Insert,Update,Delete}
//
// The wasm write path currently rejects RETURNING / ON CONFLICT (sqlite_scanner
// takes the same restriction — see sqlite_insert.cpp:186 & sqlite_update.cpp:110).
//===----------------------------------------------------------------------===//

PhysicalOperator &WasmCatalog::PlanInsert(ClientContext &context, PhysicalPlanGenerator &planner, LogicalInsert &op,
                                          optional_ptr<PhysicalOperator> plan) {
	if (op.return_chunk) {
		throw BinderException("wasm storage: RETURNING clause is not supported for INSERT yet");
	}
	if (op.on_conflict_info.action_type != OnConflictAction::THROW) {
		throw BinderException("wasm storage: ON CONFLICT is not supported for INSERT yet");
	}
	D_ASSERT(plan);
	auto &insert = planner.Make<WasmPhysicalInsert>(op, op.table);
	insert.children.push_back(*plan);
	return insert;
}

PhysicalOperator &WasmCatalog::PlanDelete(ClientContext &context, PhysicalPlanGenerator &planner, LogicalDelete &op,
                                          PhysicalOperator &plan) {
	if (op.return_chunk) {
		throw BinderException("wasm storage: RETURNING clause is not supported for DELETE yet");
	}
	auto &bound_ref = op.expressions[0]->Cast<BoundReferenceExpression>();
	auto &delete_op = planner.Make<WasmPhysicalDelete>(op, op.table, bound_ref.index);
	delete_op.children.push_back(plan);
	return delete_op;
}

PhysicalOperator &WasmCatalog::PlanUpdate(ClientContext &context, PhysicalPlanGenerator &planner, LogicalUpdate &op,
                                          PhysicalOperator &plan) {
	if (op.return_chunk) {
		throw BinderException("wasm storage: RETURNING clause is not supported for UPDATE yet");
	}
	for (auto &expr : op.expressions) {
		if (expr->type == ExpressionType::VALUE_DEFAULT) {
			throw BinderException("wasm storage: SET DEFAULT is not supported for UPDATE yet");
		}
	}
	auto &update = planner.Make<WasmPhysicalUpdate>(op, op.table, std::move(op.columns));
	update.children.push_back(plan);
	return update;
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
