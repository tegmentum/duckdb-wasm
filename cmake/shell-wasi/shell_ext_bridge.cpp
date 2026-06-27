/*
 * Route A db-handle bridge implementation. Compiled into the extension-aware
 * shell command (scripts/build-shell-ext-wasm.sh). The shell uses the DuckDB
 * C++ API (duckdb::DuckDB / duckdb::Connection), but the Rust install glue
 * registers scalar functions through the DuckDB C-API
 * (duckdb_register_scalar_function), whose `duckdb_connection` is just a
 * reinterpret_cast of a `duckdb::Connection*` (see src/main/capi/duckdb-c.cpp).
 *
 * So: create a dedicated sibling connection on the shell's database and hand its
 * pointer to the glue. The glue registers on this connection (never the
 * LOAD-busy primary connection); registered functions live in the database
 * catalog and are therefore visible to the shell's primary connection too.
 *
 * The sibling connection is intentionally leaked: it must outlive every LOAD for
 * the lifetime of the database. (A `.open` of a new file in the same process is
 * out of scope for this build.)
 */
#include "duckdb.hpp"

extern "C" void duckdb_shell_ext_set_connection(void *conn);

extern "C" void duckdb_shell_ext_register_db(void *db_ptr) {
	if (!db_ptr) {
		return;
	}
	auto *db = reinterpret_cast<duckdb::DuckDB *>(db_ptr);
	auto *conn = new duckdb::Connection(*db); // leaked on purpose; see header note
	duckdb_shell_ext_set_connection(reinterpret_cast<void *>(conn));
}
