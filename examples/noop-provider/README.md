# noop-provider

Trivial component exporting empty/no-op implementations of every non-WASI
interface `ducklink_core.wasm` (the DuckDB core component) declares as an
import.

Composing `ducklink_core.wasm` against `noop-provider.wasm` yields a
database-only component whose remaining imports are WASI + two type-only
interfaces (`duckdb:extension/types`, `duckdb:extension/column-types`).

## Build

```
./build.sh
```

Requires:
- `WASI_SDK_PREFIX` (or `/opt/wasi-sdk`)
- `wit-bindgen` CLI 0.57+
- `wasm-tools`
- `wac`

## Compose

```
wac plug \
  ../../target/wasm32-wasip2/release/ducklink_core.wasm \
  --plug build/noop-provider.wasm \
  -o build/ducklink-with-noop.wasm
```

## What "noop" means here

Every function is a well-typed no-op:
- Void-returning: fills `*ret` with an empty list or `none` option
- Bool-returning: zero-initializes `*ret` and returns `true` (success)

This is deliberately different from a trap-everywhere stub: DuckDB pokes
at these interfaces during startup (extension registration probes, pragma
listings, catalog init) even when nothing is registered. A trap aborts
the whole component; empty-success lets DuckDB fall through to its
default paths.

## Downstream consumers

- **pylon** (`~/git/python-wasm`) — uses this compose to give the wasm
  Python interpreter a `_duckdb_cap` static extension backed by real
  DuckDB. See `cpython-ext/_duckdb_capability/` in pylon.

Other Python/JS/host runtimes wanting a "database only, no extension
system" DuckDB should compose the same way.
