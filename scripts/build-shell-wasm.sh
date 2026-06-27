#!/usr/bin/env bash
# Build the REAL DuckDB shell (external/duckdb/tools/shell) for wasm32-wasip2 and
# package it as a wasi:cli/run component (duckdb-shell.wasm).
#
# This compiles the actual upstream shell sources (shell.cpp + the renderers +
# metadata/prompt/highlight) and links them against the prebuilt libduckdb-wasi.a
# (the same core archive the component build consumes) plus the DuckDB third_party
# static libs. The result is a standalone DuckDB shell -- byte-for-byte the native
# CLI behaviour (duckbox output, dot commands, -csv/-json/-readonly, ...) -- that
# runs under `wasmtime run`.
#
# Notes on the wasi port (no source patches to the shell):
#   * Built WITHOUT -DHAVE_LINENOISE: linenoise needs termios raw mode, which
#     wasip2 has no interface for. The shell's built-in fallback line reader
#     (local_getline + printed prompts, all guarded by #ifdef HAVE_LINENOISE)
#     gives a fully functional REPL -- just no arrow-key history/editing.
#   * pwd.h / system()/popen()/pclose() gaps in wasi-libc are bridged by
#     cmake/shell-wasi (a shell-only include path + force-include + stub object);
#     none of it touches the core/library build.
#   * Host-bridge hooks the prebuilt archive expects from the component runtime
#     (tvm_spill_* spill bridge, duckdb_component_load_extension) are stubbed to
#     "unavailable" -- spill falls back to temp files, LOAD behaves as a normal
#     DuckDB build. Wiring those is Phase 2.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"

# Resolve build inputs (same defaults as scripts/setup-env.sh).
if [[ -z "${WASI_SDK_PREFIX:-}" || -z "${DUCKDB_SOURCE_DIR:-}" || -z "${DUCKDB_STATIC_LIB:-}" ]]; then
  # shellcheck disable=SC1091
  source "$HERE/scripts/setup-env.sh" >/dev/null
fi
DUCKDB_BUILD_DIR="${DUCKDB_BUILD_DIR:-$HERE/build/duckdb-wasi}"
WASI_TARGET_TRIPLE="${WASI_TARGET_TRIPLE:-wasm32-wasip2}"

for v in WASI_SDK_PREFIX DUCKDB_SOURCE_DIR DUCKDB_STATIC_LIB DUCKDB_BUILD_DIR; do
  printf '  %-18s %s\n' "$v" "${!v}" >&2
  [[ -e "${!v}" ]] || { echo "MISSING: $v=${!v}" >&2; exit 1; }
done

CXX="$WASI_SDK_PREFIX/bin/clang++"
CC="$WASI_SDK_PREFIX/bin/clang"
SYSROOT="$WASI_SDK_PREFIX/share/wasi-sysroot"
SHELL_SRC="$DUCKDB_SOURCE_DIR/tools/shell"

OUT_DIR="${OUT_DIR:-$HERE/build/duckdb-shell}"
mkdir -p "$OUT_DIR/obj"

# Compile flags: mirror cmake/toolchains/wasi-sdk.cmake (exception model, wasi
# defines, the override include + force-include shim) and add the shell needs.
CXXFLAGS=(
  --target="$WASI_TARGET_TRIPLE" --sysroot="$SYSROOT"
  -O2 -std=c++17 -stdlib=libc++
  -fwasm-exceptions -mllvm -wasm-use-legacy-eh=false
  -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS
  -DDISABLE_DUCKDB_REMOTE_INSTALL -DDUCKDB_DISABLE_EXTENSION_LOAD
  -DDUCKDB_NO_THREADS -DDUCKDB_SKIP_HTTP
  -DUSE_DUCKDB_SHELL_WRAPPER
  # shell-only wasi shims (pwd.h on the include path; system() decl force-included)
  -I"$HERE/cmake/shell-wasi/include"
  -include "$HERE/cmake/shell-wasi/shell-wasi-shim.h"
  -I"$HERE/cmake/wasi-override/include"
  -include "$HERE/cmake/toolchains/wasi-shim.hpp"
  # shell + duckdb headers
  -I"$SHELL_SRC/include"
  -I"$DUCKDB_SOURCE_DIR/src/include"
  -I"$DUCKDB_SOURCE_DIR/third_party/utf8proc/include"
)

# The upstream shell translation units (CMakeLists SHELL_SOURCES, minus the
# Windows-only TU). No HAVE_LINENOISE, no SHELL_INLINE_AUTOCOMPLETE.
SOURCES=(
  shell_command_line_option.cpp
  shell_extension.cpp
  shell.cpp
  shell_helpers.cpp
  shell_metadata_command.cpp
  shell_prompt.cpp
  shell_renderer.cpp
  shell_highlight.cpp
  shell_progress_bar.cpp
  shell_render_table_metadata.cpp
)

echo "Compiling shell translation units for $WASI_TARGET_TRIPLE" >&2
OBJS=()
for src in "${SOURCES[@]}"; do
  obj="$OUT_DIR/obj/${src%.cpp}.o"
  echo "  CXX $src" >&2
  "$CXX" "${CXXFLAGS[@]}" -c "$SHELL_SRC/$src" -o "$obj"
  OBJS+=("$obj")
done

# wasi shim stubs (popen/system + host-bridge hooks).
echo "  CC  shell_wasi_stubs.c" >&2
"$CC" --target="$WASI_TARGET_TRIPLE" --sysroot="$SYSROOT" -O2 \
  -c "$HERE/cmake/shell-wasi/shell_wasi_stubs.c" -o "$OUT_DIR/obj/shell_wasi_stubs.o"
OBJS+=("$OUT_DIR/obj/shell_wasi_stubs.o")

# Link. The prebuilt libduckdb-wasi.a already merges the core, libc++/libc++abi/
# libunwind, the in-tree extension archives and (for this build) the httpfs socket
# + curl/openssl objects. The DuckDB third_party static libs are NOT merged, so
# add them. One big --start-group resolves the cross-references.
THIRD_PARTY=()
for tp in zstd re2 skiplist hyperloglog miniz mbedtls utf8proc fmt fsst yyjson libpg_query fastpforlib; do
  # the lib basename differs from the dir for a couple (skiplist -> skiplistlib)
  for cand in "$DUCKDB_BUILD_DIR/third_party/$tp"/libduckdb_*.a; do
    [[ -f "$cand" ]] && THIRD_PARTY+=("$cand")
  done
done

BASE_LIB="$SYSROOT/lib/$WASI_TARGET_TRIPLE"

# Link + componentize in one step: the wasm32-wasip2 clang driver uses
# wasm-component-ld, which runs wasm-ld and then turns the resulting core module
# into a component. wasm-ld resolves archive members lazily across the whole link
# (no --start-group needed; that's an ELF-ism wasm-ld/wasm-component-ld reject).
# A wasip2 module imports the component-model interfaces directly, so no p1->p2
# adapter is involved; with _start present the component exports wasi:cli/run.
COMPONENT="${COMPONENT_OUT:-$OUT_DIR/duckdb-shell.wasm}"
echo "Linking + componentizing -> $COMPONENT" >&2
"$CXX" \
  --target="$WASI_TARGET_TRIPLE" --sysroot="$SYSROOT" \
  -fwasm-exceptions -mllvm -wasm-use-legacy-eh=false \
  "${OBJS[@]}" \
  "$DUCKDB_STATIC_LIB" \
  "${THIRD_PARTY[@]}" \
  -L"$BASE_LIB" -lm -lwasi-emulated-mman -lwasi-emulated-signal -lwasi-emulated-process-clocks \
  -o "$COMPONENT"

echo "Component: $COMPONENT ($(du -h "$COMPONENT" | cut -f1))" >&2
echo "Exports:" >&2
wasm-tools component wit "$COMPONENT" | grep -E 'world|export|^package' | head -20 >&2
echo "OK" >&2
