//===----------------------------------------------------------------------===//
// wasm_optimizer_bridge.h
//
// C ABI between the C++ component-optimizer shim (wasm_component_optimizer.cpp)
// and the Rust core (core/src/lib.rs), which routes to the host-provided
// `duckdb:extension/optimizer-host` import and on to declared optimizer
// components' `optimizer-dispatch` export (2.3.0 / v3).
//===----------------------------------------------------------------------===//
#ifndef WASM_OPTIMIZER_BRIDGE_H
#define WASM_OPTIMIZER_BRIDGE_H

#ifdef __cplusplus
extern "C" {
#endif

// Offer the flattened plan JSON to every declared component optimizer rule.
// Returns a heap C string with the rewrite SQL (free via wasm_optimizer_free),
// or NULL if no rule rewrote it.
char *wasm_optimizer_rewrite(const char *plan_json);
void wasm_optimizer_free(char *ptr);

#ifdef __cplusplus
}
#endif

#endif // WASM_OPTIMIZER_BRIDGE_H
