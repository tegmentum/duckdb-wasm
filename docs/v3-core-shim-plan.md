# v3 core-shim wiring plan (parser + general optimizer)

Status: **DEFERRED core-shim work for the `duckdb:extension@2.3.0` (v3) contract.**

The v3 WIT contract (in tegmentum/ducklink, branch `feat/wit-v3`) added two new
advanced-tier capabilities that need a wasm-core C++ shim, plus a host<->core drain
extension. The CONTRACT, host capture wiring, ggsql PoC component, freeze policy,
leak audit, and stable-C peg are DONE and verified. This doc is the remaining
core-shim integration, anchored to the exact files, so it can land as a focused
follow-up (it needs a full `build-libduckdb-wasm.sh` + `make core` cycle to verify,
which is why it is split out).

What already works without this: the host (ducklink-runtime) CAPTURES parser and
optimizer registrations (`ExtensionStoreState::register_parser_extension` /
`register_optimizer_rule` -> `pending_parsers` / `pending_optimizers`, drained via
`take_pending_parsers` / `take_pending_optimizers`). The ggsql PoC loads and the
registration is captured ("registered parser extension 'ggsql'"). The missing piece
is the CORE applying those registrations to DuckDB and driving the component's
`parser-dispatch` / `optimizer-dispatch` exports.

## 1. Extend the host<->core drain protocol

`core/src/lib.rs` ~L4366: `extension_loader_hooks::get_pending_registrations()`
returns a struct with `scalars` / `tables` / `aggregates` / `macros` / ... Add
`parsers: Vec<ParserReg>` and `optimizers: Vec<OptimizerReg>` to that struct (and
its WIT/bridge definition between core and host), populated from the host's
`take_pending_parsers` / `take_pending_optimizers`. Then in
`process_pending_registrations` register each into DuckDB via new FFI calls.

## 2. Parser: ParserExtension shim (new `core/cpp/wasm_parser.cpp`)

Mirror `wasm_collation.cpp` / `wasm_index.cpp`. Register a `ParserExtension` whose
`parse_function` receives the raw statement text DuckDB's built-in parser rejected:

- `ParserExtensionParseResult parse(ParserExtensionInfo*, const string &query)`:
  call back into the host -> the component's `parser-dispatch.call-parse(handle,
  query)`. On `declined` return `ParserExtensionParseResult()` (no match, DuckDB
  proceeds). On `rewrite(sql)`, the by-value-safe path: re-parse `sql` with a fresh
  `Parser` and carry the resulting statements in a `ParserExtensionParseData`
  subclass; in `plan_function`, splice them in (or, simplest for the PoC: stash the
  rewrite SQL and run it via the pending-statement mechanism the pragma path
  already uses in the core -- `wasm_*` returns-SQL handling).
- The host bridge (`core/cpp/wasm_*_bridge.h` style) declares the extern "C"
  `wasm_parser_call_parse(handle, query_ptr, query_len) -> rewrite-or-null` the
  Rust core implements by invoking the component through the
  `duckdb-extension-parser` world bindings (`duckdb_extension_parser_bindings` in
  ducklink-runtime: drive `parser-dispatch.call-parse`).

`void wasm_register_parser_extension(DatabaseInstance&, const string &name, u32 handle)`
calls `config.parser_extensions.push_back(WasmParserExtension(name, handle))` (the
same `DBConfig` registration shape `OptimizerExtension::Register` uses).

## 3. Optimizer: generalize `core/cpp/wasm_index_optimizer.cpp`

Today `WasmIndexScanOptimizer` (L111) hard-codes ONE rule: match `TOP_N ->
PROJECTION(array_distance) -> GET`, find a `wasm_hnsw` index, reroute. Generalize to
a component-driven rule:

1. Keep the existing hard-coded rule as the built-in fast path (it works; don't
   regress vss/rtree).
2. Add `WasmComponentOptimizer : OptimizerExtension`. Its `Optimize` walks the plan
   and FLATTENS each operator to the v3 `optimizer-dispatch.plan-node` shape:
   `{id, op-type = EnumUtil::ToString(op.type), parent, params-json}` where
   `params-json` carries the neutral params already extracted by the index rule
   (table name from `LogicalGet`, projected column names, the order-by/limit/filter
   expression TEXT via `Expression::ToString()`). Build `plan-shape{nodes, query}`.
3. For each registered optimizer rule (from step 1's drain), call the component's
   `optimizer-dispatch.call-optimize(handle, plan-shape)`.
4. Apply the returned `rewrite-directive`:
   - `declined`: no-op.
   - `rewrite-query(sql)`: re-bind+re-plan `sql` (a fresh `Planner` over a parsed
     statement) and replace the plan -- the neutral path.
   - `apply(structured-rewrite{target, directive, args-json})`: dispatch to a
     core-owned rewrite table. Reimplement the index reroute as the first such
     directive (`"use-index"`), so the hard-coded rule BECOMES a component-expressed
     rule: the directive's `args-json` names the index + column, and the core does
     the same `DataTable` index lookup + plan splice the current `TryOptimize` does
     (L117-258).

The flatten + the `Expression::ToString()` extraction reuse what `MatchDistanceArgs`
/ `TryOptimize` already pull out; no DuckDB struct crosses the WIT boundary (the
leak audit requires this -- only strings/JSON/indices go out).

## 4. Verify

`EMBED_EXTENSIONS="" ./scripts/build-libduckdb-wasm.sh && make core`, stage the core
into ducklink, then drive the ggsql PoC: `VISUALIZE SELECT region, count(*) FROM t
GROUP BY region` should rewrite to the grouped rollup SQL and return rows; and a
component optimizer rule should fire on a flattened plan. Add a ducklink-host test
mirroring `delta_scan_embedded_local_table`.
