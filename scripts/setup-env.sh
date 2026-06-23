#!/usr/bin/env bash
# Source this to set the environment for building the duckdb-wasm core component
# for wasm32-wasip2:
#
#   source scripts/setup-env.sh
#   cargo component build -p duckdb-component-core --target wasm32-wasip2 --release
#
# The four inputs the core build needs (see libduckdb-sys/build.rs):
#   WASI_SDK_PREFIX   wasi-sdk 33.0+ (clang + sysroot; exception handling needs >=33)
#   DUCKDB_STATIC_LIB the merged prebuilt libduckdb-wasi.a
#   DUCKDB_SOURCE_DIR DuckDB checkout (for src/include/duckdb.h, the C API header)
#   DUCKDB_BUILD_DIR  the DuckDB wasm CMake build dir (third_party/*/lib*.a + extension libs)
#
# They default to the sibling `ducklink` checkout's prebuilt artifacts; override
# any of them (export before sourcing) for a standalone setup.

HERE="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
DUCKLINK="${DUCKLINK_DIR:-$HERE/../ducklink}"

# In-tree-vendored extensions (delta, ui) reference their sources via the cmake
# config's ${CMAKE_CURRENT_LIST_DIR}/../external/duckdb path = $HERE/external/duckdb.
# The actual DuckDB checkout lives under ducklink; symlink it so those resolve.
if [ ! -e "$HERE/external/duckdb" ] && [ -d "$DUCKLINK/external/duckdb" ]; then
  mkdir -p "$HERE/external"
  ln -s "$DUCKLINK/external/duckdb" "$HERE/external/duckdb"
fi

# Derive the wasi-sdk platform suffix (override WASI_SDK_PREFIX to bypass).
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  WASI_PLAT="arm64-macos" ;;
  Darwin-x86_64) WASI_PLAT="x86_64-macos" ;;
  Linux-aarch64) WASI_PLAT="arm64-linux" ;;
  Linux-x86_64)  WASI_PLAT="x86_64-linux" ;;
  *)             WASI_PLAT="arm64-macos" ;;
esac

# wasi-sdk toolchain + the DuckDB source checkout are shared inputs that still
# live under ducklink/external (override with WASI_SDK_PREFIX / DUCKDB_SOURCE_DIR
# or DUCKLINK_DIR). The static lib + CMake build dir are OUTPUTS of this repo's
# own build-libduckdb-wasm.sh, so they default to this repo's tree.
export WASI_SDK_PREFIX="${WASI_SDK_PREFIX:-$DUCKLINK/external/wasi-sdk-33.0-$WASI_PLAT}"
export DUCKDB_STATIC_LIB="${DUCKDB_STATIC_LIB:-$HERE/artifacts/libduckdb-wasi.a}"
export DUCKDB_SOURCE_DIR="${DUCKDB_SOURCE_DIR:-$DUCKLINK/external/duckdb}"
export DUCKDB_BUILD_DIR="${DUCKDB_BUILD_DIR:-$HERE/build/duckdb-wasi}"

echo "duckdb-wasm build env:"
for v in WASI_SDK_PREFIX DUCKDB_STATIC_LIB DUCKDB_SOURCE_DIR DUCKDB_BUILD_DIR; do
  path="${!v}"
  [ -e "$path" ] && status="ok" || status="MISSING"
  printf '  %-18s %s (%s)\n' "$v" "$path" "$status"
done
