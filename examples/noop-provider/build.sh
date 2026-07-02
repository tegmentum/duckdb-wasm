#!/usr/bin/env bash
# Build the noop-provider component. Composes with ducklink_core.wasm to
# satisfy every non-WASI import the core declares — the result is a
# database-only component whose sole remaining imports are WASI.
#
# Mirrors ~/git/openssl-wasm/examples/noop-provider/build.sh style.

set -euo pipefail
cd "$(dirname "$0")"

# Prefer the setup-env.sh at the repo root; fall back to a common wasi-sdk.
if [ -z "${WASI_SDK_PREFIX:-}" ]; then
    # shellcheck disable=SC1091
    source "$(cd ../.. && pwd)/scripts/setup-env.sh" >/dev/null 2>&1 || true
fi
WASI_SDK_PREFIX="${WASI_SDK_PREFIX:-/opt/wasi-sdk}"
CLANG="${CLANG:-$WASI_SDK_PREFIX/bin/clang}"
SYSROOT="${SYSROOT:-$WASI_SDK_PREFIX/share/wasi-sysroot}"

OUT=build
mkdir -p "$OUT"

# Regenerate bindings if WIT changed.
if [ wit/world.wit -nt "$OUT/.bindings-stamp" ] || [ ! -f src/noop.c ]; then
    wit-bindgen c --world noop --out-dir src wit
    touch "$OUT/.bindings-stamp"
fi

CFLAGS=(
    --target=wasm32-wasip2
    --sysroot="$SYSROOT"
    -O2 -fno-strict-aliasing
    -Wall -Wextra -Wno-unused-parameter
    -Isrc
)

echo "  cc noop.c"
"$CLANG" "${CFLAGS[@]}" -c src/noop.c      -o "$OUT/noop.o"
echo "  cc noop_impl.c"
"$CLANG" "${CFLAGS[@]}" -c src/noop_impl.c -o "$OUT/noop_impl.o"

echo "  link → $OUT/noop-provider.wasm"
"$CLANG" \
    --target=wasm32-wasip2 --sysroot="$SYSROOT" \
    -mexec-model=reactor -Wl,--no-entry -Wl,--export-dynamic \
    "$OUT/noop.o" "$OUT/noop_impl.o" src/noop_component_type.o \
    -o "$OUT/noop-provider.wasm"

wasm-tools validate "$OUT/noop-provider.wasm"
ls -lh "$OUT/noop-provider.wasm"
echo
echo "Now compose with ducklink_core.wasm:"
echo "  wac plug ../../target/wasm32-wasip2/release/ducklink_core.wasm \\"
echo "           --plug $OUT/noop-provider.wasm \\"
echo "           -o $OUT/ducklink-with-noop.wasm"
