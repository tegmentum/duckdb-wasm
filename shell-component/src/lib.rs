//! Step 0 de-risk spike: the real DuckDB shell as a wasi:cli/run command.
//!
//! Proves the load-bearing uncertainty for Route A -- that a cargo-component
//! command crate can link the upstream shell's C++ objects (which pull in the
//! whole libduckdb engine archive) and export `wasi:cli/run`, with `run()`
//! driving the shell's C `main`. The extension-install glue is layered on in
//! Step 1 once this links + runs.

#[allow(warnings)]
mod bindings;

use std::ffi::CString;
use std::os::raw::{c_char, c_int};

use bindings::exports::wasi::cli::run::Guest;
use bindings::wasi::cli::environment;

struct Component;

extern "C" {
    // The upstream shell entry point (tools/shell/shell.cpp). wasi-libc renames
    // an `int main(int, char**)` to `__main_argc_argv`; in this reactor component
    // there is no crt0 _start calling it, so we invoke it directly.
    fn __main_argc_argv(argc: c_int, argv: *const *const c_char) -> c_int;
}

impl Guest for Component {
    fn run() -> Result<(), ()> {
        // Reconstruct argv from the wasi:cli/environment arguments (the C main
        // expects argv[0] = program name). Keep the CStrings alive for the whole
        // call by holding them in `owned`.
        let mut args = environment::get_arguments();
        if args.is_empty() {
            args.push("duckdb".to_string());
        }
        let owned: Vec<CString> = args
            .into_iter()
            .map(|a| CString::new(a).unwrap_or_else(|_| CString::new("duckdb").unwrap()))
            .collect();
        let mut argv: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        argv.push(std::ptr::null());

        let rc = unsafe { __main_argc_argv(owned.len() as c_int, argv.as_ptr()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(())
        }
    }
}

bindings::export!(Component with_types_in bindings);
