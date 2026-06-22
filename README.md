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

Requires the wasi-sdk toolchain and `cargo component`:

```sh
./scripts/build-libduckdb-wasm.sh        # build libduckdb.a for wasm
./scripts/sync-core-wit.sh               # regenerate core/wit from wit/
cargo component build -p duckdb-component-core --target wasm32-wasip2 --release --features wasi
```

The compiled component is `target/wasm32-wasip2/release/ducklink_core.wasm`.

## Consumers

[ducklink](https://github.com/tegmentum/ducklink) (host, CLI, extension
ecosystem) depends on this repo by path (`../duckdb-wasm`): its host bindgens
against `core/wit` and loads the compiled `ducklink_core.wasm` at runtime.
