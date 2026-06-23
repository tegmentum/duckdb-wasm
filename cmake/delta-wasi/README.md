# Delta extension on wasm — WORKING (local read)

DuckDB's `delta` extension reads **local** Delta Lake tables on wasm32-wasip2:
`SELECT * FROM delta_scan('<local path>')` runs end-to-end through the wasm core
(verified by `delta_scan_embedded_local_table` in `ducklink-host`, which reads the
`simple_table` fixture — 10 rows — and matches native DuckDB). It wraps
delta-kernel-rs **v0.21.0**'s SYNC engine (local `std::fs`; no
tokio/reqwest/object_store).

DuckDB 1.5.4's canonical delta is the **out-of-tree** `duckdb/duckdb-delta @
45c40878` (its `.github/config/extensions/delta.cmake` pins it; it targets 1.5.4's
MultiFileReader API and the v0.21.0 kernel). We vendor it to `build/duckdb-delta`.
NOTE: the minimal `external/duckdb/extension/delta` shipped in the v1.5.4 tree is a
**stale pre-1.5.4 skeleton** that does not compile against 1.5.4's MultiFileReader
API — do not use it. DuckDB upstream also EXCLUDES delta on wasm (`NOT
${WASM_ENABLED}`), so this sync-engine path is novel.

## How it's wired

- **`scripts/build-delta-kernel-wasm.sh`** — reproducible kernel build: clones
  delta-kernel-rs `v0.21.0` (delta-io repo), applies
  `kernel-v0.21.0-sync-engine.patch`, builds `libdelta_kernel_ffi.a` for
  wasm32-wasip2 (features `sync-engine,tracing,test-ffi`) with the wasi-sdk C
  toolchain, stages it under `build/delta-kernel/out/`.
- **`scripts/build-libduckdb-wasm.sh`** (`stage_delta_kernel`) — vendors
  duckdb-delta @ 45c40878, patches its CMakeLists + C++ for wasm (below), stages
  the kernel `.a` where the CMakeLists expects it, and merges the kernel into
  `libduckdb-wasi.a`. It also reorders `LinkedExtensions()` in the generated
  loader so `core_functions`+`parquet` load before `delta` (delta's `Load()`
  grabs `parquet_scan`, and autoload is disabled in wasm).
- **`crates/ducklink-core/build.rs`** — adds `--allow-multiple-definition` to the
  core link when the lib contains the kernel (the kernel is a Rust `staticlib`
  bundling its own std runtime, which collides with the core's std; same
  toolchain, so the linker keeps the first copy).
- **`cmake/wasm-extension-config.cmake`** — `embed_ext(delta SOURCE_DIR
  build/duckdb-delta)`, guarded on the staged kernel + vendored source.

## The kernel patch (`kernel-v0.21.0-sync-engine.patch`)

v0.21.0 demoted the sync engine to a test-only, crate-private module and has NO
FFI sync-engine feature, so the patch:
- adds a kernel `sync-engine` feature (arrow + parquet WITHOUT the zstd/brotli C
  codecs that collide with DuckDB's zstd + curl-wasm's libbrotli, and WITHOUT
  flate2 — its backend isn't selectable here; sync uses snappy, the Delta
  default — plus object_store WITHOUT cloud backends),
- re-exposes the sync module + makes `SyncEngine`/`new()` public,
- un-gates the arrow/parquet `Error` variants (`need-arrow`) + the `engine_data`
  FFI (`get_engine_data` etc.) and the `ExternEngineVtable`/`engine_to_handle`
  plumbing (`any(default-engine-base, sync-engine)`) so a sync engine can be
  wrapped. NB the FFI crate has no `need-arrow` feature (only the kernel does), so
  FFI-crate gates must use `sync-engine`,
- adds the `get_sync_engine` FFI constructor,
- in `utils.rs` `try_parse_uri`: on wasm, ROOT the (preopen-relative) table path
  instead of `std::fs::canonicalize` (which doesn't absolutize on WASI, so
  `Url::from_directory_path` would reject it). The host fs shim strips the leading
  `/` and resolves against its preopen.

## The delta C++ patches (in `stage_delta_kernel`, marker-guarded)

- **delta_multi_file_list.cpp** — under `__wasi__`, build the engine via
  `get_sync_engine` instead of `CreateBuilder`+`builder_build`;
  `#ifndef __wasi__`-guard `CreateBuilder` (the cloud option-setting fn, which
  references the default-engine builder FFI absent in a sync-only kernel).
- **delta_transaction.cpp** — `#ifndef __wasi__`-guard the Unity Catalog commit
  branch (`get_uc_commit_client`/`get_uc_committer` live in the kernel's
  `delta-kernel-unity-catalog` feature, which pulls tokio — no wasm build; UC over
  the network is unusable on wasm anyway). Always take the plain local transaction.
- **CMakeLists.txt** — force `RUST_PLATFORM_TARGET=wasm32-wasip2` (its OS detect
  FATAL_ERRORs on wasm), no-op the kernel ExternalProject (prebuilt + staged) while
  KEEPING its cbindgen header-gen step, and `add_compile_options(-Wno-c++11-narrowing
  -Wno-narrowing)` (KernelStringSlice.len is `uintptr_t`=32-bit on wasm32 vs
  `idx_t`=64-bit; the braced-init narrowing is a hard error only on wasm32 and is
  harmless — slice sizes never approach 4 GiB).

## A wasi-fs shim fix (not delta-specific)

Delta is the first extension to list a non-empty directory through the core's
`__wrap_readdir`. That shim sized the name buffer from `libc::dirent.d_name`,
which is a zero-length flexible-array member on wasm (`cap=0`) -> every entry hit
`ENAMETOOLONG`. Fixed in `core/src/lib.rs` by backing the dirent with an
over-sized buffer and writing the name at the `d_name` offset.

## Scope / costs

- **Local Delta only** (sync engine = `std::fs`). Remote (`s3://`, `az://`, UC
  catalog) needs routing delta-kernel's I/O through DuckDB's FileSystem/httpfs (the
  kernel uses its own object_store, whose reqwest/tokio has no wasip2 transport).
- **~60 MB** added (`libdelta_kernel_ffi.a`): the kernel bundles its own Rust
  arrow/parquet, duplicating DuckDB's C++ ones.
- Snappy-compressed Delta read; **zstd/brotli-compressed Delta won't** (codecs
  dropped). Delta defaults to snappy, so this is rarely a constraint.
