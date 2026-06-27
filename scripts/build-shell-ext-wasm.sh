#!/usr/bin/env bash
# Route A: build the REAL DuckDB shell as a wasi:cli/run command component that
# ALSO imports the componentized-extension surface (duckdb:component/*), so a
# `LOAD <name>` inside the shell dispatches shell -> ducklink runtime -> resident
# extension wasm.
#
# This is the extension-aware sibling of scripts/build-shell-wasm.sh. The only
# differences from that proven standalone build:
#   1. shell_wasi_stubs.c is compiled WITHOUT its `duckdb_component_load_extension`
#      stub (renamed away), so the real glue wins.
#   2. The Rust glue staticlib (shell-glue, built with wit-bindgen) is linked in.
#      It defines `duckdb_component_load_extension` and carries the wit-bindgen
#      component-type metadata, which wasm-component-ld folds into the component
#      as the `duckdb:component/host-extension-loader` (+ extension-loader-hooks,
#      callback-dispatch) imports alongside the auto wasi:cli/run export.
#   3. `--export=cabi_realloc` keeps the canonical-ABI realloc the string-passing
#      imports need (otherwise componentization fails: "module does not export a
#      function named cabi_realloc").
#
# STATUS: the link + componentization are PROVEN (Step 0 de-risk). The glue's
# scalar-registration trampoline + the C++->Rust db-handle bridge (so registered
# functions land on the shell's own C-API connection, which the sqlite3 shim
# owns) are the remaining work -- see shell-glue/src/lib.rs.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"

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

OUT_DIR="${OUT_DIR:-$HERE/build/duckdb-shell-ext}"
mkdir -p "$OUT_DIR/obj"

# Build the Rust glue staticlib first (plain cargo, NOT cargo-component: the
# cargo-component command path routes through wasm32-wasip1 + the preview1
# adapter, which is ABI-incompatible with the wasip2 engine archive).
echo "Building shell-glue staticlib" >&2
( cd "$HERE" && cargo build -p duckdb-shell-glue --target "$WASI_TARGET_TRIPLE" --release ) >&2
GLUE="$HERE/target/$WASI_TARGET_TRIPLE/release/libshell_glue.a"
[[ -f "$GLUE" ]] || { echo "MISSING glue staticlib: $GLUE" >&2; exit 1; }

CXXFLAGS=(
  --target="$WASI_TARGET_TRIPLE" --sysroot="$SYSROOT"
  -O2 -std=c++17 -stdlib=libc++
  -fwasm-exceptions -mllvm -wasm-use-legacy-eh=false
  -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS
  -DDISABLE_DUCKDB_REMOTE_INSTALL -DDUCKDB_DISABLE_EXTENSION_LOAD
  -DDUCKDB_NO_THREADS -DDUCKDB_SKIP_HTTP
  -DUSE_DUCKDB_SHELL_WRAPPER
  # Route A: enable the db-handle bridge hook in the (patched) shell.cpp and make
  # its declaration visible everywhere via force-include.
  -DDUCKDB_SHELL_EXT
  -include "$HERE/cmake/shell-wasi/shell_ext_bridge.h"
  -I"$HERE/cmake/shell-wasi/include"
  -include "$HERE/cmake/shell-wasi/shell-wasi-shim.h"
  -I"$HERE/cmake/wasi-override/include"
  -include "$HERE/cmake/toolchains/wasi-shim.hpp"
  -I"$SHELL_SRC/include"
  -I"$DUCKDB_SOURCE_DIR/src/include"
  -I"$DUCKDB_SOURCE_DIR/third_party/utf8proc/include"
)

# All shell TUs except shell.cpp, which is compiled from a patched copy (the
# db-handle bridge hook is injected into ShellState::OpenDB).
SOURCES=(
  shell_command_line_option.cpp
  shell_extension.cpp
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

# shell.cpp: inject the db-handle bridge call right after the shell extension is
# loaded in ShellState::OpenDB (both the normal and fallback paths reach it with
# `db`/`conn` set). The injection is guarded by DUCKDB_SHELL_EXT, so the patched
# copy is still byte-identical behaviour for the standalone build.
PATCHED_SHELL="$OUT_DIR/obj/shell_patched.cpp"
echo "  GEN shell_patched.cpp (db-handle bridge hook)" >&2
perl -pe 's{(LoadStaticExtension<duckdb::ShellExtension>\(\);)}{$1\n#ifdef DUCKDB_SHELL_EXT\n\t\tduckdb_shell_ext_register_db((void*)db.get());\n#endif}' \
  "$SHELL_SRC/shell.cpp" > "$PATCHED_SHELL"
if ! grep -q "duckdb_shell_ext_register_db" "$PATCHED_SHELL"; then
  echo "ERROR: failed to inject db-handle bridge hook into shell.cpp" >&2; exit 1
fi
echo "  CXX shell_patched.cpp" >&2
"$CXX" "${CXXFLAGS[@]}" -c "$PATCHED_SHELL" -o "$OUT_DIR/obj/shell.o"
OBJS+=("$OUT_DIR/obj/shell.o")

# The db-handle bridge TU (creates a sibling C-API connection on the shell db).
echo "  CXX shell_ext_bridge.cpp" >&2
"$CXX" "${CXXFLAGS[@]}" -c "$HERE/cmake/shell-wasi/shell_ext_bridge.cpp" -o "$OUT_DIR/obj/shell_ext_bridge.o"
OBJS+=("$OUT_DIR/obj/shell_ext_bridge.o")

# wasi shim stubs WITHOUT the duckdb_component_load_extension stub (renamed away
# so the Rust glue provides the real symbol).
echo "  CC  shell_wasi_stubs.c (no dcle stub)" >&2
"$CC" --target="$WASI_TARGET_TRIPLE" --sysroot="$SYSROOT" -O2 \
  -Dduckdb_component_load_extension=zz_unused_stub_dcle \
  -c "$HERE/cmake/shell-wasi/shell_wasi_stubs.c" -o "$OUT_DIR/obj/shell_wasi_stubs.o"
OBJS+=("$OUT_DIR/obj/shell_wasi_stubs.o")

THIRD_PARTY=()
for tp in zstd re2 skiplist hyperloglog miniz mbedtls utf8proc fmt fsst yyjson libpg_query fastpforlib; do
  for cand in "$DUCKDB_BUILD_DIR/third_party/$tp"/libduckdb_*.a; do
    [[ -f "$cand" ]] && THIRD_PARTY+=("$cand")
  done
done

BASE_LIB="$SYSROOT/lib/$WASI_TARGET_TRIPLE"
COMPONENT="${COMPONENT_OUT:-$OUT_DIR/duckdb-shell-ext.wasm}"
echo "Linking + componentizing -> $COMPONENT" >&2
"$CXX" \
  --target="$WASI_TARGET_TRIPLE" --sysroot="$SYSROOT" \
  -fwasm-exceptions -mllvm -wasm-use-legacy-eh=false \
  "${OBJS[@]}" \
  "$GLUE" \
  "$DUCKDB_STATIC_LIB" \
  "${THIRD_PARTY[@]}" \
  -L"$BASE_LIB" -lm -lwasi-emulated-mman -lwasi-emulated-signal -lwasi-emulated-process-clocks \
  -Wl,--export=cabi_realloc \
  -o "$COMPONENT"

echo "Component: $COMPONENT ($(du -h "$COMPONENT" | cut -f1))" >&2
echo "World (imports + exports):" >&2
wasm-tools component wit "$COMPONENT" | grep -E 'import|export' | head -40 >&2
echo "OK" >&2
