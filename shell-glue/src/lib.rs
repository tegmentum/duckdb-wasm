//! Route A glue: the C entry the prebuilt libduckdb-wasi.a calls on `LOAD`
//! (`duckdb_component_load_extension`) wired, via wit-bindgen, to the host's
//! componentized-extension surface. Linked into the real DuckDB shell command by
//! the clang driver in scripts/build-shell-ext-wasm.sh.
//!
//! PROVEN (Step 0): this staticlib's wit-bindgen component-type metadata makes
//! wasm-component-ld declare `duckdb:component/host-extension-loader` as a
//! component import alongside the shell's auto `wasi:cli/run` export -- one
//! component = real shell + engine + extension-install glue.
//!
//! REMAINING for the live `LOAD aba; SELECT aba_validate(...)` round-trip:
//!   1. Expand the world to also import `extension-loader-hooks`
//!      (get-pending-registrations) + `duckdb:extension/callback-dispatch@2.0.0`
//!      (call-scalar-batch) + types/runtime, and port the scalar-registration
//!      trampoline from ../core/src/lib.rs (register_pending_scalar + the per-row
//!      DuckDB C-API scalar callback that marshals Duckvalues and calls
//!      call-scalar-batch).
//!   2. A C++->Rust db-handle bridge: the archive calls
//!      `duckdb_component_load_extension(name)` from
//!      `LoadExternalExtensionInternal(DatabaseInstance &db, ...)` WITHOUT
//!      passing `db`, and the core's registration glue only knows connections the
//!      *Rust* `database.open` created. The shell owns its connection in C++ (via
//!      the sqlite3 shim -> C-API), so the glue must capture that
//!      `duckdb_database`/`duckdb_connection` (hook the shell's open path) to
//!      register the function onto the shell's own engine.

#![allow(clippy::missing_safety_doc)]

use std::ffi::CStr;
use std::os::raw::c_char;

wit_bindgen::generate!({
    path: "wit",
    world: "shell-glue",
});

#[no_mangle]
pub unsafe extern "C" fn duckdb_component_load_extension(name: *const c_char) -> bool {
    if name.is_null() {
        return false;
    }
    let extension_name = CStr::from_ptr(name).to_string_lossy().into_owned();
    // Ask the host to resolve + load the componentized extension.
    crate::duckdb::component::host_extension_loader::request_load(&extension_name)
}
