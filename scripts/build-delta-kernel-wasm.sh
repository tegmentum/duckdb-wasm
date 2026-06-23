#!/usr/bin/env bash
# Build delta-kernel-rs's FFI staticlib for wasm32-wasip2 (the SYNC engine only:
# local std::fs, no tokio/reqwest/object_store). Produces a libdelta_kernel_ffi.a
# that links into libduckdb-wasi.a for DuckDB 1.5.4's canonical `delta` extension
# (duckdb-delta @ 45c40878, from .github/config/extensions/delta.cmake), which pins
# kernel v0.21.0 (delta-io/delta-kernel-rs).
#
# Like v0.14.0, v0.21.0 demotes the sync engine to a private test-only module and
# has no FFI sync-engine feature, so cmake/delta-wasi/kernel-v0.21.0-sync-engine.patch
# (re-anchored from the v0.14.0 patch) does the work:
#   - adds a kernel `sync-engine` feature (arrow+parquet, no cloud object_store),
#   - re-exposes the sync module + makes SyncEngine/new() public,
#   - un-gates the arrow/parquet error variants (need-arrow) + the FFI
#     ExternEngineVtable/engine_to_handle so a sync engine can be wrapped,
#   - adds the `get_sync_engine` FFI constructor,
#   - drops parquet's zstd/brotli/flate2 codecs (their bundled C symbols collide
#     with DuckDB's zstd + curl-wasm's libbrotli; sync uses snappy, the Delta
#     default) and object_store's cloud backends (aws/azure/gcp/http pull reqwest).
#
# Output: $OUT_DIR/libdelta_kernel_ffi.a + $OUT_DIR/ffi-headers/.
# Env: WASI_SDK_PREFIX (required), KERNEL_SRC (work dir), OUT_DIR.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WASI_SDK_PREFIX="${WASI_SDK_PREFIX:?set WASI_SDK_PREFIX to the wasi-sdk root}"
KERNEL_TAG="v0.21.0"   # = duckdb-delta @ 45c40878's kernel pin
KERNEL_SRC="${KERNEL_SRC:-$ROOT/build/delta-kernel/src}"
OUT_DIR="${OUT_DIR:-$ROOT/build/delta-kernel/out}"
PATCH="$ROOT/cmake/delta-wasi/kernel-v0.21.0-sync-engine.patch"

if [[ "$(git -C "$KERNEL_SRC" describe --tags 2>/dev/null)" != "$KERNEL_TAG" ]]; then
  echo ">> (re)fetching delta-kernel-rs @ $KERNEL_TAG" >&2
  rm -rf "$KERNEL_SRC"
  git clone --filter=blob:none --no-checkout \
    https://github.com/delta-io/delta-kernel-rs "$KERNEL_SRC"
  git -C "$KERNEL_SRC" fetch --depth 1 origin "refs/tags/$KERNEL_TAG"
  git -C "$KERNEL_SRC" checkout --quiet "$KERNEL_TAG"
fi

echo ">> applying sync-engine FFI patch (idempotent)" >&2
git -C "$KERNEL_SRC" checkout -- . 2>/dev/null || true
git -C "$KERNEL_SRC" apply "$PATCH"

echo ">> building delta_kernel_ffi (sync-engine, release, wasm32-wasip2)" >&2
( cd "$KERNEL_SRC" && env \
    "CC_wasm32_wasip2=$WASI_SDK_PREFIX/bin/clang" \
    "CC_wasm32-wasip2=$WASI_SDK_PREFIX/bin/clang" \
    "AR_wasm32_wasip2=$WASI_SDK_PREFIX/bin/llvm-ar" \
    "AR_wasm32-wasip2=$WASI_SDK_PREFIX/bin/llvm-ar" \
    "CFLAGS_wasm32_wasip2=--sysroot=$WASI_SDK_PREFIX/share/wasi-sysroot" \
    "CFLAGS_wasm32-wasip2=--sysroot=$WASI_SDK_PREFIX/share/wasi-sysroot" \
    cargo build -p delta_kernel_ffi --no-default-features \
      --features sync-engine,tracing,test-ffi \
      --target wasm32-wasip2 --release )

mkdir -p "$OUT_DIR/ffi-headers"
cp "$KERNEL_SRC/target/wasm32-wasip2/release/libdelta_kernel_ffi.a" "$OUT_DIR/"
# the delta extension uses its committed header, but stage the generated ones too
find "$KERNEL_SRC/target" -name 'delta_kernel_ffi.h*' -path '*ffi-headers*' \
  -exec cp {} "$OUT_DIR/ffi-headers/" \; 2>/dev/null || true
echo ">> done: $OUT_DIR/libdelta_kernel_ffi.a" >&2
