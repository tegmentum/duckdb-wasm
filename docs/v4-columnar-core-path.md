# v4 Columnar core path — `execute_scalar_function` memcpy dispatch

Concrete implementation plan for the core side of the proposed
`duckdb:extension@4.0.0` columnar ABI. Contract design + GO/NO-GO + the 82-110x
prototype live in `ducklink/docs/v4-columnar-abi.md` and
`ducklink/wit/v4-columnar-draft/`. This is the change to
`core/src/lib.rs::execute_scalar_function` (and `execute_cast`,
`execute_aggregate`) that the major-4 bump and the #183 core retarget adopt.

## Today (row-major, the cost)

`execute_scalar_function` does, per chunk:

```rust
let mut rows = Vec::with_capacity(row_count);          // 1 outer alloc
for row in 0..row_count {
    let mut args = Vec::with_capacity(columns.len());  // 2048 inner allocs/chunk
    for column in &columns {
        args.push(read_scalar_argument(column, row)?);  // per-cell tagged variant
    }
    rows.push(args);
}
let results = call_scalar_batch(handle, rows.as_slice(), invoke)?;  // ~73 ns/row boundary
for (row, result) in results.into_iter().enumerate() {
    write_duckvalue_to_vector(output, &returns, row, result)?;       // per-cell write
}
```

The `rows: Vec<Vec<Duckvalue>>` build (2048 inner Vecs/chunk), the per-cell
variant construction, and the canonical-ABI serialization of
`list<list<duckvalue>>` are the cost. DuckDB vectors are ALREADY flat contiguous
arrays, so the per-cell read loop is pure waste for fixed-width types.

## v4 (columnar, the win)

Build one `colvec` per argument by `memcpy` from the DuckDB vector data pointer
(already hoisted into `ScalarInputColumn.data`), copy the validity mask verbatim,
cross once, write the result column back by `memcpy`:

```rust
fn build_colvec(col: &ScalarInputColumn, rows: usize) -> Colvec {
    let data = match col.logical {
        Logicaltype::Int64 => {
            let src = col.data as *const i64;
            Column::Int64(slice::from_raw_parts(src, rows).to_vec()) // bulk memcpy
        }
        Logicaltype::Float64 => { /* list<f64> bulk memcpy */ }
        // ... every fixed-width physical type: one to_vec() == one memcpy ...
        Logicaltype::Text => Column::Text(/* element-wise, as today */),
        Logicaltype::Blob => Column::Blob(/* element-wise, as today */),
        Logicaltype::Complex(_) => Column::Complex(/* element-wise escape hatch */),
        // date/time/timestamp/timestamptz reuse the int32/int64 arms (physical).
    };
    // Validity: DuckDB's mask is the SAME packed-bit layout we ship; copy it.
    let validity = if col.validity.is_null() {
        Vec::new() // all-valid fast path: zero alloc, mirrors the v4 contract
    } else {
        slice::from_raw_parts(col.validity as *const u8, (rows + 7) / 8).to_vec()
    };
    Colvec { data, validity, rows: rows as u32 }
}

let args: Vec<Colvec> = columns.iter().map(|c| build_colvec(c, row_count)).collect();
let out = call_scalar_batch_col(handle, &args, invoke)?;     // ~1 ns/row boundary
write_colvec_to_vector(output, &entry.definition.returns, out)?;  // memcpy back + set validity
```

`write_colvec_to_vector` for a fixed-width result is `ptr::copy_nonoverlapping`
into `duckdb_vector_get_data(output)` plus a validity-mask write (or
`duckdb_vector_ensure_validity_writable` + copy when the column has NULLs). Text/
blob stay per-cell `duckdb_vector_assign_string_element` as today.

This removes: the `Vec<Vec<Duckvalue>>` build, the per-cell `read_scalar_argument`
match, the per-cell `write_duckvalue_to_vector` match, and the per-cell canonical
-ABI variant work. Fixed-width in/out becomes two memcpys per column + one
boundary crossing per chunk.

## NULL correctness

Identical to the row-major path: validity is copied out-of-band; an empty bitmap
== all valid. A `NullHandling::Propagate` scalar still yields NULL for any row
with a NULL argument — the guest `scalar_batch_col` primitive
(`datalink-extcore`) applies propagation and clears the output validity bit,
exactly as `scalar_batch` does per row today. The aggregate path is unchanged in
semantics (DuckDB pre-filters NULL inputs for custom aggregates).

## Embedded path

The `embedded` (compile-into-core) branch reads the same `colvec`s and runs the
Rust kernel directly (no WIT), so the embed framework benefits from the bulk
read/write without crossing a boundary at all.

## Build note

The core builds only for `wasm32-wasip2` against the prebuilt libduckdb static
lib (`WASI_SDK_PREFIX` + `DUCKDB_STATIC_LIB`); it is part of the coordinated,
review-gated major-4 rebuild, not the perf-pass branch.

## Status: LANDED (feat/wit-4.0.0)

`execute_scalar_function` / `execute_cast` / aggregate finalize now build colvecs
by bulk memcpy from the DuckDB vectors (`build_colvec`), cross via
`call-scalar-batch-col` / `call-cast-col` / `call-aggregate-col`, and write the
result column back (`write_colvec_to_vector`). The embedded (compile-into-core)
path stays row-major (no WIT boundary). Core bindings regenerated @4.0.0; the
wasm core **builds** (`cargo component build -p duckdb-component-core
--target wasm32-wasip2 --release --features wasi`) and imports the columnar
dispatch (no row-major batch). Verified end-to-end through `ducklink` (native
host + @4.0.0 core + @4.0.0 components): `isin_validate` -> true/false/NULL;
`harmonic_mean(1,2,4)` = 1.714286.

Note: the host-interface WIT files (storage-host / optimizer-host / parser-host /
table-stream-host / collation-host / files-host / pragma-host) were restored to
the canonical `wit/duckdb-extension/` + `wit/core/duckdb-core.wit` world (they had
drifted out of the canonical while surviving only in the stale generated
`core/wit` + `core/src/bindings.rs`), so `sync-core-wit.sh` now reproduces the
full core world.
