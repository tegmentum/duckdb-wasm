/*
 * Route A db-handle bridge declaration. Force-included into the shell sources
 * (only when DUCKDB_SHELL_EXT is defined by scripts/build-shell-ext-wasm.sh) so
 * ShellState::OpenDB can hand the freshly opened database to the Rust install
 * glue, which registers component-extension scalar functions onto a sibling
 * connection of that database.
 */
#ifndef DUCKDB_SHELL_EXT_BRIDGE_H
#define DUCKDB_SHELL_EXT_BRIDGE_H

#ifdef __cplusplus
extern "C" {
#endif

/* db_ptr is a duckdb::DuckDB* (passed as void* to avoid leaking the C++ type
 * into the force-include). */
void duckdb_shell_ext_register_db(void *db_ptr);

#ifdef __cplusplus
}
#endif

#endif /* DUCKDB_SHELL_EXT_BRIDGE_H */
