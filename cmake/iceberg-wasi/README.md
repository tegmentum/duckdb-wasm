# Iceberg extension on wasm — WORKING (local read)

DuckDB's `iceberg` extension reads Apache Iceberg tables on wasm32-wasip2:
`SELECT * FROM iceberg_scan('<local path>')` runs end-to-end through the wasm core
(verified by `iceberg_scan_embedded_local_table` in `ducklink-host`, which reads
the `partition_bool` fixture — 2 records — through the avro-manifest + roaring +
parquet path). It's duckdb-iceberg @ `e6fe0a4b` (DuckDB 1.5.4's pin from
`.github/config/extensions/iceberg.cmake`), fetched via the normal `embed_ext`
FetchContent. Upstream excludes iceberg from non-vcpkg/Emscripten builds because
it links the **AWS C++ SDK** (for AWS-native REST catalogs — Glue, S3 Tables).

## Why no AWS SDK is needed

The AWS C++ SDK does not build for wasm. But iceberg's **default** REST-request
path (`AWSInput::ExecuteRequest`, with the
`iceberg_via_aws_sdk_for_catalog_interactions` setting off — the default) already
computes the **SigV4 signature by hand** (DuckDB's mbedtls sha256/hmac) and issues
the signed request through DuckDB's own **HTTPUtil (curl)** — no AWS SDK. The AWS
SDK is only used by the opt-in legacy path (`CreateSignedRequest` /
`ExecuteRequestLegacy`), which is already `#ifndef EMSCRIPTEN`-guarded upstream.

So on wasm we don't replace anything — we just need the pervasively-AWS-typed code
(`Aws::Http::HttpMethod` / `URI` / `Scheme`, the credentials + HTTP-client types,
which appear in method signatures across `src/catalog/rest/storage/aws.{cpp,hpp}`
and `.../authorization/sigv4.cpp`) to **compile**.

## How it's wired (`scripts/build-libduckdb-wasm.sh`, iceberg section)

1. **CMake**: extend the upstream `NOT CMAKE_SYSTEM_NAME STREQUAL "Emscripten"`
   guards to also skip the `find_package(CURL/AWSSDK)` on `WASI` (curl + openssl
   come from httpfs's wasi build), and put `cmake/iceberg-wasi/aws-stubs` on the
   include path so `<aws/core/...>` resolves to the stubs.
2. **aws.cpp**: extend the two `#ifndef EMSCRIPTEN` blocks (the only spots with
   heavy AWS-SDK calls — `CreateHttpRequest` / `AWSAuthV4Signer` / `CreateHttpClient`)
   to `#if !defined(EMSCRIPTEN) && !defined(__wasi__)`, so wasi takes their existing
   `#else` fallback. Everything else compiles against the stubs.

## The AWS-type stubs (`aws-stubs/aws/core/...`)

Minimal headers providing exactly the AWS types the iceberg code names:
- `http/HttpRequest.h` — the `HttpMethod`/`Scheme` enums, `HttpMethodMapper`, and a
  **real** minimal `URI` (the default path builds it + reads it back —
  scheme/authority/percent-encoded path/query/full string — to assemble the SigV4
  canonical request and the request URL). `HttpRequest` is opaque-enough for the
  compiled-out legacy declarations.
- `Aws.h` — `SDKOptions` + no-op `InitAPI`, `StringStream`, `MakeShared`, and
  `Aws::Client::ClientConfiguration` (returned by the type-only `BuildClientConfig`).
- `auth/AWSCredentials.h` + `AWSCredentialsProvider.h` + `AWSCredentialsProviderChain.h`,
  `http/HttpClient.h` — minimal; only referenced from the compiled-out legacy path.

## Dependencies

iceberg `AutoLoadExtension("avro")`s (so **avro** must be embedded too — both fetch
their wasi C libs via `scripts/build-wasi-deps.sh`: jansson + avro-c for avro,
**roaring** (CRoaring) for iceberg manifests). The merge step adds libavro /
libjansson / libsnappy / liblzma / libroaring to `libduckdb-wasi.a`.

## Scope

- **Local Iceberg** (`iceberg_scan('/path')`) works fully (metadata.json + avro
  manifests + snappy parquet).
- **REST catalogs**: the SigV4 + curl path is compiled (not the legacy AWS-SDK
  path), so AWS-native catalogs *should* sign + fetch via curl over httpfs's
  `wasi:sockets` — not yet verified end-to-end here. The opt-in
  `iceberg_via_aws_sdk_for_catalog_interactions` legacy path is unavailable on wasm.
