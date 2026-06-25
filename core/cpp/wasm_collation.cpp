//===----------------------------------------------------------------------===//
// wasm_collation.cpp
//
// Collation registration for the wasm core (plan Item 2).
//
// A collation-capable component (e.g. icufns) registers a sort-key SCALAR
// (text -> sort-key text) normally via register-scalar, then declares a
// COLLATION whose transform IS that already-registered scalar. `ORDER BY x
// COLLATE <name>` resolves the collation at bind time (CollationBinding pulls
// the CollateCatalogEntry from the system catalog and binds its stored
// ScalarFunction around the column), so we register the collation by:
//   1. looking the scalar `transform_scalar` up in the system catalog,
//   2. copying its ScalarFunction object out, and
//   3. wrapping it in a DuckDB CreateCollationInfo + CreateCollation.
//
// This REUSES the existing scalar dispatch entirely -- no new callback path.
// Registration is mid-session safe: CreateCollation goes through the system
// transaction with IGNORE_ON_CONFLICT and the binder reads collations from the
// system catalog at bind time, exactly as the built-in ICU extension does.
//
// Compiled in-core (DUCKDB_BUILD_LIBRARY) with the exact wasi-sdk flags
// extracted from sqlite_scanner's build (see core/build.rs / build_wasm_cpp).
//===----------------------------------------------------------------------===//

#include "duckdb.hpp"
#include "duckdb.h"

#include "duckdb/main/capi/capi_internal.hpp"
#include "duckdb/main/database.hpp"
#include "duckdb/catalog/catalog.hpp"
#include "duckdb/catalog/catalog_transaction.hpp"
#include "duckdb/catalog/catalog_entry/schema_catalog_entry.hpp"
#include "duckdb/catalog/catalog_entry/scalar_function_catalog_entry.hpp"
#include "duckdb/catalog/entry_lookup_info.hpp"
#include "duckdb/parser/parsed_data/create_collation_info.hpp"
#include "duckdb/common/enums/catalog_type.hpp"
#include "duckdb/common/exception.hpp"
#include "duckdb/common/string_util.hpp"

#include <cstdio>
#include <string>

namespace duckdb {

//! Registers a collation `name` whose transform is the already-registered scalar
//! function `transform_scalar`. Looks the scalar up in the system catalog's main
//! schema (via the system transaction -- no ClientContext needed), copies its
//! ScalarFunction object, and wraps it in a CreateCollationInfo.
static void RegisterWasmCollation(DatabaseInstance &instance, const std::string &name,
                                  const std::string &transform_scalar, bool combinable) {
	auto &system_catalog = Catalog::GetSystemCatalog(instance);
	auto transaction = CatalogTransaction::GetSystemTransaction(instance);

	// Resolve the sort-key scalar the component already registered.
	auto &schema = system_catalog.GetSchema(transaction, DEFAULT_SCHEMA);
	auto entry = schema.GetEntry(transaction, CatalogType::SCALAR_FUNCTION_ENTRY, transform_scalar);
	if (!entry) {
		throw CatalogException(
		    "register_collation: transform scalar \"%s\" not found in the catalog (register it first)",
		    transform_scalar);
	}
	auto &func_entry = entry->Cast<ScalarFunctionCatalogEntry>();
	if (func_entry.functions.Size() == 0) {
		throw CatalogException("register_collation: scalar \"%s\" has no overloads", transform_scalar);
	}
	// The collation transform is a single (text -> sort-key text) function. Take
	// the first overload; the component registers exactly one single-arg variant.
	ScalarFunction transform = func_entry.functions.GetFunctionByOffset(0);

	// `not_required_for_equality = false`: a locale-aware collation can change
	// equality too (e.g. case- or accent-insensitive), so apply it for equality
	// as well. This matches the conservative default for ICU-style collations.
	CreateCollationInfo info(name, std::move(transform), combinable, /*not_required_for_equality=*/false);
	info.on_conflict = OnCreateConflict::IGNORE_ON_CONFLICT;
	system_catalog.CreateCollation(transaction, info);
}

} // namespace duckdb

//! Registers a wasm collation on the given database. `name` is the COLLATE name,
//! `transform_scalar` names an already-registered scalar (text -> sort-key
//! text) used as the collation's transform. Idempotent (IGNORE_ON_CONFLICT).
extern "C" void wasm_register_collation(duckdb_database db, const char *name, const char *transform_scalar,
                                        bool combinable) {
	if (!db || !name || !transform_scalar) {
		return;
	}
	try {
		auto wrapper = reinterpret_cast<duckdb::DatabaseWrapper *>(db);
		if (!wrapper || !wrapper->database) {
			return;
		}
		auto &instance = *wrapper->database->instance;
		duckdb::RegisterWasmCollation(instance, std::string(name), std::string(transform_scalar), combinable);
	} catch (const std::exception &e) {
		fprintf(stderr, "wasm_register_collation('%s') failed: %s\n", name, e.what());
	} catch (...) {
		fprintf(stderr, "wasm_register_collation('%s') failed: unknown error\n", name);
	}
}
