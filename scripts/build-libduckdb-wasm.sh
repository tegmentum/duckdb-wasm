#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${DUCKDB_SOURCE_DIR:-}" ]]; then
  echo "Set DUCKDB_SOURCE_DIR to a DuckDB checkout" >&2
  exit 1
fi

if [[ -z "${WASI_SDK_PREFIX:-}" ]]; then
  echo "Set WASI_SDK_PREFIX to the wasi-sdk installation path" >&2
  exit 1
fi

# DuckDB MUST be compiled for the same wasm target the component links against
# (wasm32-wasip2). The toolchain otherwise defaults to wasm32-wasip1-threads,
# which is a -pthread build where errno/TLS are thread-local; that thread-local
# access faults (out-of-bounds) in the single-threaded component runtime -- e.g.
# the SQL parser traps in process_integer_literal/core_yylex on the first parse.
# Keep this aligned with the component target.
export WASI_TARGET_TRIPLE=${WASI_TARGET_TRIPLE:-"wasm32-wasip2"}

WASM_EXTENSIONS=${WASM_EXTENSIONS:-"json"}

# Which in-tree DuckDB extensions get statically linked + registered as builtins
# is driven by this CMake config (duckdb_extension_load calls), NOT by
# WASM_EXTENSIONS (that env only flips DuckDB's WASM_ENABLED flag). Override with
# DUCKDB_EXTENSION_CONFIGS to point at a different file.
DUCKDB_EXTENSION_CONFIGS=${DUCKDB_EXTENSION_CONFIGS:-"$(pwd)/cmake/wasm-extension-config.cmake"}

# EMBED_EXTENSIONS: comma-separated list of extensions to compile + embed into
# libduckdb-wasi.a. Default empty => FULLY LEAN (only DuckDB's base
# core_functions + parquet). Both the cmake config (which duckdb_extension_load
# calls fire) and the source-staging/patching/dep-merging steps below gate on
# this same list, so an unselected extension adds nothing. Example:
#   EMBED_EXTENSIONS="httpfs,json,icu,spatial" ./scripts/build-libduckdb-wasm.sh
export EMBED_EXTENSIONS=${EMBED_EXTENSIONS:-""}
echo "  embed extensions: ${EMBED_EXTENSIONS:-<none — fully lean>}" >&2

# Is extension <name> selected for embedding? (membership in EMBED_EXTENSIONS)
ext_selected() {
  local want="$1" list=",${EMBED_EXTENSIONS//[[:space:]]/},"
  [[ "$list" == *",$want,"* ]]
}

BUILD_DIR=${BUILD_DIR:-"$(pwd)/build/duckdb-wasi"}
mkdir -p "$BUILD_DIR"

echo "Configuring DuckDB for wasm32-wasi in $BUILD_DIR" >&2
echo "  extension config: $DUCKDB_EXTENSION_CONFIGS" >&2
configure_duckdb() {
  # EXTRA_CXX_FLAGS / EXTRA_C_FLAGS append to the DuckDB compile (e.g.
  # EXTRA_CXX_FLAGS=-msimd128 to enable wasm SIMD autovectorization). Empty by
  # default; the runtime (wasmtime, browsers) supports SIMD.
  # Note: -msimd128 was measured as noise-level on sort/aggregation workloads
  # (the bottleneck was component COMPILE time, fixed by the host's on-disk
  # cache, not query compute), so it is intentionally not enabled by default.
  env WASM_EXTENSIONS="$WASM_EXTENSIONS" cmake -S "$DUCKDB_SOURCE_DIR" -B "$BUILD_DIR" \
    -DCMAKE_TOOLCHAIN_FILE="$(pwd)/cmake/toolchains/wasi-sdk.cmake" \
    -DWASI_SDK_PREFIX:PATH="$WASI_SDK_PREFIX" \
    -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
    -DDUCKDB_EXTENSION_CONFIGS="$DUCKDB_EXTENSION_CONFIGS" \
    -DEMBED_EXTENSIONS="$EMBED_EXTENSIONS" \
    -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
    -DCMAKE_CXX_FLAGS="${EXTRA_CXX_FLAGS:-}" \
    -DCMAKE_C_FLAGS="${EXTRA_C_FLAGS:-${EXTRA_CXX_FLAGS:-}}" \
    -DBUILD_SHELL=OFF \
    -DBUILD_TESTS=OFF \
    -DBUILD_BENCHMARK=OFF \
    -DDUCKDB_PLATFORM="wasm32-wasi" \
    -DDUCKDB_LIBDYNAMIC=OFF \
    -DDUCKDB_LIBDUCKDB_STATIC=ON
}
# Patches for FetchContent-populated extension sources. Some are
# configure-blocking (avro's find_library(LZMA), iceberg's find_package(AWSSDK))
# and must be applied before configure can process that extension; since sources
# are fetched progressively, the loop below re-runs configure + patch until it
# succeeds. Every patch is idempotent and guards on its source being present.
apply_extension_patches() {
# Embed a CA bundle into httpfs's curl client so HTTPS certificate verification
# works without a host CA file: openssl's file BIO isn't reliably reachable
# through the component's wrapped filesystem, so we load the bundle from memory
# via CURLOPT_CAINFO_BLOB. Runs after configure (which FetchContent-populates the
# httpfs source) and before the build; idempotent (skips if already patched).
HTTPFS_SRC="$BUILD_DIR/_deps/httpfs_extension_fc-src/extension/httpfs"
CA_BUNDLE="$(pwd)/cmake/ca-bundle/cacert.pem"
if ext_selected httpfs \
   && [[ -d "$HTTPFS_SRC" && -f "$CA_BUNDLE" ]]; then
  { printf '{'; xxd -i < "$CA_BUNDLE"; printf '}'; } > "$HTTPFS_SRC/duckdb_ca_bundle.inc"
  python3 - "$HTTPFS_SRC/httpfs_curl_client.cpp" <<'PY'
import re, sys
p = sys.argv[1]
s = open(p).read()
if 'duckdb_wasi_ca_bundle' in s:
    sys.exit(0)
m = re.search(r'\n[ \t]*if \(!cert_path\.empty\(\)\) \{\n[ \t]*curl_easy_setopt\(curl, CURLOPT_CAINFO, cert_path\.c_str\(\)\);\n[ \t]*\}\n\}', s)
if not m:
    sys.stderr.write('CAINFO anchor not found in httpfs_curl_client.cpp\n'); sys.exit(1)
block = m.group(0)
inject = '''
#ifdef __wasi__
\t// wasi: openssl's file BIO can't reach the host filesystem reliably, so load
\t// an embedded CA bundle from memory (CURLOPT_CAINFO_BLOB, no file I/O).
\t{
\t\tstatic const unsigned char duckdb_wasi_ca_bundle[] =
#include "duckdb_ca_bundle.inc"
\t\t;
\t\tstruct curl_blob ca_blob;
\t\tca_blob.data = (void *)duckdb_wasi_ca_bundle;
\t\tca_blob.len = sizeof(duckdb_wasi_ca_bundle);
\t\tca_blob.flags = CURL_BLOB_COPY;
\t\tcurl_easy_setopt(curl, CURLOPT_CAINFO_BLOB, &ca_blob);
\t}
#endif
}'''
s = s.replace(block, block[:-1] + inject, 1)
open(p, 'w').write(s)
print('patched httpfs_curl_client.cpp for embedded CA bundle')
PY
  echo "Embedded CA bundle into httpfs curl client ($(grep -c 'BEGIN CERTIFICATE' "$CA_BUNDLE") certs)" >&2

  # DuckDB's bundled httplib (compiled only when httpfs is embedded) has an
  # AF_UNIX path using sockaddr_un::sun_path, which wasi's <sys/un.h> omits. wasi
  # uses curl + TCP, never unix sockets, so exclude the AF_UNIX block on wasi.
  HTTPLIB_HPP="$DUCKDB_SOURCE_DIR/third_party/httplib/httplib.hpp"
  if [[ -f "$HTTPLIB_HPP" ]]; then
    python3 - "$HTTPLIB_HPP" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
old = '#if !defined(_WIN32) || defined(CPPHTTPLIB_HAVE_AFUNIX_H)'
new = '#if (!defined(_WIN32) || defined(CPPHTTPLIB_HAVE_AFUNIX_H)) && !defined(__wasi__)'
if new in s:
    sys.exit(0)
if old not in s:
    sys.exit('httplib.hpp: AF_UNIX guard anchor not found')
s = s.replace(old, new)
open(p, 'w').write(s)
print('patched httplib.hpp: AF_UNIX excluded on wasi (no sun_path)')
PY
  fi

  # Make curl the default HTTP client on wasi: the vendored httplib client
  # compiles but its non-blocking connect (select/poll) doesn't work on wasi, so
  # `read_csv('https://...')` must use curl. Remap the default -> curl and seed
  # config.http_util with HTTPFSCurlUtil. Idempotent (marker: wasi-default-curl).
  python3 - "$HTTPFS_SRC/httpfs_extension.cpp" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
if 'wasi-default-curl' in s:
    sys.exit(0)
# 1) initial default assignment in LoadInternal
init_old = ('\t} else {\n'
            '\t\tconfig.http_util = make_shared_ptr<HTTPFSUtil>();\n'
            '\t}')
init_new = ('\t} else {\n'
            '#ifdef __wasi__\n'
            '\t\tconfig.http_util = make_shared_ptr<HTTPFSCurlUtil>(); // wasi-default-curl\n'
            '#else\n'
            '\t\tconfig.http_util = make_shared_ptr<HTTPFSUtil>();\n'
            '#endif\n'
            '\t}')
# 2) inside the SET callback, remap "default" -> "curl" on wasi
cb_old = '#ifndef EMSCRIPTEN\n\t\tif (value == "curl") {'
cb_new = ('#ifndef EMSCRIPTEN\n'
          '#ifdef __wasi__\n'
          '\t\tif (value == "default") {\n'
          '\t\t\tvalue = "curl";\n'
          '\t\t}\n'
          '#endif\n'
          '\t\tif (value == "curl") {')
for old, new, what in ((init_old, init_new, 'LoadInternal default'),
                       (cb_old, cb_new, 'SET callback')):
    if old not in s:
        sys.stderr.write('anchor not found: %s\n' % what); sys.exit(1)
    s = s.replace(old, new, 1)
open(p, 'w').write(s)
print('patched httpfs_extension.cpp: curl is the default client on wasi')
PY
fi

# unity_catalog (DuckDB 1.5.4 renamed uc_catalog -> unity_catalog @ d52a7ee): no
# source patch needed for wasm. Unlike the old v1.4.0 (raw libcurl + CURLOPT_CAINFO
# file), d52a7ee issues its Unity Catalog REST calls through DuckDB's HTTPUtil
# (curl from httpfs, which carries the embedded CA bundle), already uses
# build_static_extension + the new ExtensionLoader API, and AutoLoadExtension's
# httpfs. It depends on delta (uc_delta_ccv2_commit) at runtime. So just embed it
# (with httpfs + delta) -- the config enables it; nothing to patch here.

# quack: CLIENT-ONLY on wasm. The httplib SERVER (quack_http_server.cpp, a
# 128-thread pool + listen/accept) compiles + links unchanged on wasm32-wasip2
# (std::thread is in libc++; pthread_create is a weak libc stub), so the server
# TUs are harmless dead code -- no exclusion needed. But quack_serve would, at
# runtime, try to spin a listener httplib can't bind() in the wasip2 sandbox, so
# extend quack's bind-time guard (it already throws for __EMSCRIPTEN__) to cover
# __wasi__, giving a clean error instead of a thread/socket failure. The CLIENT
# (quack_client.cpp, over the curl HTTPUtil) is unaffected. Idempotent; a missing
# anchor (beta churn) is a non-fatal skip -- the server is dead code regardless.
QUACK_SRC="$BUILD_DIR/_deps/quack_extension_fc-src"
if ext_selected quack \
   && [[ -f "$QUACK_SRC/src/quack_start_stop.cpp" ]]; then
  python3 - "$QUACK_SRC/src/quack_start_stop.cpp" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
if 'quack-wasi-no-serve' in s:
    sys.exit(0)
old = '#ifdef __EMSCRIPTEN__\n\tthrow NotImplementedException("quack_serve is currently not implemented for the wasm platform'
new = ('#if defined(__EMSCRIPTEN__) || defined(__wasi__) // quack-wasi-no-serve\n'
       '\tthrow NotImplementedException("quack_serve is currently not implemented for the wasm platform')
if old not in s:
    sys.stderr.write('quack_start_stop.cpp: __EMSCRIPTEN__ quack_serve guard anchor '
                     'not found -- skipping (server is dead code on wasi anyway)\n')
    sys.exit(0)
s = s.replace(old, new, 1)
open(p, 'w').write(s)
print('patched quack_start_stop.cpp: quack_serve guarded off on wasi (client-only)')
PY
fi

# duckdb-avro: our wasi avro-c is deflate-only (no lzma/snappy), so drop those
# REQUIRED find_library() calls + their use in ALL_AVRO_LIBRARIES. Idempotent.
AVRO_SRC="$BUILD_DIR/_deps/avro_extension_fc-src"
if ext_selected avro \
   && [[ -f "$AVRO_SRC/CMakeLists.txt" ]]; then
  python3 - "$AVRO_SRC/CMakeLists.txt" <<'PY'
import re, sys
p = sys.argv[1]; s = open(p).read()
if 'wasi-no-lzma-snappy' in s:
    sys.exit(0)
# drop the lzma/snappy REQUIRED finds (both MSVC and non-MSVC spellings)
for pat in [r'\n\s*find_library\(LZMA_LIBRARY [^\)]*REQUIRED\)',
            r'\n\s*find_library\(SNAPPY_LIBRARY [^\)]*REQUIRED\)']:
    s = re.sub(pat, '', s)
# drop their use in ALL_AVRO_LIBRARIES (+ jemalloc/gmp/math which we don't provide)
for var in ('LZMA_LIBRARY', 'SNAPPY_LIBRARY', 'JEMALLOC_LIBRARY', 'GMP_LIBRARY', 'MATH_LIBRARY'):
    s = re.sub(r'\n\s*\$\{%s\}' % var, '', s)
s = '# wasi-no-lzma-snappy\n' + s
open(p, 'w').write(s)
print('patched duckdb-avro CMakeLists: deflate-only (no lzma/snappy/jemalloc/gmp)')
PY
fi

# iceberg: upstream finds the AWS C++ SDK + CURL behind `NOT Emscripten` guards.
# The AWS SDK doesn't build for wasm, but iceberg's DEFAULT request path
# (AWSInput::ExecuteRequest, with the iceberg_via_aws_sdk_for_catalog_interactions
# setting off -- the default) already signs SigV4 by hand (mbedtls) and issues the
# request through DuckDB's own HTTPUtil (curl) -- no AWS SDK. So on WASI we
# (1) skip the AWS/CURL find_package in CMake (curl comes from httpfs),
# (2) put a minimal AWS-type stub tree (cmake/iceberg-wasi/aws-stubs) on the
#     include path so the pervasively-AWS-typed headers (e6fe0a4b moved them to
#     src/catalog/rest/storage/aws.{cpp,hpp}) compile, and
# (3) compile out the opt-in AWS-SDK legacy path (CreateSignedRequest /
#     ExecuteRequestLegacy -- the only spots with heavy AWS-SDK calls) under
#     __wasi__; their existing EMSCRIPTEN #else branches are the wasm fallback.
# EMSCRIPTEN keeps upstream's vcpkg AWS path. So AWS-native Iceberg REST catalogs
# (Glue/S3 Tables) sign + fetch via curl, and local iceberg_scan needs none of it.
ICE_SRC="$BUILD_DIR/_deps/iceberg_extension_fc-src"
AWS_STUBS="$(pwd)/cmake/iceberg-wasi/aws-stubs"
if ext_selected iceberg && [[ -d "$ICE_SRC" ]]; then
  ICE_CML="$ICE_SRC/CMakeLists.txt"
  # CMake: skip AWS/CURL find_package on WASI as well as Emscripten.
  if ! grep -q 'STREQUAL "WASI"' "$ICE_CML"; then
    perl -0pi -e 's/NOT CMAKE_SYSTEM_NAME STREQUAL "Emscripten"/NOT CMAKE_SYSTEM_NAME STREQUAL "Emscripten" AND NOT CMAKE_SYSTEM_NAME STREQUAL "WASI"/g' "$ICE_CML"
  fi
  # CMake: put the AWS-type stub tree on the include path (WASI only). The
  # pervasively-AWS-typed headers then resolve <aws/core/...> to the stubs.
  if ! grep -q 'DUCKLINK_AWS_STUBS' "$ICE_CML"; then
    python3 - "$ICE_CML" "$AWS_STUBS" <<'PY'
import sys
p, stubs = sys.argv[1], sys.argv[2]
s = open(p).read()
anchor = 'include_directories(src/include)'
add = (anchor + '\n# DUCKLINK_AWS_STUBS: minimal AWS SDK type stubs (no AWS SDK on wasm)\n'
       'if(CMAKE_SYSTEM_NAME STREQUAL "WASI")\n  include_directories("%s")\nendif()' % stubs)
s = s.replace(anchor, add, 1)
open(p, 'w').write(s)
print("patched iceberg CMakeLists: AWS stubs on include path (WASI)", file=sys.stderr)
PY
  fi
  # aws.cpp: compile out the AWS-SDK legacy path on __wasi__. The two #ifndef
  # EMSCRIPTEN blocks (CreateSignedRequest + ExecuteRequestLegacy) are the only
  # spots with heavy AWS-SDK calls (CreateHttpRequest/AWSAuthV4Signer/HttpClient);
  # extend the guard so wasi takes their existing #else fallback. Everything else
  # (the default ExecuteRequest path + the type-only helpers) compiles against the
  # stubs.
  AWS_CPP="$ICE_SRC/src/catalog/rest/storage/aws.cpp"
  if [[ -f "$AWS_CPP" ]] && ! grep -q 'DUCKLINK_WASI_NO_AWS_SDK' "$AWS_CPP"; then
    perl -0pi -e 's/#ifndef EMSCRIPTEN/#if !defined(EMSCRIPTEN) && !defined(__wasi__) \/\/ DUCKLINK_WASI_NO_AWS_SDK/g' "$AWS_CPP"
    echo "patched iceberg aws.cpp: AWS-SDK legacy path compiled out on wasi" >&2
  fi
  echo "patched iceberg: AWS SDK skipped on wasi (stubs + inline SigV4 via HTTPUtil)" >&2
fi

# spatial: replace its find_package(GDAL/PROJ/EXPAT/sqlite/ZLIB/GEOS) with our
# IMPORTED targets (cmake/spatial-deps.cmake, backed by the ~/git/*-wasm libs)
# and turn network off on wasi (like the upstream Emscripten path).
SPATIAL_SRC="$BUILD_DIR/_deps/spatial_extension_fc-src"
SPATIAL_DEPS_CMAKE="$(pwd)/cmake/spatial-deps.cmake"
if ext_selected spatial \
   && [[ -f "$SPATIAL_SRC/CMakeLists.txt" ]]; then
  python3 - "$SPATIAL_SRC/CMakeLists.txt" "$SPATIAL_DEPS_CMAKE" <<'PY'
import sys
p, inc = sys.argv[1], sys.argv[2]
s = open(p).read()
if 'spatial-deps.cmake' in s:
    sys.exit(0)
# 1) replace the find_package block with include(our imported targets)
fp_old = ('find_package(ZLIB REQUIRED)\n'
          'find_package(PROJ CONFIG REQUIRED)\n'
          'find_package(GDAL CONFIG REQUIRED)\n'
          'find_package(EXPAT REQUIRED)\n'
          'find_package(unofficial-sqlite3 CONFIG REQUIRED)')
fp_new = 'include("%s")' % inc
# 2) GEOS is found separately; our include() already defines GEOS::geos_c
geos_old = '  find_package(GEOS REQUIRED)\n'
# 3) network off on wasi too (matches the Emscripten branch)
net_old = ('if(EMSCRIPTEN)\n'
           '  message(STATUS "Building for Emscripten, disabling network functionality")\n'
           '  set(SPATIAL_USE_NETWORK OFF)\n'
           'endif()')
net_new = ('if(EMSCRIPTEN OR CMAKE_SYSTEM_NAME STREQUAL "WASI")\n'
           '  message(STATUS "Disabling network functionality")\n'
           '  set(SPATIAL_USE_NETWORK OFF)\n'
           'endif()')
for old, new, what in ((fp_old, fp_new, 'find_package block'),
                       (geos_old, '', 'GEOS find_package'),
                       (net_old, net_new, 'network guard')):
    if old not in s:
        sys.stderr.write('spatial anchor not found: %s\n' % what); sys.exit(1)
    s = s.replace(old, new, 1)
open(p, 'w').write(s)
print('patched spatial CMakeLists: IMPORTED geo deps + network off on wasi')
PY
  # proj_db.c: the extension embeds an OLDER proj.db (DATABASE.LAYOUT.VERSION
  # MINOR=2) than proj-wasm's libproj (PROJ 9.x rejects layout < 1.6).
  # Regenerate it (xxd -i -> proj_db[]/proj_db_len) from the matching proj.db.
  PROJ_DB="$HOME/git/proj-wasm/build_real_sqlite/deps/proj/data/proj.db"
  PROJ_DB_C="$SPATIAL_SRC/src/spatial/modules/proj/proj_db.c"
  if [[ -f "$PROJ_DB" && -f "$PROJ_DB_C" ]]; then
    _sz="$(wc -c < "$PROJ_DB" | tr -d ' ')"
    if ! grep -q "proj_db_len = $_sz" "$PROJ_DB_C" 2>/dev/null; then
      _t="$(mktemp -d)"; cp "$PROJ_DB" "$_t/proj.db"
      ( cd "$_t" && xxd -i proj.db ) > "$PROJ_DB_C"
      rm -rf "$_t"
      echo "regenerated spatial proj_db.c from proj-wasm proj.db (layout 1.6)" >&2
    fi
  fi
fi

# excel: replace its find_package(EXPAT/ZLIB/minizip-ng) with our IMPORTED
# targets (cmake/excel-deps.cmake, backed by expat-wasm + curl-wasm zlib +
# the minizip-ng built by build-wasi-deps.sh).
EXCEL_SRC="$BUILD_DIR/_deps/excel_extension_fc-src"
EXCEL_DEPS_CMAKE="$(pwd)/cmake/excel-deps.cmake"
if ext_selected excel \
   && [[ -f "$EXCEL_SRC/CMakeLists.txt" ]]; then
  python3 - "$EXCEL_SRC/CMakeLists.txt" "$EXCEL_DEPS_CMAKE" <<'PY'
import sys
p, inc = sys.argv[1], sys.argv[2]
s = open(p).read()
if 'excel-deps.cmake' in s:
    sys.exit(0)
fp_old = ('find_package(EXPAT REQUIRED)\n'
          'find_package(ZLIB REQUIRED)\n'
          'find_package(minizip-ng CONFIG REQUIRED)')
if fp_old not in s:
    sys.stderr.write('excel anchor not found: find_package block\n'); sys.exit(1)
s = s.replace(fp_old, 'include("%s")' % inc, 1)
open(p, 'w').write(s)
print('patched excel CMakeLists: IMPORTED EXPAT + ZLIB + minizip-ng deps')
PY
fi

# postgres_scanner: the pinned extension (f012a4f) compiles libpq's sources
# inline from a downloaded PostgreSQL tree and is DONT_LINK (loadable only).
# For the static wasm core we: replace find_package(OpenSSL) with our deps
# (cmake/postgres-deps.cmake -> openssl-wasm + shim force-include); add a
# build_static_extension call; drop the getaddrinfo/gettimeofday fallback files
# (wasi has those); and stage the wasi-cross-configured PG 15.13 source (built
# by build-wasi-deps.sh) as the extension's `postgres/` tree so it skips its own
# download + host ./configure. Networking comes from httpfs's wasip2 graft.
PG_SRC="$BUILD_DIR/_deps/postgres_scanner_extension_fc-src"
PG_DEPS_CMAKE="$(pwd)/cmake/postgres-deps.cmake"
PG_STAGED="$(pwd)/build/wasi-deps/src/postgresql-15.13"
if ext_selected postgres_scanner \
   && [[ -f "$PG_SRC/CMakeLists.txt" ]]; then
  python3 - "$PG_SRC/CMakeLists.txt" "$PG_DEPS_CMAKE" <<'PY'
import sys
p, inc = sys.argv[1], sys.argv[2]
s = open(p).read()
if 'postgres-deps.cmake' in s:
    sys.exit(0)
# postgres_scanner 1.5.4 uses find_package(OpenSSL)+find_package(PostgreSQL) and
# already calls build_static_extension. cmake/postgres-deps.cmake provides both
# (openssl-wasm + the prebuilt libpq.a) + the wasi shim force-include, so just
# redirect the first find_package to the include and drop the second.
if 'find_package(OpenSSL REQUIRED)' not in s:
    sys.stderr.write('postgres anchor not found: find_package(OpenSSL)\n'); sys.exit(1)
s = s.replace('find_package(OpenSSL REQUIRED)', 'include("%s")' % inc, 1)
s = s.replace('find_package(PostgreSQL REQUIRED)', '', 1)
open(p, 'w').write(s)
print('patched postgres CMakeLists: openssl + prebuilt libpq via postgres-deps.cmake')
PY
  # postgres_scanner 1.5.4 splits its connection helpers into a database-connector
  # git submodule (dbconnector/*.hpp); FetchContent doesn't init it.
  if [[ -d "$PG_SRC/.git" || -f "$PG_SRC/.git" ]]; then
    git -C "$PG_SRC" submodule update --init database-connector 2>/dev/null \
      && echo "initialized postgres_scanner database-connector submodule" >&2
  fi
  # postgres_scanner 1.5.4's postgres_oauth.cpp uses the PG18 libpq OAuth API,
  # absent from the staged PG-15.13 libpq. Back-port the decls into libpq-fe.h so
  # it compiles; the hook never fires on wasi (no OAuth backend) and
  # PQsetAuthDataHook/PQgetAuthDataHook are no-op stubs in postgres-wasi/stubs.c.
  PG_FE_H="$PG_STAGED/src/interfaces/libpq/libpq-fe.h"
  if [[ -f "$PG_FE_H" ]]; then
    python3 - "$PG_FE_H" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
if 'PQAUTHDATA_OAUTH_BEARER_TOKEN' in s:
    sys.exit(0)
oauth = (
    '\n/* wasi back-port of the PG18 libpq OAuth API (no-op; see postgres-wasi). */\n'
    'typedef enum { PQAUTHDATA_PROMPT_OAUTH_DEVICE, PQAUTHDATA_OAUTH_BEARER_TOKEN } PGauthData;\n'
    'typedef struct _PGoauthBearerRequest {\n'
    '\tconst char *openid_configuration;\n'
    '\tconst char *scope;\n'
    '\tPostgresPollingStatusType (*async)(PGconn *conn, struct _PGoauthBearerRequest *req, int *altsock);\n'
    '\tvoid (*cleanup)(PGconn *conn, struct _PGoauthBearerRequest *req);\n'
    '\tchar *token;\n'
    '\tvoid *user;\n'
    '} PGoauthBearerRequest;\n'
    'typedef int (*PQauthDataHook_type)(PGauthData type, PGconn *conn, void *data);\n'
    'extern void PQsetAuthDataHook(PQauthDataHook_type hook);\n'
    'extern PQauthDataHook_type PQgetAuthDataHook(void);\n'
)
anchor = '#ifdef __cplusplus\n}\n#endif\n\n#endif'
if anchor not in s:
    sys.stderr.write('libpq-fe.h: end anchor not found\n'); sys.exit(1)
s = s.replace(anchor, oauth + anchor, 1)
open(p, 'w').write(s)
print('patched libpq-fe.h: PG18 OAuth API decls (no-op on wasi)')
PY
  fi
fi

# mysql_scanner: like postgres but links a PREBUILT MariaDB Connector/C
# (libmariadbclient.a). Replace find_package(libmysql) with cmake/mysql-deps.cmake
# (openssl-wasm + the shim + PG_WASI_REAL_NETDB) and add a static build (the
# pinned extension is DONT_LINK / loadable only). Networking reuses the postgres
# socket graft + getaddrinfo wrapper.
MY_SRC="$BUILD_DIR/_deps/mysql_scanner_extension_fc-src"
MY_DEPS_CMAKE="$(pwd)/cmake/mysql-deps.cmake"
if ext_selected mysql_scanner \
   && [[ -f "$MY_SRC/CMakeLists.txt" ]]; then
  python3 - "$MY_SRC/CMakeLists.txt" "$MY_DEPS_CMAKE" <<'PY'
import sys
p, inc = sys.argv[1], sys.argv[2]
s = open(p).read()
if 'mysql-deps.cmake' in s:
    sys.exit(0)
if 'find_package(libmariadb REQUIRED)' not in s:
    sys.stderr.write('mysql anchor not found: find_package(libmariadb)\n'); sys.exit(1)
s = s.replace('find_package(libmariadb REQUIRED)', 'include("%s")' % inc, 1)
# the extension resets MYSQL_INCLUDE_DIR to a vcpkg path absent on wasi; keep the
# mariadb include from mysql-deps.cmake.
s = s.replace('set(MYSQL_INCLUDE_DIR\n    ${CMAKE_BINARY_DIR}/vcpkg_installed/${VCPKG_TARGET_TRIPLET}/include/mysql)',
              '# MYSQL_INCLUDE_DIR provided by mysql-deps.cmake', 1)
anchor = 'build_loadable_extension(${TARGET_NAME} ${PARAMETERS} ${ALL_OBJECT_FILES})'
if anchor not in s:
    sys.stderr.write('mysql anchor not found: build_loadable_extension\n'); sys.exit(1)
add = (anchor + '\n'
       'build_static_extension(${TARGET_NAME} ${ALL_OBJECT_FILES})\n'
       'target_include_directories(${TARGET_NAME}_extension PRIVATE include ${MYSQL_INCLUDE_DIR})\n'
       'install(TARGETS ${TARGET_NAME}_extension EXPORT "${DUCKDB_EXPORT_SET}"\n'
       '        LIBRARY DESTINATION "${INSTALL_LIB_DIR}"\n'
       '        ARCHIVE DESTINATION "${INSTALL_LIB_DIR}")')
s = s.replace(anchor, add, 1)
open(p, 'w').write(s)
print('patched mysql CMakeLists: libmariadb deps + static build + export')
PY
  # mysql_scanner 1.5.4 also uses the database-connector git submodule.
  if [[ -d "$MY_SRC/.git" || -f "$MY_SRC/.git" ]]; then
    git -C "$MY_SRC" submodule update --init database-connector 2>/dev/null \
      && echo "initialized mysql_scanner database-connector submodule" >&2
  fi
  # postgres + mysql both vendor these database-connector helpers in the duckdb
  # namespace with different bodies (e.g. EscapeConnectionString escapes ' vs ")
  # -> duplicate-symbol clash when both are linked. They're single-file in the
  # mysql extension, so give them internal linkage (static).
  python3 - "$MY_SRC/src/storage/mysql_catalog.cpp" "$MY_SRC/src/storage/mysql_schema_entry.cpp" <<'PY'
import sys
defs = {
  'mysql_catalog.cpp': [('string EscapeConnectionString(const string &input) {',
                         'static string EscapeConnectionString(const string &input) {'),
                        ('unique_ptr<SecretEntry> GetSecret(ClientContext &context, const string &secret_name) {',
                         'static unique_ptr<SecretEntry> GetSecret(ClientContext &context, const string &secret_name) {')],
  'mysql_schema_entry.cpp': [('bool CatalogTypeIsSupported(CatalogType type) {',
                              'static bool CatalogTypeIsSupported(CatalogType type) {')],
}
for path in sys.argv[1:]:
    name = path.rsplit('/', 1)[-1]
    s = open(path).read()
    for old, new in defs.get(name, []):
        if old in s and new not in s:
            s = s.replace(old, new, 1)
    open(path, 'w').write(s)
print('patched mysql: static linkage for shared duckdb-namespace helpers')
PY
  # MariaDB Connector/C lacks MYSQL_OPT_SSL_MODE (the extension's mechanism); map
  # ssl_mode to MariaDB's MYSQL_OPT_SSL_ENFORCE on wasi.
  python3 - "$MY_SRC/src/mysql_utils.cpp" <<'PY'
import sys
p=sys.argv[1]; s=open(p).read()
old='''	if (config.ssl_mode != SSL_MODE_PREFERRED) {
		mysql_options(mysql, MYSQL_OPT_SSL_MODE, &config.ssl_mode);
	}'''
new='''#ifdef __wasi__
	{
		my_bool _ssl_enforce = (config.ssl_mode == SSL_MODE_REQUIRED ||
		                        config.ssl_mode == SSL_MODE_VERIFY_CA ||
		                        config.ssl_mode == SSL_MODE_VERIFY_IDENTITY) ? 1 : 0;
		mysql_options(mysql, MYSQL_OPT_SSL_ENFORCE, &_ssl_enforce);
	}
#else
	if (config.ssl_mode != SSL_MODE_PREFERRED) {
		mysql_options(mysql, MYSQL_OPT_SSL_MODE, &config.ssl_mode);
	}
#endif'''
if old in s and '_ssl_enforce' not in s:
    open(p,'w').write(s.replace(old,new,1)); print('patched mysql ssl_mode -> SSL_ENFORCE')
PY
fi
}

# delta: DuckDB 1.5.4's canonical `delta` extension is the out-of-tree
# duckdb-delta @ 45c40878 (from .github/config/extensions/delta.cmake), wrapping
# delta-kernel-rs v0.21.0. We vendor it to build/duckdb-delta. On wasm it uses the
# kernel's SYNC engine only (local std::fs; no tokio/reqwest/object_store). The
# kernel is prebuilt for wasm32-wasip2 (sync engine, zstd/brotli/flate2 codecs
# dropped, a get_sync_engine FFI constructor added) by build-delta-kernel-wasm.sh.
# This: (1) vendors 45c40878, (2) patches its CMakeLists -- forces
# RUST_PLATFORM_TARGET=wasm32-wasip2 (its OS detect FATAL_ERRORs on wasm) and no-ops
# the kernel ExternalProject's git-clone + cargo builds while KEEPING the cbindgen
# header-gen step (it post-processes the staged FFI header into codegen/include),
# (3) swaps the extension's default-engine builder construction to get_sync_engine
# under __wasi__, then (4) stages the prebuilt .a + headers where the CMakeLists
# expects them. Marker-guarded/idempotent. Runs before configure.
stage_delta_kernel() {
  ext_selected delta || return 0
  local OUT_DIR="$(pwd)/build/delta-kernel/out"
  if [[ ! -f "$OUT_DIR/libdelta_kernel_ffi.a" ]]; then
    echo "delta: building wasm sync-engine kernel" >&2
    OUT_DIR="$OUT_DIR" WASI_SDK_PREFIX="$WASI_SDK_PREFIX" \
      "$(pwd)/scripts/build-delta-kernel-wasm.sh"
  fi

  # (1) vendor duckdb-delta @ 45c40878 (the 1.5.4 canonical delta) if absent.
  local DELTA_DIR="$(pwd)/build/duckdb-delta"
  local DELTA_TAG="45c40878601b54b4188b09e08732fe0d576ad222"
  if [[ "$(git -C "$DELTA_DIR" rev-parse HEAD 2>/dev/null)" != "$DELTA_TAG" ]]; then
    echo "delta: vendoring duckdb-delta @ ${DELTA_TAG:0:8} into $DELTA_DIR" >&2
    rm -rf "$DELTA_DIR"
    git clone --quiet --filter=blob:none --no-checkout \
      https://github.com/duckdb/duckdb-delta "$DELTA_DIR"
    git -C "$DELTA_DIR" fetch --quiet --depth 1 origin "$DELTA_TAG"
    git -C "$DELTA_DIR" checkout --quiet "$DELTA_TAG"
  fi
  local CML="$DELTA_DIR/CMakeLists.txt"
  local MFL="$DELTA_DIR/src/functions/delta_scan/delta_multi_file_list.cpp"

  # (2) patch the CMakeLists: wasm platform target + no-op (but keep header-gen).
  if [[ -f "$CML" ]] && ! grep -q 'DUCKLINK_WASI_KERNEL_NOOP' "$CML"; then
    python3 - "$CML" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
# 2a) the CMakeLists FATAL_ERRORs when it can't map the OS to a rust triple (wasm).
# Force the wasip2 triple so DELTA_KERNEL_LIBPATH = target/wasm32-wasip2/<cfg>/...
s = s.replace(
    'message(FATAL_ERROR "Failed to detect the correct platform")',
    'set(RUST_PLATFORM_TARGET "wasm32-wasip2")  # DUCKLINK_WASI: prebuilt kernel triple')
# 2b) no-op the ExternalProject's download + cargo builds, but KEEP the cbindgen
# header-gen sh step (it reads the staged FFI hpp and writes the patched header to
# codegen/include, which the extension #includes). Replace the whole block.
start = s.index('ExternalProject_Add(')
i = s.index('(', start); depth = 0; j = i
while j < len(s):
    if s[j] == '(': depth += 1
    elif s[j] == ')':
        depth -= 1
        if depth == 0: break
    j += 1
noop = (
'ExternalProject_Add(\n'
'  ${KERNEL_NAME}\n'
'  # DUCKLINK_WASI_KERNEL_NOOP: prebuilt wasm sync-engine FFI is staged by\n'
'  # build-libduckdb-wasm.sh; skip git-clone + cargo, keep only the header-gen.\n'
'  DOWNLOAD_COMMAND ""\n'
'  CONFIGURE_COMMAND ""\n'
'  UPDATE_COMMAND ""\n'
'  BUILD_IN_SOURCE 1\n'
'  BUILD_COMMAND\n'
'    sh ${CMAKE_CURRENT_SOURCE_DIR}/scripts/ffi/generate_delta_kernel_ffi_header\n'
'      ${CMAKE_CURRENT_SOURCE_DIR}/scripts/ffi\n'
'      ${DELTA_KERNEL_FFI_HEADER_CXX}\n'
'      ${CMAKE_BINARY_DIR}/codegen/include\n'
'  INSTALL_COMMAND ""\n'
'  BUILD_BYPRODUCTS "${DELTA_KERNEL_LIBPATH}"\n'
'  BUILD_BYPRODUCTS "${DELTA_KERNEL_FFI_HEADER_C}"\n'
'  BUILD_BYPRODUCTS "${DELTA_KERNEL_FFI_HEADER_CXX}")\n'
'# DUCKLINK_WASI: the kernel FFI types use uintptr_t (32-bit on wasm32) where the\n'
'# extension passes idx_t (64-bit); the 64->32 braced-init narrowing is a hard\n'
'# error on wasm32 but harmless here (slice sizes never approach 4 GiB).\n'
'add_compile_options(-Wno-c++11-narrowing -Wno-narrowing)'
)
open(p, 'w').write(s[:start] + noop + s[j+1:])
print("delta: patched CMakeLists -> wasm triple + no-op kernel (header-gen kept)", file=sys.stderr)
PY
  fi

  # (3) swap the engine construction in delta_multi_file_list.cpp: the default
  #     engine (get_engine_builder + builder_build -> object_store/tokio) is gated
  #     out of the sync-only kernel; use get_sync_engine under __wasi__ instead, and
  #     compile out CreateBuilder (references the absent default-engine builder FFI).
  if [[ -f "$MFL" ]] && ! grep -q 'DUCKLINK_WASI_SYNC_ENGINE' "$MFL"; then
    python3 - "$MFL" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
# 3a) compile out CreateBuilder (default-engine-only FFI)
needle = 'static ffi::EngineBuilder *CreateBuilder('
k = s.index(needle)
s = (s[:k]
     + '// DUCKLINK_WASI_SYNC_ENGINE: CreateBuilder uses the default-engine builder\n'
       '// FFI (get_engine_builder/set_builder_option), gated out of the wasm\n'
       '// sync-only kernel. Compiled out on wasm; the sync engine is used instead.\n'
       '#ifndef __wasi__\n'
     + s[k:])
end = s.index('\n}\n', s.index(needle)) + len('\n}\n')
s = s[:end] + '#endif // __wasi__\n' + s[end:]
# 3b) swap the engine build (builder_build -> get_sync_engine) on wasm
old = ('\tauto interface_builder = CreateBuilder(*client_ctx_shared, paths[0].path);\n'
       '\textern_engine = TryUnpackKernelResult(ffi::builder_build(interface_builder));')
new = ('#ifdef __wasi__\n'
       '\t// wasm: link only the kernel\'s local-filesystem SYNC engine.\n'
       '\textern_engine = TryUnpackKernelResult(ffi::get_sync_engine(DuckDBEngineError::AllocateError));\n'
       '#else\n'
       '\tauto interface_builder = CreateBuilder(*client_ctx_shared, paths[0].path);\n'
       '\textern_engine = TryUnpackKernelResult(ffi::builder_build(interface_builder));\n'
       '#endif')
assert old in s, "delta_multi_file_list engine-build anchor not found"
s = s.replace(old, new)
open(p, 'w').write(s)
print("delta: patched delta_multi_file_list.cpp -> get_sync_engine on wasm", file=sys.stderr)
PY
  fi

  # (3c) guard the Unity Catalog commit branch in delta_transaction.cpp. UC commits
  #      go through get_uc_commit_client/get_uc_committer, which live in the kernel's
  #      delta-kernel-unity-catalog feature -- and that feature pulls tokio (no wasm
  #      build). UC over the network is unusable on wasm anyway, so compile the UC
  #      branch out under __wasi__ and always take the plain local transaction.
  local TXN="$DELTA_DIR/src/storage/delta_transaction.cpp"
  if [[ -f "$TXN" ]] && ! grep -q 'DUCKLINK_WASI_NO_UC' "$TXN"; then
    python3 - "$TXN" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
open_if = '\t\tif (parent_commit) {'
assert open_if in s, "delta_transaction UC if-branch not found"
s = s.replace(open_if,
    '#ifndef __wasi__  // DUCKLINK_WASI_NO_UC: UC commit needs tokio/network; use plain txn\n'
    + open_if, 1)
else_brace = ('\t\t} else {\n'
              '\t\t\tnew_kernel_transaction = table_entry->snapshot->TryUnpackKernelResult(\n'
              '\t\t\t    ffi::transaction(path_slice, table_entry->snapshot->extern_engine.get()));')
assert else_brace in s, "delta_transaction else-branch anchor not found"
s = s.replace(else_brace,
    '\t\t} else\n#endif // __wasi__\n\t\t{\n'
    '\t\t\tnew_kernel_transaction = table_entry->snapshot->TryUnpackKernelResult(\n'
    '\t\t\t    ffi::transaction(path_slice, table_entry->snapshot->extern_engine.get()));', 1)
open(p, 'w').write(s)
print("delta: patched delta_transaction.cpp -> no UC commit on wasm", file=sys.stderr)
PY
  fi

  # (4) pre-place the kernel .a + headers where the (now no-op) ExternalProject +
  #     target_link_libraries expect them: target/wasm32-wasip2/{release,debug}/ +
  #     target/ffi-headers/ (the header-gen sh reads the .hpp from there).
  local KDIR="$BUILD_DIR/rust/src/delta_kernel/target"
  mkdir -p "$KDIR/wasm32-wasip2/release" "$KDIR/wasm32-wasip2/debug" "$KDIR/ffi-headers"
  cp "$OUT_DIR/libdelta_kernel_ffi.a" "$KDIR/wasm32-wasip2/release/"
  cp "$OUT_DIR/libdelta_kernel_ffi.a" "$KDIR/wasm32-wasip2/debug/"
  cp "$OUT_DIR/ffi-headers/"* "$KDIR/ffi-headers/" 2>/dev/null || true
  echo "delta: staged prebuilt wasm kernel ($(du -h "$OUT_DIR/libdelta_kernel_ffi.a" | cut -f1))" >&2
}

# aws: the `aws` extension resolves AWS credentials for httpfs's S3 secrets. The
# AWS C++ SDK doesn't build for wasm, so we vendor duckdb-aws (the version-matched
# pin) + patch it to resolve credentials natively (env vars + ~/.aws INI files +
# region) under __wasi__ via cmake/aws-wasi/aws_wasi_credentials.hpp. Pure C++,
# no extra deps. Runs before configure (in-tree extension read at configure time).
stage_aws_extension() {
  ext_selected aws || return 0
  local AWS_DIR="$DUCKDB_SOURCE_DIR/extension/aws"
  local PIN="812ce80fde0bfa6e4641b6fd798087349a610795"
  if [[ ! -f "$AWS_DIR/CMakeLists.txt" ]]; then
    echo "aws: vendoring duckdb-aws @ $PIN" >&2
    local tmp="$BUILD_DIR/duckdb-aws-src"
    if [[ ! -d "$tmp/.git" ]]; then
      git clone --quiet https://github.com/duckdb/duckdb-aws "$tmp"
      git -C "$tmp" checkout --quiet "$PIN"
    fi
    mkdir -p "$AWS_DIR"
    ( cd "$tmp" && git archive "$PIN" CMakeLists.txt src ) | tar -x -C "$AWS_DIR"
  fi
  # Apply the wasm patches + drop in the native resolver header (idempotent).
  if ! grep -q '__wasi__' "$AWS_DIR/src/aws_secret.cpp" 2>/dev/null; then
    cp "$(pwd)/cmake/aws-wasi/aws_wasi_credentials.hpp" "$AWS_DIR/src/include/"
    for p in "$(pwd)"/cmake/aws-wasi/aws-812ce80-*.patch; do
      ( cd "$AWS_DIR" && git apply "$p" 2>/dev/null ) || patch -p1 -d "$AWS_DIR" < "$p"
    done
    echo "aws: applied wasm patches + native credential resolver" >&2
  fi
}

# azure: build the Azure SDK for wasm (if needed) + vendor/patch the extension.
# DuckDB 1.5.4's canonical azure is duckdb-azure @ 563589b2 (out-of-tree, from
# .github/config/extensions/azure.cmake); we vendor it to build/duckdb-azure. The
# Azure SDK for C++ doesn't build under vcpkg for wasm, so it's prebuilt for
# wasm32-wasip2 (libcurl transport over curl-wasm) and merged into libduckdb-wasi.a
# below; the CMakeLists is patched to use the prebuilt SDK headers + register a
# static extension, and the curl transport carries an embedded CA bundle
# (CURLOPT_CAINFO_BLOB) since openssl-wasm can't read a CA file via the wrapped FS.
stage_azure_extension() {
  ext_selected azure || return 0
  if [[ ! -f "$(pwd)/build/azure-sdk/out/lib/libazure-storage-blobs.a" ]]; then
    echo "azure: building Azure SDK for wasm" >&2
    WASI_SDK_PREFIX="$WASI_SDK_PREFIX" "$(pwd)/scripts/build-azure-sdk-wasm.sh"
  fi
  local AZ_DIR="$(pwd)/build/duckdb-azure"
  local PIN="563589b2f24290a4dcdd4247eaedf2b544f9dbcd"
  if [[ "$(git -C "$AZ_DIR" rev-parse HEAD 2>/dev/null)" != "$PIN" ]]; then
    echo "azure: vendoring duckdb-azure @ ${PIN:0:8} into $AZ_DIR" >&2
    rm -rf "$AZ_DIR"
    git clone --quiet --filter=blob:none --no-checkout \
      https://github.com/duckdb/duckdb-azure "$AZ_DIR"
    git -C "$AZ_DIR" fetch --quiet --depth 1 origin "$PIN"
    git -C "$AZ_DIR" checkout --quiet "$PIN"
  fi
  # Patch the CMakeLists (prebuilt SDK headers + build_static_extension) + the curl
  # transport (embedded CA bundle via CURLOPT_CAINFO_BLOB) for wasm. git apply, with
  # a fuzzy `patch` fallback (the source list drifts between azure versions).
  if ! grep -q 'AZURE_SDK_WASM_DIR' "$AZ_DIR/CMakeLists.txt" 2>/dev/null; then
    for p in "$(pwd)"/cmake/azure-deps/azure-563589b2-CMakeLists.txt.patch \
             "$(pwd)"/cmake/azure-deps/azure-563589b2-ca-bundle-blob.patch; do
      ( cd "$AZ_DIR" && git apply "$p" 2>/dev/null ) \
        || patch -p1 --fuzz=3 -d "$AZ_DIR" < "$p"
    done
    echo "azure: applied wasm CMakeLists + CA-bundle patches" >&2
  fi
  # Embed the CA bundle (httpfs's) for CURLOPT_CAINFO_BLOB. Regenerate if missing.
  local CA_BUNDLE="$(pwd)/cmake/ca-bundle/cacert.pem"
  if [[ -f "$CA_BUNDLE" && ! -f "$AZ_DIR/src/azure_ca_bundle.inc" ]]; then
    { printf '{'; xxd -i < "$CA_BUNDLE"; printf '}'; } > "$AZ_DIR/src/azure_ca_bundle.inc"
    echo "azure: embedded CA bundle ($(grep -c 'BEGIN CERTIFICATE' "$CA_BUNDLE") certs)" >&2
  fi
}

# ui: the real DuckDB UI. httplib can't listen() in the wasip2 sandbox, so the
# native host (ducklink ui) owns the socket and bridges each request to the
# extension's HttpServer::HandleRequest (exposed as duckdb_ui_handle_request).
# Vendor duckdb-ui @ ded075b (DuckDB 1.4.0) + apply the wasm patches (cmake/ui-deps/).
stage_ui_extension() {
  ext_selected ui || return 0
  # DuckDB 1.5.4's matching UI is duckdb-ui @ a135471 (the "duckdb 1.5.4 and 1.4.5"
  # release, #61; not pinned in .github/config -- ui autoloads via start_ui()).
  # Vendor it to build/duckdb-ui (the 1.4-era ded075b uses the removed ExtensionUtil
  # API). The native host owns the listening socket; the wasm patches bridge each
  # request to duckdb_ui_handle_request (httplib can't listen() in wasip2).
  local UI_DIR="$(pwd)/build/duckdb-ui"
  local PIN="a135471122605f528d3f3c058ad420c30c8ef235"
  if [[ "$(git -C "$UI_DIR" rev-parse HEAD 2>/dev/null)" != "$PIN" ]]; then
    echo "ui: vendoring duckdb-ui @ ${PIN:0:8} into $UI_DIR" >&2
    rm -rf "$UI_DIR"
    git clone --quiet --filter=blob:none --no-checkout \
      https://github.com/duckdb/duckdb-ui "$UI_DIR"
    git -C "$UI_DIR" fetch --quiet --depth 1 origin "$PIN"
    git -C "$UI_DIR" checkout --quiet "$PIN"
  fi
  # Apply the wasm patches (httplib AF_UNIX/getnameinfo, watcher no-op, the request
  # bridge, system()-guard, CMakeLists). Idempotent (guard on the bridge marker).
  if ! grep -q 'duckdb_ui_handle_request' "$UI_DIR/src/http_server.cpp" 2>/dev/null; then
    for p in "$(pwd)"/cmake/ui-deps/ui-a135471-*.patch; do
      ( cd "$UI_DIR" && git apply "$p" 2>/dev/null ) || patch -p1 --fuzz=3 -d "$UI_DIR" < "$p"
    done
    echo "ui: applied wasm patches" >&2
  fi
}

# delta: stage the prebuilt wasm kernel before configure (the vendored delta
# CMakeLists references the staged .a as an ExternalProject byproduct).
stage_delta_kernel
stage_aws_extension
stage_azure_extension
stage_ui_extension

# Configure, patching fetched sources after each failure, until it succeeds.
# Extensions are fetched progressively, so a configure-blocking extension only
# becomes patchable once the earlier-failing one is fixed (hence the loop).
attempt=0
until configure_duckdb; do
  attempt=$((attempt + 1))
  if [[ $attempt -ge 6 ]]; then
    echo "configure still failing after $attempt attempts" >&2; exit 1
  fi
  echo "configure attempt $attempt failed; patching fetched sources + retrying" >&2
  apply_extension_patches
done
# Final pass for compile-only source patches (httpfs CA bundle, curl default) on
# sources fetched in the successful configure.
apply_extension_patches

# TVM spill: route the buffer manager's temporary-block spill (evicted blocks +
# larger-than-memory sort/hash/aggregate) to host-owned Tiered Virtual Memory
# regions (cmake/.. crates/ducklink-core/src/tvm_spill.rs) instead of
# temp files -- extending capacity past the wasm32 4 GiB ceiling. The hooks fall
# back to temp files when no TVM host is wired, so this is always safe.
python3 - "$DUCKDB_SOURCE_DIR/src/storage/standard_buffer_manager.cpp" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
if 'tvm_spill_write' in s:
    sys.exit(0)
# 1) extern "C" decls of the bridge (wasm-only)
decls = ('\n#ifdef __wasi__\n'
         'extern "C" {\n'
         '// TVM spill bridge (crates/ducklink-core/src/tvm_spill.rs).\n'
         'int tvm_spill_write(uint8_t tag, int64_t block_id, const uint8_t *data,\n'
         '                    uint64_t alloc_size, uint64_t logical_size, uint64_t header_size);\n'
         'int tvm_spill_query(int64_t block_id, uint64_t *out_logical, uint64_t *out_header);\n'
         'int tvm_spill_read(int64_t block_id, uint8_t *out, uint64_t capacity);\n'
         'uint64_t tvm_spill_delete(int64_t block_id);\n'
         '}\n#endif\n')
s = s.replace('namespace duckdb {\n', 'namespace duckdb {\n' + decls, 1)
# 2) WriteTemporaryBuffer -> TVM first
wanchor = 'void StandardBufferManager::WriteTemporaryBuffer(MemoryTag tag, block_id_t block_id, FileBuffer &buffer) {\n'
s = s.replace(wanchor, wanchor +
    '#ifdef __wasi__\n'
    '\tif (tvm_spill_write(uint8_t(tag), block_id, buffer.InternalBuffer(), buffer.AllocSize(),\n'
    '\t                    buffer.size, buffer.GetHeaderSize())) {\n'
    '\t\tevicted_data_per_tag[uint8_t(tag)] += buffer.AllocSize();\n'
    '\t\treturn;\n'
    '\t}\n#endif\n', 1)
# 3) ReadTemporaryBuffer -> TVM first
ranchor = 'unique_ptr<FileBuffer> reusable_buffer) {\n\tD_ASSERT(!temporary_directory.path.empty());'
s = s.replace(ranchor,
    'unique_ptr<FileBuffer> reusable_buffer) {\n'
    '#ifdef __wasi__\n'
    '\t{\n'
    '\t\tuint64_t tvm_logical = 0, tvm_header = 0;\n'
    '\t\tif (tvm_spill_query(block.BlockId(), &tvm_logical, &tvm_header)) {\n'
    '\t\t\tauto tvm_buffer = ConstructManagedBuffer(tvm_logical, tvm_header, std::move(reusable_buffer));\n'
    '\t\t\ttvm_spill_read(block.BlockId(), tvm_buffer->InternalBuffer(), tvm_buffer->AllocSize());\n'
    '\t\t\treturn tvm_buffer;\n'
    '\t\t}\n'
    '\t}\n#endif\n'
    '\tD_ASSERT(!temporary_directory.path.empty());', 1)
# 4) DeleteTemporaryFile -> TVM first
danchor = 'void StandardBufferManager::DeleteTemporaryFile(BlockHandle &block) {\n\tauto id = block.BlockId();\n'
s = s.replace(danchor, danchor +
    '#ifdef __wasi__\n'
    '\t{\n'
    '\t\tuint64_t tvm_freed = tvm_spill_delete(id);\n'
    '\t\tif (tvm_freed) {\n'
    '\t\t\tevicted_data_per_tag[uint8_t(block.GetMemoryTag())] -= tvm_freed;\n'
    '\t\t\treturn;\n'
    '\t\t}\n'
    '\t}\n#endif\n', 1)
open(p, 'w').write(s)
print('patched standard_buffer_manager.cpp: TVM spill hooks')
PY

# TVM spill, part 2: make a temp block evictable when a TVM host is wired even
# with no temporary directory. BlockHandle::CanUnload otherwise refuses to unload
# such a block (no spill target), which surfaces as "Unused blocks cannot be
# offloaded to disk" before WriteTemporaryBuffer (and its TVM hook) is reached.
python3 - "$DUCKDB_SOURCE_DIR/src/storage/buffer/block_handle.cpp" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
if 'tvm_spill_available' in s:
    sys.exit(0)
decl = ('\n#ifdef __wasi__\n'
        '// TVM spill bridge (crates/ducklink-core/src/tvm_spill.rs).\n'
        'extern "C" int tvm_spill_available();\n'
        '#endif\n')
s = s.replace('namespace duckdb {\n', 'namespace duckdb {\n' + decl, 1)
# DuckDB 1.5.4 moved CanUnload into BlockMemory and collapsed the guard onto one
# line with accessor calls (BlockId()/GetBufferManager()); earlier versions used
# block_manager.buffer_manager across two lines. Handle both forms.
candidates = [
    # 1.5.4+ single-line form
    ('\tif (BlockId() >= MAXIMUM_BLOCK && MustWriteToTemporaryFile() && '
     '!GetBufferManager().HasTemporaryDirectory()) {',
     '\tif (BlockId() >= MAXIMUM_BLOCK && MustWriteToTemporaryFile() && '
     '!GetBufferManager().HasTemporaryDirectory()\n'
     '#ifdef __wasi__\n'
     '\t    && !tvm_spill_available()\n'
     '#endif\n'
     '\t    ) {'),
    # pre-1.5 two-line form
    ('\tif (block_id >= MAXIMUM_BLOCK && MustWriteToTemporaryFile() &&\n'
     '\t    !block_manager.buffer_manager.HasTemporaryDirectory()) {',
     '\tif (block_id >= MAXIMUM_BLOCK && MustWriteToTemporaryFile() &&\n'
     '\t    !block_manager.buffer_manager.HasTemporaryDirectory()\n'
     '#ifdef __wasi__\n'
     '\t    && !tvm_spill_available()\n'
     '#endif\n'
     '\t    ) {'),
]
for anchor, repl in candidates:
    if anchor in s:
        s = s.replace(anchor, repl, 1)
        break
else:
    sys.exit('block_handle.cpp: CanUnload anchor not found')
open(p, 'w').write(s)
print('patched block_handle.cpp: TVM makes temp blocks evictable without a temp dir')
PY

# DuckDB 1.5.4's TaskScheduler::GetEstimatedCPUId only special-cases
# __EMSCRIPTEN__ (returns 0); a wasi (non-emscripten) build falls into the
# _GNU_SOURCE branch and calls sched_getcpu(), which the wasi sysroot does not
# provide. Treat __wasi__ like emscripten -> a single estimated CPU id.
python3 - "$DUCKDB_SOURCE_DIR/src/parallel/task_scheduler.cpp" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
anchor = 'idx_t TaskScheduler::GetEstimatedCPUId() {\n#if defined(__EMSCRIPTEN__)'
new = 'idx_t TaskScheduler::GetEstimatedCPUId() {\n#if defined(__EMSCRIPTEN__) || defined(__wasi__)'
if new in s:
    sys.exit(0)
if anchor not in s:
    sys.exit('task_scheduler.cpp: GetEstimatedCPUId anchor not found')
s = s.replace(anchor, new, 1)
open(p, 'w').write(s)
print('patched task_scheduler.cpp: wasi has no sched_getcpu (estimated cpu id = 0)')
PY

# DuckDB 1.5.4's block_allocator.cpp calls madvise(MADV_FREE_REUSABLE/DONTNEED)
# to release physical pages on alloc/dealloc; the wasi sysroot has no madvise.
# z/OS (__MVS__) already treats "madvise unavailable" as a no-op success -- join
# __wasi__ to that branch instead of the madvise-calling #else.
python3 - "$DUCKDB_SOURCE_DIR/src/storage/block_allocator.cpp" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
old = '#elif defined(__MVS__)'
new = '#elif defined(__MVS__) || defined(__wasi__)'
if new in s:
    sys.exit(0)
if old not in s:
    sys.exit('block_allocator.cpp: __MVS__ madvise guard not found')
s = s.replace(old, new)  # OnAllocation + OnDeallocation
open(p, 'w').write(s)
print('patched block_allocator.cpp: wasi has no madvise (no-op success, like __MVS__)')
PY

# DuckDB 1.5.4 takes an fcntl(F_SETLK) advisory lock when opening a disk
# database; the wasi sysroot's fcntl(F_SETLK) returns ENOSYS, which DuckDB turns
# into "Could not set lock on file". wasi is a single-process sandbox with no
# concurrent access, so the lock is meaningless -- report success (rc=0) on wasi
# so the lock path is skipped (only the first/write-lock attempt is reachable;
# the F_GETLK retry is gated behind has_error, which never fires here).
python3 - "$DUCKDB_SOURCE_DIR/src/common/local_file_system.cpp" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
if 'wasi: single-process sandbox, no file locking' in s:
    sys.exit(0)
old = 'rc = fcntl(fd, F_SETLK, &fl);'
new = ('#ifdef __wasi__\n'
       '\t\t\t\trc = 0; // wasi: single-process sandbox, no file locking\n'
       '#else\n'
       '\t\t\t\trc = fcntl(fd, F_SETLK, &fl);\n'
       '#endif')
i = s.find(old)  # first occurrence = the initial write-lock attempt
if i == -1:
    sys.exit('local_file_system.cpp: F_SETLK anchor not found')
s = s[:i] + new + s[i + len(old):]
open(p, 'w').write(s)
print('patched local_file_system.cpp: wasi skips fcntl(F_SETLK) file locking')
PY

# Route DuckDB's LOAD to the ducklink host component loader. The wasm toolchain
# compiles with -DDUCKDB_DISABLE_EXTENSION_LOAD (no dlopen), so 1.5.4's
# LoadExternalExtensionInternal throws immediately. Before throwing, call the
# core's duckdb_component_load_extension(name): if the host loads a matching
# duckdb:extension WebAssembly component (functions already registered on the
# active connection), mark the load finished and return; otherwise fall through.
python3 - "$DUCKDB_SOURCE_DIR/src/main/extension/extension_load.cpp" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
if 'duckdb_component_load_extension' in s:
    sys.exit(0)
# 1. Declare the core's C bridge at global scope (extern "C" is not allowed in a
#    function body).
decl = 'extern "C" bool duckdb_component_load_extension(const char *name);\n\n'
ns = s.find('namespace duckdb {\n')
if ns == -1:
    sys.exit('extension_load.cpp: namespace duckdb not found')
s = s[:ns] + decl + s[ns:]
# 2. Route the disabled-load throw (in LoadExternalExtensionInternal, which has
#    `&info`; the identical throw in InitialLoad is unreachable when the macro is
#    defined) to the component loader.
fn = s.find('ExtensionActiveLoad &info) {')
if fn == -1:
    sys.exit('extension_load.cpp: LoadExternalExtensionInternal signature not found')
needle = '\tthrow PermissionException("Loading external extensions is disabled through a compile time flag");'
i = s.find(needle, fn)
if i == -1:
    sys.exit('extension_load.cpp: disabled-load throw anchor not found')
inject = (
    '\tif (duckdb_component_load_extension(extension.c_str())) {\n'
    '\t\tExtensionInstallInfo wasm_install_info;\n'
    '\t\twasm_install_info.mode = ExtensionInstallMode::STATICALLY_LINKED;\n'
    '\t\twasm_install_info.full_path = extension;\n'
    '\t\tinfo.FinishLoad(wasm_install_info);\n'
    '\t\treturn;\n'
    '\t}\n'
)
s = s[:i] + inject + s[i:]
open(p, 'w').write(s)
print('patched extension_load.cpp: LOAD routes to host component loader on wasi')
PY

# icu extension: 1.5.4 bundles an ICU whose putil.cpp uses U_TZSET (=tzset) and
# U_TIMEZONE (=timezone) on unrecognised platforms. wasi has no tzset, and
# `timezone` is a struct (sys/time.h), not the POSIX global. Add a wasi branch to
# putilimp.h's macro chains: U_TZSET no-op, U_TIMEZONE 0 (UTC; real zone comes
# from getenv("TZ") / SET TimeZone). Only matters when icu is embedded; idempotent.
ICU_PUTILIMP="$DUCKDB_SOURCE_DIR/extension/icu/third_party/icu/common/putilimp.h"
if [[ -f "$ICU_PUTILIMP" ]]; then
  python3 - "$ICU_PUTILIMP" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
if 'wasi: no tzset' in s:
    sys.exit(0)
subs = [
    ('#else\n#   define U_TZSET tzset',
     '#elif defined(__wasi__)\n#   define U_TZSET() /* wasi: no tzset */\n#else\n#   define U_TZSET tzset'),
    ('#else\n#   define U_TIMEZONE timezone',
     '#elif defined(__wasi__)\n#   define U_TIMEZONE 0 /* wasi: UTC */\n#else\n#   define U_TIMEZONE timezone'),
]
for old, new in subs:
    if old not in s:
        sys.exit('putilimp.h: anchor not found: %r' % old[:40])
    s = s.replace(old, new, 1)
open(p, 'w').write(s)
print('patched icu putilimp.h: wasi U_TZSET/U_TIMEZONE')
PY
fi

# icu's bundled double-conversion errors out on unrecognised architectures; add
# wasm32 to the supported list (it is a standard IEEE-754 little-endian target).
ICU_DC="$DUCKDB_SOURCE_DIR/extension/icu/third_party/icu/i18n/double-conversion-utils.h"
if [[ -f "$ICU_DC" ]]; then
  python3 - "$ICU_DC" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
if '__wasm__' in s:
    sys.exit(0)
old = '#if defined(_M_X64) || defined(__x86_64__) || \\'
new = '#if defined(__wasm__) || defined(__wasm32__) || defined(_M_X64) || defined(__x86_64__) || \\'
if old not in s:
    sys.exit('double-conversion-utils.h: arch anchor not found')
s = s.replace(old, new, 1)
open(p, 'w').write(s)
print('patched icu double-conversion-utils.h: wasm32 supported arch')
PY
fi

echo "Building libduckdb static archive" >&2
cmake --build "$BUILD_DIR" --target duckdb_static

# The extension config (cmake/wasm-extension-config.cmake) defines a
# `sqlite_wasivfs` static lib (the WASI VFS + sqlite3_os_init backing
# sqlite_scanner's vendored sqlite3.c). Build it and merge it below.
WASIVFS_LIB=""
if cmake --build "$BUILD_DIR" --target sqlite_wasivfs >&2; then
  WASIVFS_LIB="$(find "$BUILD_DIR" -name 'libsqlite_wasivfs.a' -print -quit)"
fi

STATIC_LIB="$(find "$BUILD_DIR" -name 'libduckdb_static.a' -print -quit)"
if [[ -z "$STATIC_LIB" ]]; then
  echo "libduckdb_static.a not found; check the build output" >&2
  exit 1
fi

ARTIFACTS_DIR=${ARTIFACTS_DIR:-"$(pwd)/artifacts"}
mkdir -p "$ARTIFACTS_DIR"
# Merge DuckDB with the C++ runtime archives so downstream consumers
# do not need to manually link libc++/libc++abi when building components. Use
# the `eh` multilib (exception-handling) variants plus libunwind so the merged
# archive carries the runtime that DuckDB's `-fwasm-exceptions` code needs.
SYSROOT_LIBDIR="$WASI_SDK_PREFIX/share/wasi-sysroot/lib/${WASI_TARGET_TRIPLE:-wasm32-wasip1-threads}/eh"
if [[ ! -d "$SYSROOT_LIBDIR" ]]; then
  echo "Expected exception-handling sysroot lib directory '$SYSROOT_LIBDIR' not found (needs wasi-sdk >= 33)" >&2
  exit 1
fi

TMPDIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMPDIR"
}
trap cleanup EXIT

cp "$STATIC_LIB" "$TMPDIR/libduckdb_base.a"
cp "$SYSROOT_LIBDIR/libc++abi.a" "$TMPDIR/libc++abi.a"
cp "$SYSROOT_LIBDIR/libc++.a" "$TMPDIR/libc++.a"
cp "$SYSROOT_LIBDIR/libunwind.a" "$TMPDIR/libunwind.a"
ADDLIBS=$'ADDLIB libduckdb_base.a\nADDLIB libc++abi.a\nADDLIB libc++.a\nADDLIB libunwind.a'

# DuckDB 1.5.4 emits ExtensionHelper::LoadAllExtensions (called from
# database.cpp) in $BUILD_DIR/codegen/src/generated_extension_loader.cpp, but its
# codegen target is not linked into duckdb_static for the wasm build, leaving the
# symbol undefined at component link. Compile it with the exact flags of a
# sibling duckdb object (from compile_commands.json) and merge it in. Guarded on
# the file existing, so older DuckDB (which defined it elsewhere) is unaffected.
GEN_LOADER="$BUILD_DIR/codegen/src/generated_extension_loader.cpp"
# LoadAllExtensions iterates LinkedExtensions() in order; an extension whose Load()
# grabs another's functions (delta -> parquet_scan) fails if its dependency hasn't
# loaded yet (autoload is disabled in wasm). Reorder so the foundational extensions
# (core_functions, parquet) come first. Idempotent / no-op if already first.
if [[ -f "$GEN_LOADER" ]] && ! grep -q 'DUCKLINK_LOADER_REORDER' "$GEN_LOADER"; then
  python3 - "$GEN_LOADER" <<'PY'
import re, sys
p = sys.argv[1]
s = open(p).read()
m = re.search(r'(vector<string>\s+VEC\s*=\s*\{)(.*?)(\};)', s, re.S)
if m:
    names = re.findall(r'"([a-z_0-9]+)"', m.group(2))
    first = [n for n in ('core_functions', 'parquet') if n in names]
    rest = [n for n in names if n not in first]
    ordered = first + rest
    body = '\n' + ''.join('\t"%s",\n' % n for n in ordered)
    body = body.rstrip(',\n') + '\n    '
    s = s[:m.start()] + '// DUCKLINK_LOADER_REORDER: deps (core_functions, parquet) first\n    ' \
        + m.group(1) + body + m.group(3) + s[m.end():]
    open(p, 'w').write(s)
    print("loader: reordered LinkedExtensions -> %s" % ', '.join(ordered), file=sys.stderr)
PY
fi
if [[ -f "$GEN_LOADER" && -f "$BUILD_DIR/compile_commands.json" ]]; then
  GEN_CMD="$(python3 - "$BUILD_DIR/compile_commands.json" "$GEN_LOADER" "$TMPDIR/genextloader.o" "$BUILD_DIR" "$DUCKDB_SOURCE_DIR" <<'PY'
import json, os, re, shlex, sys
cc = json.load(open(sys.argv[1]))
src, out, bdir, sdir = sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
ref = next(e for e in cc if e["file"].endswith("ub_duckdb_main.cpp"))
cmd = ref["command"].replace(ref["file"], src)
cmd = re.sub(r'-o\s+\S+', '-o ' + shlex.quote(out), cmd)
# The loader resolves each linked extension's class (CoreFunctionsExtension, ...)
# through generated_extension_headers.hpp, gated on GENERATED_EXTENSION_HEADERS.
# Embedded extensions live both in-tree (extension/<n>/include) and fetched into
# the build dir, so harvest EVERY -I dir CMake used across the whole compilation
# (from compile_commands.json) -- guarantees all extension headers resolve.
incs = set()
for e in cc:
    incs.update(re.findall(r'-I\s*(\S+)', e['command']))
extra = ['-DGENERATED_EXTENSION_HEADERS', '-I' + os.path.join(bdir, 'codegen', 'include')]
extra += ['-I' + i for i in sorted(incs)]
print(cmd + ' ' + ' '.join(shlex.quote(x) for x in extra))
PY
)"
  echo "Compiling generated_extension_loader.cpp (LoadAllExtensions) for the archive" >&2
  eval "$GEN_CMD"
  "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libgenextloader.a" "$TMPDIR/genextloader.o"
  ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libgenextloader.a"
  echo "Merging generated extension loader into libduckdb-wasi.a" >&2

  # The loader calls LoadStaticExtension<XxxExtension> for each linked extension,
  # but 1.5.4 builds each extension as its own static-lib target
  # (extension/<name>/lib<name>_extension.a) that is NOT linked into duckdb_static
  # for the wasm build -- so neither the extension class nor its implementation is
  # in the archive. Build each linked extension's target and merge its whole
  # archive. The set comes from the loader's `extension=="NAME"` dispatch.
  for ext in $(grep -oE 'extension=="[a-z_0-9]+"' "$GEN_LOADER" | sed -E 's/.*"([a-z_0-9]+)".*/\1/' | sort -u); do
    cmake --build "$BUILD_DIR" --target "${ext}_extension" >/dev/null 2>&1 || true
    EXT_A="$(find "$BUILD_DIR/extension/$ext" -name "lib${ext}_extension.a" 2>/dev/null | head -1)"
    if [[ -z "$EXT_A" || ! -f "$EXT_A" ]]; then
      echo "warning: lib${ext}_extension.a not found; ${ext} symbols may be unresolved" >&2
      continue
    fi
    cp "$EXT_A" "$TMPDIR/libext_${ext}.a"
    # sqlite_scanner vendors a full sqlite3 (sqlite3.c.obj) that collides with
    # spatial's shared sqlite3 amalgamation (merged as libsqlite3uri, carrying the
    # memvfs + wasi VFS that the core's database open relies on). Two sqlite3
    # copies corrupt VFS registration -> "Could not open sqlite3 memvfs database".
    # When both are embedded, drop sqlite_scanner's copy so all sqlite3 calls
    # resolve to the single shared amalgamation.
    if [[ "$ext" == "sqlite_scanner" ]] && ext_selected spatial; then
      "$WASI_SDK_PREFIX/bin/llvm-ar" d "$TMPDIR/libext_${ext}.a" sqlite3.c.obj 2>/dev/null \
        && echo "  dropped sqlite_scanner's vendored sqlite3.c.obj (uses shared sqlite3)" >&2
    fi
    ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libext_${ext}.a"
    echo "Merging ${ext} extension (full static lib) into libduckdb-wasi.a" >&2
  done
fi

if [[ -n "$WASIVFS_LIB" && -f "$WASIVFS_LIB" ]]; then
  cp "$WASIVFS_LIB" "$TMPDIR/libsqlite_wasivfs.a"
  ADDLIBS="$ADDLIBS"$'\nADDLIB libsqlite_wasivfs.a'
  echo "Merging WASI VFS ($WASIVFS_LIB) into libduckdb-wasi.a" >&2
fi

# httpfs links openssl (openssl-wasm: socket-capable) + libcurl/zlib/zstd
# (curl-wasm). Merge them so the core resolves SSL/EVP/curl/inflate symbols. One
# openssl (openssl-wasm); curl's openssl symbols resolve from it. Only when httpfs
# is enabled in the config; harmless if the libs are absent.
OPENSSL_WASM_BUILD="${OPENSSL_WASM_BUILD:-$HOME/git/openssl-wasm/build/openssl}"
CURL_WASM_BUILD="${CURL_WASM_BUILD:-$HOME/git/curl-wasm/build}"

# postgres_scanner 1.5.4 links a prebuilt libpq.a (built from the staged
# wasi-cross PG-15.13 tree by build-wasi-deps.sh) instead of compiling it inline.
# Merge libpq into the archive so the static extension resolves PQ* symbols; pull
# openssl too unless httpfs already merges it (shared openssl-wasm).
if ext_selected postgres_scanner; then
  _PGI="$(pwd)/build/wasi-deps/src/postgresql-15.13/src"
  # libpq.a (frontend) + libpgport/libpgcommon shlib variants it references
  # (pg_snprintf / pg_getaddrinfo_all / encoding helpers).
  pgdeps=("$_PGI/interfaces/libpq/libpq.a"
          "$_PGI/port/libpgport_shlib.a" "$_PGI/common/libpgcommon_shlib.a")
  ext_selected httpfs \
    || pgdeps+=("$OPENSSL_WASM_BUILD/libssl.a" "$OPENSSL_WASM_BUILD/libcrypto.a")
  for src in "${pgdeps[@]}"; do
    name="$(basename "$src")"
    if [[ -f "$src" ]]; then
      cp "$src" "$TMPDIR/$name"
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB $name"
      echo "Merging postgres dep ($src) into libduckdb-wasi.a" >&2
    fi
  done
fi

if ext_selected httpfs; then
  # curl-wasm is built with HTTP/2 (nghttp2) + HTTP/3 (USE_NGTCP2: ngtcp2 +
  # nghttp3). Merge those too, alongside openssl/zlib/zstd/brotli.
  for src in "$OPENSSL_WASM_BUILD/libssl.a" "$OPENSSL_WASM_BUILD/libcrypto.a" \
             "$CURL_WASM_BUILD/curl/lib/libcurl.a" "$CURL_WASM_BUILD/zlib/lib/libz.a" \
             "$CURL_WASM_BUILD/zstd/lib/libzstd.a" \
             "$CURL_WASM_BUILD/brotli/lib/libbrotlidec.a" \
             "$CURL_WASM_BUILD/brotli/lib/libbrotlicommon.a" \
             "$CURL_WASM_BUILD/nghttp2/lib/libnghttp2.a" \
             "$CURL_WASM_BUILD/ngtcp2/lib/libngtcp2.a" \
             "$CURL_WASM_BUILD/ngtcp2/lib/libngtcp2_crypto_ossl.a" \
             "$CURL_WASM_BUILD/nghttp3/lib/libnghttp3.a"; do
    name="$(basename "$src")"
    if [[ -f "$src" ]]; then
      cp "$src" "$TMPDIR/$name"
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB $name"
      echo "Merging httpfs dep ($src) into libduckdb-wasi.a" >&2
    fi
  done

  # cargo-component builds the core as a wasm32-wasip1 module (+ p1->p2 adapter),
  # and the wasip1 libc has NO BSD sockets. openssl-wasm/curl/httplib call
  # socket/connect/bind/...; those live only in the wasm32-wasip2 libc as thin
  # shims over wasi:sockets. Graft the exact socket objects PLUS the generated
  # component-binding glue they call (descriptor_table.c.obj + wasip2.c.obj, which
  # provide poll_poll / streams_method_* / network_* / monotonic_clock_* / list
  # helpers). These import wasi:sockets/io/clocks directly; wit-component surfaces
  # those imports on the final component (host grants them via inherit_network()).
  WASIP2_LIBC="$WASI_SDK_PREFIX/share/wasi-sysroot/lib/wasm32-wasip2/libc.a"
  if [[ -f "$WASIP2_LIBC" ]]; then
    SOCKDIR="$TMPDIR/wasip2-sockets"; mkdir -p "$SOCKDIR"
    # socket surface + the generated component bindings (wasip2.c.obj) + their
    # transitive deps: descriptor_table, wasip2 stdio-over-streams (wasip2_stdio,
    # file_utils), and wasip2_component_type.o (defines the force-link marker that
    # wasip2.c.obj references; carries the wasi import type info wit-component
    # reads when componentizing).
    SOCK_OBJS="socket.c.obj connect.c.obj bind.c.obj listen.c.obj accept.c.obj \
      getsockpeername.c.obj sockopt.c.obj netdb.c.obj recv.c.obj send.c.obj \
      recvfrom.c.obj sendto.c.obj recvmsg.c.obj sendmsg.c.obj shutdown.c.obj \
      socketpair.c.obj sockets_utils.c.obj tcp.c.obj udp.c.obj poll.c.obj \
      descriptor_table.c.obj wasip2.c.obj wasip2_stdio.c.obj file_utils.c.obj \
      wasip2_component_type.o"
    avail=""
    for o in $SOCK_OBJS; do
      "$WASI_SDK_PREFIX/bin/llvm-ar" t "$WASIP2_LIBC" 2>/dev/null | grep -qx "$o" && avail="$avail $o"
    done
    if [[ -n "$avail" ]]; then
      ( cd "$SOCKDIR" && "$WASI_SDK_PREFIX/bin/llvm-ar" x "$WASIP2_LIBC" $avail )
      # extracted members end in .obj AND .o (wasip2_component_type.o) -> glob both
      "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libwasip2sockets.a" "$SOCKDIR"/*
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libwasip2sockets.a"
      echo "Merging wasip2 socket+binding objects ($(echo $avail | wc -w | tr -d ' ') objs) into libduckdb-wasi.a" >&2
    fi
  fi

  # openssl-wasm seeds its RNG with getpid(); wasi libc has no getpid. Provide a
  # fixed-value stub (getpid is only mixed into entropy, not a security source on
  # a single-process wasm sandbox). Compiled for the same target as the archive.
  # openssl's bio_addr.c also references gai_strerror (no wasi libc impl) -- needed
  # by azure (it pulls more of openssl's BIO/socket code than httpfs). pg/mysql
  # already provide gai_strerror via pg_stubs.c, so only add it here otherwise (to
  # avoid a duplicate definition in the final link).
  printf 'int getpid(void){return 42;}\n' > "$TMPDIR/wasi_getpid.c"
  if ! ext_selected postgres_scanner && ! ext_selected mysql_scanner; then
    printf 'const char *gai_strerror(int e){(void)e;return "getaddrinfo error";}\n' \
      >> "$TMPDIR/wasi_getpid.c"
  fi
  "$WASI_SDK_PREFIX/bin/clang" --target="${WASI_TARGET_TRIPLE:-wasm32-wasip2}" \
    -O2 -c "$TMPDIR/wasi_getpid.c" -o "$TMPDIR/wasi_getpid.o"
  "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libwasigetpid.a" "$TMPDIR/wasi_getpid.o"
  ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libwasigetpid.a"
  echo "Merging getpid stub into libduckdb-wasi.a" >&2
fi

# UI bridge fallback. The core component unconditionally references the ui
# extension's duckdb_ui_handle_request/duckdb_ui_free (the host bridges /ddb/*
# through them). When `ui` is NOT embedded those symbols are absent, so provide
# no-op stubs (handle-ui-request then returns none) so the core still links.
# When `ui` IS embedded its real symbols (http_server.cpp) are the only ones --
# the stub is omitted to avoid a duplicate definition.
if ! ext_selected ui; then
  cat > "$TMPDIR/ui_bridge_stub.c" <<'EOF'
#include <stddef.h>
#include <stdint.h>
uint8_t *duckdb_ui_handle_request(
    const char *method, const char *path, const char *headers,
    const uint8_t *body, size_t body_len, size_t *out_len) {
  (void)method; (void)path; (void)headers; (void)body; (void)body_len;
  if (out_len) { *out_len = 0; }
  return NULL;
}
void duckdb_ui_free(uint8_t *ptr) { (void)ptr; }
EOF
  "$WASI_SDK_PREFIX/bin/clang" --target="${WASI_TARGET_TRIPLE:-wasm32-wasip2}" \
    -O2 -c "$TMPDIR/ui_bridge_stub.c" -o "$TMPDIR/ui_bridge_stub.o"
  "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libuibridgestub.a" "$TMPDIR/ui_bridge_stub.o"
  ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libuibridgestub.a"
  echo "Merging UI bridge no-op stub into libduckdb-wasi.a (ui not embedded)" >&2
fi

# avro extension links libavro + libjansson (deflate codec uses zlib, already
# merged with httpfs). iceberg links libroaring. Merge the wasi deps built by
# scripts/build-wasi-deps.sh so the core resolves their symbols.
WASI_DEPS="${WASI_DEPS:-$(pwd)/build/wasi-deps}"
if ext_selected avro; then
  deps=("$WASI_DEPS/avro-c/lib/libavro.a" "$WASI_DEPS/jansson/lib/libjansson.a" \
        "$WASI_DEPS/snappy/lib/libsnappy.a" "$WASI_DEPS/lzma/lib/liblzma.a")
  # zlib (deflate codec) -- only if httpfs didn't already merge it
  ext_selected httpfs \
    || deps+=("$HOME/git/curl-wasm/build/zlib/lib/libz.a")
  for src in "${deps[@]}"; do
    name="$(basename "$src")"
    if [[ -f "$src" ]]; then
      cp "$src" "$TMPDIR/$name"
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB $name"
      echo "Merging avro/iceberg dep ($src) into libduckdb-wasi.a" >&2
    fi
  done
fi

# CRoaring: ducklake (1.5.4 deletion vectors) + iceberg (manifests) both link it.
# Merge libroaring.a once if either is selected.
if ext_selected ducklake || ext_selected iceberg; then
  RB="$WASI_DEPS/roaring/lib/libroaring.a"
  if [[ -f "$RB" ]]; then
    cp "$RB" "$TMPDIR/libroaring.a"
    ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libroaring.a"
    echo "Merging roaring (ducklake/iceberg) into libduckdb-wasi.a" >&2
  fi
fi

# delta: merge the prebuilt delta-kernel-rs FFI staticlib (sync engine). CMake
# links it to the extension target via target_link_libraries, but a static lib
# doesn't bundle its link deps, so the kernel symbols must be merged into the
# combined archive the core links against (same as avro/iceberg/spatial).
if ext_selected delta; then
  KERNEL_A="$(pwd)/build/delta-kernel/out/libdelta_kernel_ffi.a"
  if [[ -f "$KERNEL_A" ]]; then
    cp "$KERNEL_A" "$TMPDIR/libdelta_kernel_ffi.a"
    ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libdelta_kernel_ffi.a"
    echo "Merging delta kernel ($KERNEL_A) into libduckdb-wasi.a" >&2
  else
    echo "WARNING: delta enabled but kernel missing: $KERNEL_A" >&2
  fi
fi

# azure: merge the prebuilt Azure SDK for C++ (azure-core + storage-common/blobs/
# datalake + identity) and its libxml2 dep. The extension links these via the
# Azure:: targets on native, but on wasm they're separate static libs that must
# be in the combined archive the core links against. curl-wasm + openssl-wasm
# (the transport + crypto) are already merged for httpfs.
if ext_selected azure; then
  AZURE_LIB="$(pwd)/build/azure-sdk/out/lib"
  azure_deps=("$AZURE_LIB/libazure-storage-files-datalake.a" "$AZURE_LIB/libazure-storage-blobs.a"
              "$AZURE_LIB/libazure-storage-common.a" "$AZURE_LIB/libazure-identity.a"
              "$AZURE_LIB/libazure-core.a"
              "${LIBXML2_WASM:-$HOME/git/libxml2-wasm/build/install}/lib/libxml2.a")
  for src in "${azure_deps[@]}"; do
    name="$(basename "$src")"
    if [[ -f "$src" ]]; then
      cp "$src" "$TMPDIR/$name"
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB $name"
      echo "Merging azure dep ($src) into libduckdb-wasi.a" >&2
    else
      echo "WARNING: azure enabled but dep missing: $src" >&2
    fi
  done
fi

# spatial: merge the geo stack (GEOS + PROJ + GDAL + tiff/jpeg/png/expat/sqlite +
# proj data) from the ~/git/*-wasm libs, plus a stubs lib for the ~24 wasi-missing
# symbols GDAL references (dlopen/fork/exec/sqlite-extras). zlib comes from httpfs.
if ext_selected spatial; then
  # compile the weak stubs
  "$WASI_SDK_PREFIX/bin/clang" --target="${WASI_TARGET_TRIPLE:-wasm32-wasip2}" -O2 \
    -c "$(pwd)/cmake/spatial-deps/stubs.c" -o "$TMPDIR/spatial_stubs.o"
  "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libspatialstubs.a" "$TMPDIR/spatial_stubs.o"
  ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libspatialstubs.a"
  # libjpeg uses setjmp/longjmp -> the wasm-sjlj runtime (__wasm_setjmp/longjmp)
  SJLJ="$WASI_SDK_PREFIX/share/wasi-sysroot/lib/${WASI_TARGET_TRIPLE:-wasm32-wasip2}/libsetjmp.a"
  if [[ -f "$SJLJ" ]]; then
    cp "$SJLJ" "$TMPDIR/libsetjmp.a"; ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libsetjmp.a"
    echo "Merging libsetjmp (wasm sjlj runtime) into libduckdb-wasi.a" >&2
  fi
  # PROJ: use the build_real_sqlite variant (real sqlite + memvfs-embedded
  # proj.db, no runtime files). libproj.a references pj_get_embedded_proj_db
  # which lives in a separate proj_resources object -> bundle it as a lib.
  PROJ_RS="$HOME/git/proj-wasm/build_real_sqlite/deps/proj"
  PROJ_RES_OBJ="$PROJ_RS/src/CMakeFiles/proj_resources.dir/embedded_resources.c.obj"
  if [[ -f "$PROJ_RES_OBJ" ]]; then
    "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libprojresources.a" "$PROJ_RES_OBJ"
    ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libprojresources.a"
  fi
  # sqlite3: compile proj-wasm's 3.45.0 amalgamation with SQLITE_USE_URI=1 (the
  # spatial extension's proj_module opens the embedded proj.db via a memvfs
  # `file:?ptr=` URI without passing SQLITE_OPEN_URI, so URI parsing must be
  # compiled in). Matches proj-wasm's wasi flags otherwise. This .obj wins the
  # final merge (added last) over sqlite_scanner's 3.38.1, so all sqlite3
  # callers (proj, memvfs, gdal, sqlite_scanner) share one URI-enabled 3.45.0.
  SQLITE_SRC="$HOME/git/proj-wasm/deps/sqlite/sqlite3.c"
  if [[ -f "$SQLITE_SRC" ]]; then
    "$WASI_SDK_PREFIX/bin/clang" --target="${WASI_TARGET_TRIPLE:-wasm32-wasip2}" -O2 \
      -DSQLITE_USE_URI=1 -DSQLITE_OMIT_WAL=1 -DSQLITE_OMIT_LOAD_EXTENSION=1 \
      -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_SHARED_CACHE=1 -DSQLITE_DEFAULT_MEMSTATUS=0 \
      -DSQLITE_LIKE_DOESNT_MATCH_BLOBS=1 -DSQLITE_OMIT_DEPRECATED=1 -DSQLITE_USE_ALLOCA=1 \
      -DSQLITE_OMIT_AUTOINIT=1 -DSQLITE_OMIT_POSIX_ADVISORY_LOCKING=1 \
      -c "$SQLITE_SRC" -o "$TMPDIR/sqlite3.c.obj"
    "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libsqlite3uri.a" "$TMPDIR/sqlite3.c.obj"
    ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libsqlite3uri.a"
    echo "Merging URI-enabled sqlite3 (3.45.0) into libduckdb-wasi.a" >&2
  fi
  geo=("$HOME/git/gdal-wasm/build/deps/gdal/libgdal.a"
       "$HOME/git/geos-wasm/lib/lib/libgeos_c.a" "$HOME/git/geos-wasm/lib/lib/libgeos.a"
       "$PROJ_RS/lib/libproj.a"
       "$HOME/git/libtiff-wasm/build/lib/libtiff.a"
       "$HOME/git/libjpeg-turbo-wasm/build/libjpeg-turbo/libjpeg.a"
       "$HOME/git/libpng-wasm/build-wasip1/lib/libpng16.a"
       "$HOME/git/expat-wasm/build/lib/libexpat.a")
  for src in "${geo[@]}"; do
    name="$(basename "$src")"
    if [[ -f "$src" ]]; then
      cp "$src" "$TMPDIR/$name"
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB $name"
      echo "Merging spatial geo dep ($src) into libduckdb-wasi.a" >&2
    else
      echo "WARNING: spatial geo dep missing: $src" >&2
    fi
  done
fi

# excel: xlsx = zip(expat-parsed XML). Merge minizip-ng + expat + zlib (the
# latter two only if not already merged by spatial/httpfs/avro).
if ext_selected excel; then
  WASI_DEPS="${WASI_DEPS:-$(pwd)/build/wasi-deps}"
  xdeps=("$WASI_DEPS/minizip/lib/libminizip-ng.a")
  ext_selected spatial \
    || xdeps+=("$HOME/git/expat-wasm/build/lib/libexpat.a")
  { ext_selected spatial || ext_selected httpfs || ext_selected avro; } \
    || xdeps+=("$HOME/git/curl-wasm/build/zlib/lib/libz.a")
  for src in "${xdeps[@]}"; do
    name="$(basename "$src")"
    if [[ -f "$src" ]]; then
      cp "$src" "$TMPDIR/$name"
      ADDLIBS="$ADDLIBS"$'\n'"ADDLIB $name"
      echo "Merging excel dep ($src) into libduckdb-wasi.a" >&2
    else
      echo "WARNING: excel dep missing: $src" >&2
    fi
  done
fi

# postgres_scanner / mysql_scanner: the pg-wasi posix stubs (no-op signal API +
# getpwuid/getuid/popen + gai_strerror) + the getaddrinfo wrapper (numeric IPs
# resolve locally; wasi's getaddrinfo rejects them via resolve-addresses).
# Sockets + openssl come from httpfs's wasip2 graft, so both DB scanners require
# httpfs. postgres compiles libpq inline; mysql merges a prebuilt libmariadb.
if ext_selected postgres_scanner || ext_selected mysql_scanner; then
  "$WASI_SDK_PREFIX/bin/clang" --target="${WASI_TARGET_TRIPLE:-wasm32-wasip2}" -O2 \
    -c "$(pwd)/cmake/postgres-wasi/stubs.c" -o "$TMPDIR/pg_stubs.o" \
    -I"$(pwd)/cmake/postgres-wasi/include"
  "$WASI_SDK_PREFIX/bin/clang" --target="${WASI_TARGET_TRIPLE:-wasm32-wasip2}" -O2 \
    -c "$(pwd)/cmake/postgres-wasi/getaddrinfo_wrap.c" -o "$TMPDIR/pg_gaiwrap.o"
  "$WASI_SDK_PREFIX/bin/llvm-ar" rcs "$TMPDIR/libpgstubs.a" "$TMPDIR/pg_stubs.o" "$TMPDIR/pg_gaiwrap.o"
  ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libpgstubs.a"
  echo "Merging pg-wasi stubs + getaddrinfo wrapper into libduckdb-wasi.a" >&2
fi
# mysql_scanner: merge the prebuilt MariaDB Connector/C (libpq is inline; this
# is the equivalent for mysql).
if ext_selected mysql_scanner; then
  WASI_DEPS="${WASI_DEPS:-$(pwd)/build/wasi-deps}"
  if [[ -f "$WASI_DEPS/mariadb/lib/mariadb/libmariadbclient.a" ]]; then
    cp "$WASI_DEPS/mariadb/lib/mariadb/libmariadbclient.a" "$TMPDIR/libmariadbclient.a"
    ADDLIBS="$ADDLIBS"$'\n'"ADDLIB libmariadbclient.a"
    echo "Merging libmariadbclient into libduckdb-wasi.a" >&2
  else
    echo "WARNING: mysql dep missing: $WASI_DEPS/mariadb/lib/mariadb/libmariadbclient.a" >&2
  fi
fi
pushd "$TMPDIR" >/dev/null
printf 'CREATE libduckdb_combined.a\n%s\nSAVE\nEND\n' "$ADDLIBS" | "$WASI_SDK_PREFIX/bin/llvm-ar" -M
popd >/dev/null

cp "$TMPDIR/libduckdb_combined.a" "$ARTIFACTS_DIR/libduckdb-wasi.a"

echo "Static library copied to $ARTIFACTS_DIR/libduckdb-wasi.a" >&2

# Self-record the core's embedded set for ducklink's embedding-tracking
# ("Bundles") feature. ducklink's tooling/builds.py ingests this manifest:
#   python3 tooling/builds.py record <name> --kind core \
#       --from-manifest registry/last-core-build.json
# Minimal + non-breaking: only runs when $DUCKLINK points at the ducklink repo.
if [ -n "${DUCKLINK:-}" ] && [ -d "${DUCKLINK}/registry" ]; then
  CORE_ARTIFACT="$ARTIFACTS_DIR/libduckdb-wasi.a"
  CORE_HASH="$(
    { command -v b2sum >/dev/null 2>&1 && b2sum -l 256 "$CORE_ARTIFACT" 2>/dev/null; } \
      || { command -v shasum >/dev/null 2>&1 && shasum -a 256 "$CORE_ARTIFACT" 2>/dev/null; } \
      || { command -v sha256sum >/dev/null 2>&1 && sha256sum "$CORE_ARTIFACT" 2>/dev/null; } \
      || echo "unknown -"
  )"
  CORE_HASH="${CORE_HASH%% *}"
  BUILT_AT="$(date +%s)"
  EMBED_JSON="$(
    printf '%s' "${EMBED_EXTENSIONS:-}" | awk -v RS=',' '
      { gsub(/^[ \t]+|[ \t]+$/, "", $0); if ($0 != "") printf "%s\"%s\"", (n++ ? "," : ""), $0 }'
  )"
  cat > "${DUCKLINK}/registry/last-core-build.json" <<EOF
{
  "name": "core",
  "kind": "core",
  "embedded_extensions": [${EMBED_JSON}],
  "core_artifact": "artifacts/libduckdb-wasi.a",
  "core_hash": "${CORE_HASH}",
  "built_at": ${BUILT_AT}
}
EOF
  echo "Recorded core embed-manifest -> ${DUCKLINK}/registry/last-core-build.json" >&2
fi
