# duckdb-wasm

The DuckDB engine compiled to a **WebAssembly Component** (`wasm32-wasip2`),
packaged as the reusable core that consumers embed — without pulling in the
rest of [ducklink](https://github.com/tegmentum/ducklink).

## Crates

- **`core`** (`duckdb-component-core`) — wraps `libduckdb-sys` and exports the
  `duckdb:component/libduckdb` world; compiles to `ducklink_core.wasm`. Its WIT
  contract (including the `duckdb:extension` import surface and the `tvm:memory`
  spill interface) is self-contained under `core/wit`.
- **`libduckdb-sys`** — DuckDB's C/C++ amalgamation built for wasm.

## Building

Requires `cargo component`, the `wasm32-wasip2` Rust target, and four build
inputs (wasi-sdk 33.0+, the prebuilt `libduckdb-wasi.a`, the DuckDB headers, and
the DuckDB wasm CMake build dir). `scripts/setup-env.sh` wires them, defaulting
to the sibling `ducklink` checkout's prebuilt artifacts (override any var to go
standalone):

```sh
make core            # standalone/server core (wasi feature)
make core-browser    # browser core (no fs shims)
make env             # print the resolved inputs (+ whether each exists)
```

Equivalently, by hand:

```sh
source scripts/setup-env.sh
cargo component build -p duckdb-component-core --target wasm32-wasip2 --release --features wasi
```

The compiled component is `target/wasm32-wasip2/release/ducklink_core.wasm`.

To rebuild `libduckdb-wasi.a` itself from DuckDB source (needs wasi-sdk), use
`./scripts/build-libduckdb-wasm.sh`; `./scripts/sync-core-wit.sh` regenerates
`core/wit` from a canonical `wit/`.

## Consumers

[ducklink](https://github.com/tegmentum/ducklink) (host, CLI, extension
ecosystem) depends on this repo by path (`../duckdb-wasm`): its host bindgens
against `core/wit` and loads the compiled `ducklink_core.wasm` at runtime.
