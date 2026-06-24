//===----------------------------------------------------------------------===//
// wasm_files_bridge.h
//
// httpfs M2: the C ABI the C++ WasmFileSystem (cpp/wasm_files.cpp) calls to
// forward file opens/reads/closes to a wasm files-backend component. Each fn is
// implemented in Rust (core/src/lib.rs) and routes to the host-provided
// `duckdb:extension/files-host` import, which the host forwards to the
// registered files component's `file-dispatch` export. Mirrors the
// wasm_storage_* bridge.
//
// ABI choices (single-threaded wasm; kept leak-free):
//   * file-open returns true on success, writing the component-side handle +
//     total size via out-params; false on error (message in
//     wasm_file_last_error()).
//   * file-read returns the number of bytes copied into `buf` (<= len), or -1 on
//     error. A short read (fewer than len) at EOF is normal.
//   * file-close is best-effort (no return).
//===----------------------------------------------------------------------===//

#ifndef WASM_FILES_BRIDGE_H
#define WASM_FILES_BRIDGE_H

#include <cstdint>

extern "C" {

// Open (fetch + cache) the resource at `url`. On success returns true and writes
// the component-side handle + total byte size. On failure returns false; the
// message is available via wasm_file_last_error().
bool wasm_file_open(const char *url, uint32_t *out_handle, uint64_t *out_size);

// Copy up to `len` bytes from the cached resource starting at `offset` into
// `buf`. Returns the count copied (may be < len at EOF), or -1 on error.
int64_t wasm_file_read(uint32_t handle, uint64_t offset, uint32_t len, uint8_t *buf);

// Drop the component-side cache entry for `handle`.
void wasm_file_close(uint32_t handle);

// The most recent files-bridge error message (empty C string if none). Owned by
// the core; valid until the next bridge call.
const char *wasm_file_last_error();

} // extern "C"

#endif // WASM_FILES_BRIDGE_H
