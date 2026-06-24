fn main() {
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_arch != "wasm32" {
        return;
    }

    build_wasm_cpp("wasm_storage.cpp");
    // httpfs M1: the stub FileSystem subsystem (cpp/wasm_files.cpp). Compiled +
    // linked the same way as wasm_storage.cpp.
    build_wasm_cpp("wasm_files.cpp");

    // DuckDB's libpg_query parser (base_yyparse) is deeply recursive and runs at
    // database-open time: statically-linked extensions (e.g. json) register
    // their internal SQL macros during Load(), which parses SQL via the pg
    // parser. The default 1 MiB wasm stack overflows on that
    // open -> Load -> ParseExpressionList -> yyparse chain and traps in
    // core_yylex. Reserve a larger stack; --stack-first (set by the target)
    // places it at the base of linear memory so an overflow faults cleanly.
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=stack-size=8388608");

    // The delta extension merges delta-kernel-rs (a Rust `staticlib`) into
    // libduckdb-wasi.a; that bundles its own copy of the Rust std runtime
    // (rust_panic / __rdl_alloc / rust_eh_personality / ...), which collide with
    // the core component's std at this link. Same toolchain on both sides, so let
    // rust-lld keep the first definition. Only when delta is actually present.
    if let Ok(lib) = std::env::var("DUCKDB_STATIC_LIB") {
        if let Ok(bytes) = std::fs::read(&lib) {
            let needle = b"get_sync_engine";
            if bytes.windows(needle.len()).any(|w| w == needle) {
                println!("cargo:rustc-link-arg=--allow-multiple-definition");
                println!("cargo:rerun-if-changed={}", lib);
            }
        }
    }

    // postgres_scanner's getaddrinfo wrapper: wasi-libc's getaddrinfo rejects
    // numeric IP literals + loopback (resolve-addresses returns "unsupported"),
    // so libpq can't connect by IP. When the merged archive provides the C helper
    // (postgres_scanner enabled), enable the Rust trampolines (cfg pg_net) and
    // --wrap getaddrinfo/freeaddrinfo onto them. The trampolines must live in the
    // root crate (not an on-demand archive) for --wrap to bind them, mirroring
    // the fs-shims __wrap_open.
    println!("cargo:rustc-check-cfg=cfg(pg_net)");
    if let Ok(lib) = std::env::var("DUCKDB_STATIC_LIB") {
        if let Ok(bytes) = std::fs::read(&lib) {
            let needle = b"pg_wasi_getaddrinfo";
            if bytes.windows(needle.len()).any(|w| w == needle) {
                println!("cargo:rustc-cfg=pg_net");
                println!("cargo:rustc-link-arg=--wrap=getaddrinfo");
                println!("cargo:rustc-link-arg=--wrap=freeaddrinfo");
                println!("cargo:rerun-if-changed={}", lib);
            }
        }
    }

    if std::env::var_os("CARGO_FEATURE_FS_SHIMS").is_none() {
        return;
    }

    const SHIM_SYMBOLS: &[&str] = &[
        "open",
        "close",
        "read",
        "pread",
        "write",
        "pwrite",
        "lseek",
        "fsync",
        "fdatasync",
        "ftruncate",
        "stat",
        "lstat",
        "fstat",
        "mkdir",
        "rmdir",
        "unlink",
        "remove",
        "rename",
        "access",
        "isatty",
        "opendir",
        "readdir",
        "closedir",
        "chdir",
        "getcwd",
        "readlink",
        "_ZN6duckdb16DatabaseInstance21LoadExtensionSettingsEv",
    ];

    for sym in SHIM_SYMBOLS {
        println!("cargo:rustc-link-arg=--wrap={}", sym);
    }
}

/// Compiles a single cpp/<name> in-core (DUCKDB_BUILD_LIBRARY) with the EXACT
/// wasi-sdk clang++ flags extracted from sqlite_scanner's sqlite_storage.cpp
/// build, so the resulting object's C++ ABI / exception model matches the
/// prebuilt libduckdb-wasi.a it links against. The object is linked into the
/// core component via `cargo:rustc-link-arg`. Used for wasm_storage.cpp (the
/// StorageExtension stub) and wasm_files.cpp (the httpfs M1 FileSystem stub).
fn build_wasm_cpp(file_name: &str) {
    use std::path::PathBuf;
    use std::process::Command;

    let wasi_sdk = std::env::var("WASI_SDK_PREFIX")
        .unwrap_or_else(|_| panic!("Set WASI_SDK_PREFIX (source scripts/setup-env.sh) to build {file_name}"));
    let duckdb_src = std::env::var("DUCKDB_SOURCE_DIR")
        .unwrap_or_else(|_| panic!("Set DUCKDB_SOURCE_DIR (source scripts/setup-env.sh) to build {file_name}"));

    let clangxx = format!("{wasi_sdk}/bin/clang++");
    let sysroot = format!("{wasi_sdk}/share/wasi-sysroot");

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let src = PathBuf::from(&manifest).join("cpp").join(file_name);
    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-env-changed=WASI_SDK_PREFIX");
    println!("cargo:rerun-if-env-changed=DUCKDB_SOURCE_DIR");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let obj = PathBuf::from(&out_dir).join(format!("{file_name}.obj"));

    // Force-include shim path: relative to this crate, mirroring the CMake build.
    let shim = PathBuf::from(&manifest).join("../cmake/toolchains/wasi-shim.hpp");
    let shim = shim.canonicalize().unwrap_or(shim);
    let override_inc = PathBuf::from(&manifest).join("../cmake/wasi-override/include");
    let override_inc = override_inc.canonicalize().unwrap_or(override_inc);

    // Third-party include roots required because duckdb.hpp pulls in fmt etc.
    let tp = |p: &str| format!("{duckdb_src}/third_party/{p}");
    let mut cmd = Command::new(&clangxx);
    cmd.arg(format!("--sysroot={sysroot}"))
        .arg("-DDUCKDB_BUILD_LIBRARY")
        .arg(format!("-I{duckdb_src}/src/include"))
        .arg(format!("-I{}", tp("fsst")))
        .arg(format!("-I{}", tp("fmt/include")))
        .arg(format!("-I{}", tp("hyperloglog")))
        .arg(format!("-I{}", tp("fastpforlib")))
        .arg(format!("-I{}", tp("skiplist")))
        .arg(format!("-I{}", tp("ska_sort")))
        .arg(format!("-I{}", tp("fast_float")))
        .arg(format!("-I{}", tp("re2")))
        .arg(format!("-I{}", tp("miniz")))
        .arg(format!("-I{}", tp("utf8proc/include")))
        .arg(format!("-I{}", tp("concurrentqueue")))
        .arg(format!("-I{}", tp("pcg")))
        .arg(format!("-I{}", tp("pdqsort")))
        .arg(format!("-I{}", tp("tdigest")))
        .arg(format!("-I{}", tp("mbedtls/include")))
        .arg(format!("-I{}", tp("httplib")))
        .arg(format!("-I{}", tp("jaro_winkler")))
        .arg(format!("-I{}", tp("vergesort")))
        .arg(format!("-I{}", tp("yyjson/include")))
        .arg(format!("-I{}", tp("zstd/include")))
        .arg("--target=wasm32-wasip2")
        .arg(format!("--sysroot={sysroot}"))
        .arg("-stdlib=libc++")
        .arg("-fwasm-exceptions")
        .arg("-mllvm")
        .arg("-wasm-use-legacy-eh=false")
        .arg("-D_WASI_EMULATED_MMAN")
        .arg("-D_WASI_EMULATED_SIGNAL")
        .arg("-DDISABLE_DUCKDB_REMOTE_INSTALL")
        .arg("-DDUCKDB_DISABLE_EXTENSION_LOAD")
        .arg("-DDUCKDB_NO_THREADS")
        .arg("-DDUCKDB_SKIP_HTTP")
        .arg(format!("-I{}", override_inc.display()))
        .arg(format!("-include{}", shim.display()))
        .arg("-O3")
        .arg("-DNDEBUG")
        .arg("-std=c++11")
        .arg("-fPIC")
        .arg("-o")
        .arg(&obj)
        .arg("-c")
        .arg(&src);

    let status = cmd.status().expect("failed to spawn wasi-sdk clang++");
    if !status.success() {
        panic!("clang++ failed to compile {file_name}: {status}");
    }

    // Link the object directly into the core component.
    println!("cargo:rustc-link-arg={}", obj.display());
}
