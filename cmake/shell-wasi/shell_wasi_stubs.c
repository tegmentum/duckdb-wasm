/*
 * Link-time stubs for the standalone WASI build of the DuckDB shell.
 *
 * Two groups of symbols are resolved here:
 *
 * 1. POSIX process APIs wasi-libc omits (no subprocesses in the sandbox). The
 *    shell references popen/pclose/system for the pager, `.shell`/`.system`, and
 *    output redirection to a pipe. On wasi those features are unavailable, so the
 *    stubs fail gracefully (popen -> NULL, system -> -1) and the rest of the REPL
 *    keeps working.
 *
 * 2. Host-bridge hooks the prebuilt libduckdb-wasi.a expects from the *component*
 *    runtime (ducklink-core's Rust crate): the TVM larger-than-memory spill bridge
 *    and the WebAssembly-component extension loader. The standalone shell has no
 *    such host, so these report "unavailable": spill falls back to temp files and
 *    LOAD behaves exactly as a normal DuckDB build with no component catalog.
 *
 * Only built into the shell executable; never merged into libduckdb-wasi.a.
 */
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <errno.h>

/* --- 1. process APIs ----------------------------------------------------- */

FILE *popen(const char *command, const char *type) {
	(void)command;
	(void)type;
	errno = ENOSYS; /* no subprocesses on wasi */
	return NULL;
}

int pclose(FILE *stream) {
	(void)stream;
	return -1;
}

int system(const char *command) {
	/* POSIX: system(NULL) returns nonzero iff a command processor is available.
	 * There is none on wasi. */
	(void)command;
	return -1;
}

/* --- 2. host-bridge hooks (component runtime only) ------------------------ */

/* TVM larger-than-memory spill bridge (crates/ducklink-core/src/tvm_spill.rs).
 * Returning 0/"not available" makes DuckDB's buffer manager fall back to its
 * normal temp-file spill path. */
int tvm_spill_write(uint8_t tag, int64_t block_id, const uint8_t *data, uint64_t alloc_size,
                    uint64_t logical_size, uint64_t header_size) {
	(void)tag;
	(void)block_id;
	(void)data;
	(void)alloc_size;
	(void)logical_size;
	(void)header_size;
	return 0;
}

int tvm_spill_query(int64_t block_id, uint64_t *out_logical, uint64_t *out_header) {
	(void)block_id;
	(void)out_logical;
	(void)out_header;
	return 0;
}

int tvm_spill_read(int64_t block_id, uint8_t *out, uint64_t capacity) {
	(void)block_id;
	(void)out;
	(void)capacity;
	return 0;
}

uint64_t tvm_spill_delete(int64_t block_id) {
	(void)block_id;
	return 0;
}

int tvm_spill_available(void) {
	return 0;
}

/* WebAssembly-component extension loader (crates/ducklink-core/src/extension_loader.rs).
 * No component host in the standalone shell, so LOAD falls through to DuckDB's
 * normal "extension load disabled" handling. */
int duckdb_component_load_extension(const char *name) {
	(void)name;
	return 0; /* false: not handled by a component host */
}
