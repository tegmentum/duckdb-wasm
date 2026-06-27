/*
 * Minimal <pwd.h> for the WASI build of the DuckDB shell.
 *
 * wasi-libc has no passwd database. shell.cpp #includes <pwd.h> unconditionally
 * on non-Windows platforms but never actually calls getpwuid()/getpwnam() on the
 * wasi code path, so this header only has to make the #include resolve and
 * provide the (unused) declarations. Placed on the shell-only include path
 * (cmake/shell-wasi/include) so it never affects the core/library build.
 */
#ifndef _DUCKDB_WASI_PWD_H
#define _DUCKDB_WASI_PWD_H

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

struct passwd {
	char *pw_name;
	char *pw_passwd;
	uid_t pw_uid;
	gid_t pw_gid;
	char *pw_gecos;
	char *pw_dir;
	char *pw_shell;
};

struct passwd *getpwuid(uid_t uid);
struct passwd *getpwnam(const char *name);

#ifdef __cplusplus
}
#endif

#endif /* _DUCKDB_WASI_PWD_H */
