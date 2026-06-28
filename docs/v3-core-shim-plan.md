# v3 core-shim wiring status (parser/window/optimizer DONE; filter pushdown documented)

Status (2026-06-28, major-3 baseline `duckdb:extension@3.0.0`):

- **PARSER -- DONE, executes end-to-end.** The core (`core/src/lib.rs`) imports a
  new `parser-host` interface and intercepts in `execute`: when the built-in parser
  rejects a statement, it offers the text to each declared parser extension via
  `parser-host.call-parse` and runs the returned string->SQL rewrite. The host
  (ducklink-host) provides `parser-host` and routes `call-parse` to the owning
  component's `parser-dispatch` export. PROVEN: `LOAD ggsql; VISUALIZE SELECT
  'apple' AS label, 3 AS n UNION ALL SELECT 'pear', 1` returns
  `(apple,3,###) (pear,1,#)`.
- **WINDOW (aggregate+frame) -- DONE, executes end-to-end (no new code).** The
  core already registers component aggregates via the C aggregate API with real
  init/update/combine/finalize callbacks (`aggregate_state_*`) that accumulate the
  rows of each call and dispatch to the component. DuckDB's WINDOW machinery REUSES
  those callbacks, calling update with the frame's rows + finalize per output row.
  PROVEN: `harmonic_mean(x) OVER (ORDER BY i ROWS BETWEEN UNBOUNDED PRECEDING AND
  CURRENT ROW)` -> running harmonic mean `(1, 1.333, 1.714)`; a bounded sliding
  frame `ROWS BETWEEN 1 PRECEDING AND CURRENT ROW` -> `(1, 1.333, 2.667, 4)`. The
  explicit `aggregate-incr-dispatch.call-aggregate-window` contract entry is an
  alternative path; execution rides DuckDB's framing of the registered aggregate.
- **OPTIMIZER -- DONE, executes end-to-end.** A real C++ component-driven
  `OptimizerExtension` (`core/cpp/wasm_component_optimizer.cpp`) flattens the bound
  plan to neutral JSON, offers it to declared rules via the new `optimizer-host`
  interface (+ `wasm_optimizer_rewrite` bridge -> the component's
  `optimizer-dispatch.call-optimize`), and re-plans the returned `rewrite-query`
  SQL in place (Parser+Planner). PROVEN: `LOAD qopt; SELECT x FROM optme` -> `99`
  (the rule matches the GET on `optme` and rewrites the whole query).
- **TABLE-FN FILTER PUSHDOWN -- ADDITIVE MARKER LANDED @3.1.0; core-shim DESIGN
  COMPLETE; host-mediated SQL run is the remaining integration step.** Update
  (2026-06-28): the registration-marker blocker called out below was resolved as
  a SANCTIONED ADDITIVE MINOR (the first off the frozen major-3 baseline, the
  proof the freeze policy's additive growth-path works). Landed on the ducklink
  branch `feat/wit-3.1-filterpushdown`:
    - a NEW component-facing `table-stream` registration interface (opt-in marker,
      `register-filterable-table`) in a new opt-in world -- NO edit to the shared
      types/runtime enums (freeze rule 3). CONTRACT_MINOR 0->1, digest
      80d5951b -> 1b94f15d; catalog re-stamped + verified; existing @3.0.0
      components load UN-REBUILT on the 3.1.0 host (proven by a runtime test).
    - the host driver `ExtensionInstance::table_open_filtered` over the (already
      @3.0.0) `table-stream-dispatch.call-table-open-filtered`, plus a routable
      global callback-handle for the registered streaming fn.
    - PROVEN at the WIT boundary: the `numstream` component prunes rows AT THE
      SOURCE from the pushed-down neutral filter descriptor crossing the boundary
      (`v > 5` -> 6,7,8,9; `v > 2 AND v <= 6` -> 3,4,5,6), via the
      `table-stream-boundary-test` crate (mirrors the storage scan-filter proof).
  REMAINING (this core, host-mediated SQL): the by-value-safe core<->host bridge
  is designed as `table-stream-host` (added here, UNWIRED into the libduckdb world
  so this commit is non-breaking) -- mirrors `storage-host`: `ts-open-filtered`
  (handle, args, projection, ts-filter[]) / `ts-next` / `ts-close`. The C++ shim
  is a near-copy of `wasm_storage.cpp`'s scan TableFunction (filter_pushdown =
  true; init reads `column_ids` + `filters`; flatten to the neutral descriptor;
  `wasm_table_stream_open/fill/close` extern-C bridges -> the imported
  `table-stream-host`), registered by NAME (ExtensionUtil::RegisterFunction) from
  a new `filterable-tables` list added to `extension-loader-hooks.pending-
  registrations`, populated host-side from `take_pending_filterable_tables`.
  Why this slice is NOT wired in this isolated pass: it is a COORDINATED
  core+host change, and `ducklink-host`'s core bindgen reads the SIBLING main
  duckdb-wasm checkout's `core/wit` (path `../../../duckdb-wasm/core/wit`), so a
  core-world import of `table-stream-host` cannot be matched by a host provider
  without either editing the main duckdb-wasm working tree (which a concurrent
  agent is merging into) or hacking the bindgen path -- neither is permitted under
  the isolation constraint. The interface + plan are landed so integration is a
  mechanical wiring step once the branches merge; the boundary test already
  proves the filter reaches the component and prunes.

- **TABLE-FN FILTER PUSHDOWN (original v3 finding) -- feasible via internal
  C++, NOT via the stable C API.** Confirmed: the DuckDB stable C table-function
  API exposes only PROJECTION pushdown (`duckdb_table_function_supports_projection_
  pushdown` + `duckdb_init_get_column_index`); there is NO C entry point to mark a
  table function filter-pushdown-capable or to read the pushed `TableFilter` set.
  So `call-table-open-filtered` cannot be driven through the C API the component
  table functions currently use. It IS feasible over the boundary -- the STORAGE
  path already does by-value-safe filter pushdown (`storage.scan-filter` /
  `compare-op` extracted in `core/cpp/wasm_storage.cpp`, which is an internal C++
  `TableFunction` with `filter_pushdown = true`). The remaining work is a C++
  STREAMING `TableFunction` shim (the `wasm_storage` pattern: `filter_pushdown =
  true`; in init read `TableFunctionInitInput`'s filters + project list; open the
  cursor via a new `table-stream-host.call-open-filtered` carrying the neutral
  filter descriptor; `execute` pulls via `call-table-next`). The host-side driver
  (`table_stream_bindings` in ducklink-runtime) already exists; the gap is the C++
  streaming TableFunction + the `table-stream-host` interface + a streaming
  component. This is the one remaining capability; it is additive within major-3.

  Added constraint (why it is documented rather than wired now): the FROZEN
  component-facing contract registers every table function through
  `runtime.table-registry` with NO streaming / filter-pushdown marker, so the core
  cannot tell a streaming+filterable table fn from a regular one at registration
  time. Driving `call-table-open-filtered` therefore means either (a) a registration
  marker -- a change to the frozen WIT surface, out of scope here -- or (b)
  promoting ALL component table functions to internal C++ filter-pushdown
  TableFunctions, which risks regressing the working C-API whole-batch `call-table`
  path. Both are larger than an additive shim; hence documented with the plan above
  rather than half-wired (the storage `scan-filter` path remains the proof that
  by-value-safe filter pushdown reaches a component today).

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
