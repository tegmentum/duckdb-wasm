// quack_wasi_bridge.cpp -- wasm32-wasip2 SERVER bridge for the quack extension.
//
// quack's httplib server (quack_http_server.cpp: a 128-thread ThreadPool +
// listen/accept) can't listen() inside the wasip2 sandbox. Following the ui
// extension's pattern (duckdb_ui_handle_request + the native host's accept
// loop), the native host (ducklink-host) owns the TcpListener + single-threaded
// accept loop and bridges each POST /quack body into the core here.
//
// The decouple is clean because quack's per-request HANDLER is already
// independent of httplib: QuackServer::HandleMessage(MemoryStream&) does the
// whole deserialize -> run query -> serialize (DuckDB-internal serialization,
// lossless) round-trip. The httplib glue in quack_http_server.cpp is only:
//   read body -> MemoryStream -> HandleMessage -> ToMemoryStream -> set content.
// This file reproduces exactly that, minus httplib, behind a C entrypoint.
//
// Staged into the fetched quack source + added to its object library, and
// wired into CreateServer (quack_storage.cpp), by scripts/build-libduckdb-wasm.sh.

#ifdef __wasi__

#include <cstdlib>
#include <cstring>

#include "duckdb/common/serializer/memory_stream.hpp"
#include "quack_message.hpp"
#include "quack_server.hpp"
#include "quack_uri.hpp"

namespace duckdb {

// A concrete, listen-less QuackServer: it inherits the full request processor
// (HandleMessage / the active-connections/session map) and merely exposes the
// protected handler. No duckdb_httplib::Server, no ThreadPool, no std::thread.
class WasiQuackServer : public QuackServer {
public:
	WasiQuackServer(ClientContext &context, const QuackUri &uri, const string &token)
	    : QuackServer(context, uri, token) {
	}
	unique_ptr<QuackMessage> HandleOne(MemoryStream &read_stream) {
		return HandleMessage(read_stream);
	}
};

// One bridge server for the process lifetime. CreateServer() (patched on wasi)
// constructs it and records it here; the host's accept loop reaches it through
// the C entrypoint below. Non-owning: the QuackStorageExtensionInfo::servers map
// owns the unique_ptr; this is cleared when that server is dropped.
static WasiQuackServer *g_wasi_quack_server = nullptr;

// Called by the patched CreateServer on wasi: build the bridge server (no
// listener) and return it as a QuackServer* for the servers map to own.
QuackServer *QuackWasiCreateServer(ClientContext &context, const QuackUri &uri, const string &token) {
	auto *server = new WasiQuackServer(context, uri, token);
	g_wasi_quack_server = server;
	return server;
}

void QuackWasiClearServer(QuackServer *server) {
	if (g_wasi_quack_server == server) {
		g_wasi_quack_server = nullptr;
	}
}

} // namespace duckdb

using namespace duckdb;

extern "C" {

// Handle one serialized quack request body and return a malloc'd buffer holding
// the serialized response body (DuckDB-internal "application/vnd.duckdb"). On
// any failure (no active bridge server, OOM, or a handler exception) returns
// nullptr with *out_len = 0. The caller (the host bridge, via the core's WIT
// handle-quack-request export) frees the buffer with duckdb_quack_free.
uint8_t *duckdb_quack_handle_request(const uint8_t *body, size_t body_len, size_t *out_len) {
	*out_len = 0;
	if (!g_wasi_quack_server) {
		return nullptr;
	}
	try {
		MemoryStream in_stream;
		if (body_len) {
			in_stream.WriteData(reinterpret_cast<data_ptr_t>(const_cast<uint8_t *>(body)), body_len);
		}
		auto response = g_wasi_quack_server->HandleOne(in_stream);
		MemoryStream out_stream;
		response->ToMemoryStream(out_stream);
		const size_t n = out_stream.GetPosition();
		auto *buf = static_cast<uint8_t *>(malloc(n ? n : 1));
		if (!buf) {
			return nullptr;
		}
		if (n) {
			memcpy(buf, out_stream.GetData(), n);
		}
		*out_len = n;
		return buf;
	} catch (...) {
		return nullptr;
	}
}

void duckdb_quack_free(uint8_t *buf) {
	free(buf);
}

} // extern "C"

#endif // __wasi__
