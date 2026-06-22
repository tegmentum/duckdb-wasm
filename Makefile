# duckdb-wasm: build the DuckDB-compiled-to-WebAssembly core component.
#
# Build inputs come from scripts/setup-env.sh (wasi-sdk + the prebuilt
# libduckdb-wasi.a + DuckDB headers + the DuckDB wasm build dir). Override any of
# WASI_SDK_PREFIX / DUCKDB_STATIC_LIB / DUCKDB_SOURCE_DIR / DUCKDB_BUILD_DIR.

WASI_TARGET ?= wasm32-wasip2
ENV = . scripts/setup-env.sh >/dev/null

.PHONY: core core-browser env clean

# Print the resolved build environment (and whether each input exists).
env:
	@. scripts/setup-env.sh

# The standalone/server core (wasi feature: fs shims, sockets).
core:
	$(ENV) && cargo component build -p duckdb-component-core \
	  --target $(WASI_TARGET) --release --features wasi

# The browser core (no fs shims; jco-transpiled for the web build).
core-browser:
	$(ENV) && cargo component build -p duckdb-component-core \
	  --target $(WASI_TARGET) --release --no-default-features --features browser

clean:
	cargo clean
