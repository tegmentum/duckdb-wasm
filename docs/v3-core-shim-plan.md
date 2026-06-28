# v3 core-shim wiring status (parser DONE; optimizer/window/filter remaining)

Status (2026-06-28, major-3 baseline `duckdb:extension@3.0.0`):

- **PARSER -- DONE, executes end-to-end.** The core (`core/src/lib.rs`) imports a
  new `parser-host` interface and intercepts in `execute`: when the built-in parser
  rejects a statement, it offers the text to each declared parser extension via
  `parser-host.call-parse` and runs the returned string->SQL rewrite. The host
  (ducklink-host) provides `parser-host` and routes `call-parse` to the owning
  component's `parser-dispatch` export. PROVEN: `LOAD ggsql; VISUALIZE SELECT
  'apple' AS label, 3 AS n UNION ALL SELECT 'pear', 1` returns
  `(apple,3,###) (pear,1,#)`.
- **OPTIMIZER / WINDOW / TABLE-FN FILTER PUSHDOWN -- contract + host capture done;
  DuckDB-execution driving DEFERRED.** Each needs deeper DuckDB-execution
  integration than the parser query-path interception:
  - Optimizer: a real C++ `OptimizerExtension` (scaffolding exists in
    `wasm_index_optimizer.cpp`) that flattens the plan to the neutral `plan-shape`,
    calls a new `optimizer-host.call-optimize`, and applies the rewrite directive.
    The `rewrite-query` directive needs a mid-optimize re-bind (a C++ Parser+Planner
    pass); the `apply(structured-rewrite)` directive generalizes the existing
    hard-coded index reroute.
  - Window (aggregate+frame): the core must register the component's incremental
    aggregate (init/update/combine/finalize via the C aggregate API) so DuckDB's
    window machinery frames it, plus drive `aggregate-incr-dispatch.call-aggregate-
    window`. `aggregate-incr-dispatch` is NOT yet wired core-side.
  - Table-fn filter pushdown: the core must first register a STREAMING table
    function driven through `table-stream-dispatch` (not yet wired core-side), then
    pass pushed filters via `call-table-open-filtered`. The host-side drivers
    (`table_stream_bindings`, `aggregate_incr_bindings`) already EXIST in
    ducklink-runtime; the gap is the core connecting them to DuckDB execution.

The PARSER pattern below was the template that landed; the remaining three follow
the same host<->core shape (a new `<cap>-host` interface + a core hook).

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
