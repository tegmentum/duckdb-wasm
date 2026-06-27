/*
 * Force-included shim for the WASI build of the DuckDB shell.
 *
 * The shell uses a few process/terminal POSIX APIs that wasi-libc omits. shell.cpp
 * declares popen()/pclose() itself; this only adds the declarations wasi-libc
 * leaves out so the C++ TUs compile. The implementations are no-op/failure stubs
 * in shell_wasi_stubs.c. Shell-only (passed via -include on the shell build), so
 * it never touches the core/library compilation.
 */
#ifndef _DUCKDB_WASI_SHELL_SHIM_H
#define _DUCKDB_WASI_SHELL_SHIM_H

#ifdef __wasi__

#include <stdio.h>

#ifdef __cplusplus
extern "C" {
#endif

/* wasi-libc's <stdlib.h> does not declare system() (no process spawning); the
 * shell calls it for `.shell`/`.system`. Declare it so the C++ call compiles;
 * the stub returns -1 (command not run). */
int system(const char *command);

/* wasi-libc's <stdio.h> omits popen()/pclose(). shell.cpp declares them itself,
 * but as plain (C++-mangled) externs. Declare them here with C linkage FIRST so
 * the names match the C stubs in shell_wasi_stubs.c; shell.cpp's later extern
 * declaration is then a compatible redeclaration that retains C linkage. */
FILE *popen(const char *command, const char *type);
int pclose(FILE *stream);

#ifdef __cplusplus
}
#endif

#endif /* __wasi__ */

#endif /* _DUCKDB_WASI_SHELL_SHIM_H */
