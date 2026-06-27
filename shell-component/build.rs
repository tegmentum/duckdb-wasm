// Link the prebuilt DuckDB shell C++ objects (compiled by
// scripts/build-shell-wasm.sh into build/duckdb-shell/obj) directly into this
// cargo-component command crate. libduckdb-sys already links the merged
// libduckdb-wasi.a + the DuckDB third_party static libs; this build.rs only adds
// the shell translation units on top, plus the larger stack the
// open -> Load -> yyparse chain needs (mirrors core/build.rs).
fn main() {
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_arch != "wasm32" {
        return;
    }

    // Where the prebuilt shell objects live. Defaults to the sibling main
    // checkout's build dir; override with SHELL_OBJ_DIR.
    let obj_dir = std::env::var("SHELL_OBJ_DIR").unwrap_or_else(|_| {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        format!("{manifest}/../build/duckdb-shell/obj")
    });
    println!("cargo:rerun-if-env-changed=SHELL_OBJ_DIR");
    println!("cargo:rerun-if-changed={obj_dir}");

    let mut linked = 0usize;
    if let Ok(entries) = std::fs::read_dir(&obj_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("o") {
                println!("cargo:rustc-link-arg={}", path.display());
                linked += 1;
            }
        }
    }
    if linked == 0 {
        panic!(
            "no shell .o objects found in {obj_dir}; run scripts/build-shell-wasm.sh first \
             (or set SHELL_OBJ_DIR)"
        );
    }

    // libc++/libc++abi: the merged libduckdb-wasi.a only *references* operator
    // new/delete and the rest of the C++ runtime; the standalone shell build
    // resolves them because it links via the clang++ driver (which auto-adds
    // -lc++ -lc++abi). rust-lld does not, so add them explicitly. Use the `eh`
    // variant to match the -fwasm-exceptions objects.
    let wasi_sdk = std::env::var("WASI_SDK_PREFIX")
        .expect("set WASI_SDK_PREFIX (source scripts/setup-env.sh)");
    let triple = std::env::var("WASI_TARGET_TRIPLE").unwrap_or_else(|_| "wasm32-wasip2".to_string());
    let cxx_lib = format!("{wasi_sdk}/share/wasi-sysroot/lib/{triple}/eh");
    println!("cargo:rustc-link-search=native={cxx_lib}");
    println!("cargo:rustc-link-lib=static=c++");
    println!("cargo:rustc-link-lib=static=c++abi");

    // Larger stack for the libpg_query parser run at database open (see
    // core/build.rs for the full rationale). --stack-first is set by the target.
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=stack-size=8388608");
}
