//===----------------------------------------------------------------------===//
// wasm_files.cpp
//
// httpfs M2: a real forwarding FileSystem subsystem for the wasm core.
//
// Mirrors the PROVEN storage shim (wasm_storage.cpp): a C++ class subclasses a
// DuckDB-internal type (here duckdb::FileSystem instead of StorageExtension),
// gets registered on the live DatabaseInstance mid-session via an extern-C
// bridge driven from Rust (core/src/lib.rs), and routes a path family to itself.
//
// M1 proved the mechanism (CanHandleFile claims http(s):// and OpenFile threw a
// stub). M2 makes it real: OpenFile/Read/GetFileSize forward through the C ABI
// (wasm_files_bridge.h) to a files-backend wasm component (webfs), which fetches
// the whole resource over wasi:sockets, caches it, and serves byte ranges. The
// reader (read_csv) globs the URL, opens it, then pulls bytes -> rows.
//
// Compiled in-core (DUCKDB_BUILD_LIBRARY) with the EXACT wasi-sdk flags used for
// wasm_storage.cpp (see core/build.rs).
//===----------------------------------------------------------------------===//

#include "duckdb.hpp"
#include "duckdb.h"

#include "duckdb/common/file_system.hpp"
#include "duckdb/common/file_open_flags.hpp"
#include "duckdb/common/open_file_info.hpp"
#include "duckdb/common/exception.hpp"
#include "duckdb/main/capi/capi_internal.hpp"
#include "duckdb/main/database.hpp"

#include "wasm_files_bridge.h"

#include <atomic>
#include <string>

namespace duckdb {

//===----------------------------------------------------------------------===//
// WasmFileHandle
//
// Pure-virtual from FileHandle in this version: only Close(). The ctor signature
// is FileHandle(FileSystem&, string path, FileOpenFlags). Carries the component-
// side wasm handle, the total size (discovered at open), and the streaming-read
// cursor for the position-less Read/Seek overloads.
//===----------------------------------------------------------------------===//

class WasmFileHandle : public FileHandle {
public:
	WasmFileHandle(FileSystem &file_system, const string &path, FileOpenFlags flags, uint32_t handle, uint64_t size)
	    : FileHandle(file_system, path, flags), wasm_handle(handle), file_size(size), position(0) {
	}

	void Close() override {
		if (!closed) {
			wasm_file_close(wasm_handle);
			closed = true;
		}
	}

	uint32_t wasm_handle;
	uint64_t file_size;
	uint64_t position;
	bool closed = false;
};

//===----------------------------------------------------------------------===//
// WasmFileSystem
//
// VirtualFileSystem dispatch: FindFileSystem() picks the subsystem whose
// CanHandleFile(path) returns true, then calls OpenFile(file.path, ...) on it
// (SupportsOpenFileExtended() defaults false, so the string overload is used).
//===----------------------------------------------------------------------===//

class WasmFileSystem : public FileSystem {
public:
	bool CanHandleFile(const string &fpath) override {
		return StringUtil::StartsWith(fpath, "http://") || StringUtil::StartsWith(fpath, "https://") ||
		       StringUtil::StartsWith(fpath, "s3://") || StringUtil::StartsWith(fpath, "az://") ||
		       StringUtil::StartsWith(fpath, "azure://") || StringUtil::StartsWith(fpath, "gcs://") || StringUtil::StartsWith(fpath, "gs://");
	}

	unique_ptr<FileHandle> OpenFile(const string &path, FileOpenFlags flags,
	                                optional_ptr<FileOpener> opener) override {
		// M2 is read-only: reject writes early with a clear message.
		if (flags.OpenForWriting()) {
			throw NotImplementedException("wasm filesystem: write to '%s' not supported", path);
		}
		uint32_t handle = 0;
		uint64_t size = 0;
		if (!wasm_file_open(path.c_str(), &handle, &size)) {
			throw IOException("wasm filesystem: %s", wasm_file_last_error());
		}
		return make_uniq<WasmFileHandle>(*this, path, flags, handle, size);
	}

	string GetName() const override {
		return "WasmFileSystem";
	}

	// http(s) URLs are not glob patterns: the reader globs the path before
	// opening, so return the path verbatim as the single match. This lets the
	// scan proceed to OpenFile. Mirrors how the real httpfs HTTPFileSystem
	// handles non-glob remote paths.
	vector<OpenFileInfo> Glob(const string &path, FileOpener *opener) override {
		return {OpenFileInfo(path)};
	}

	// Positional read: pull [location, location+nr_bytes) from the component
	// cache. The CSV reader uses this overload with explicit offsets.
	void Read(FileHandle &handle, void *buffer, int64_t nr_bytes, idx_t location) override {
		auto &wh = handle.Cast<WasmFileHandle>();
		int64_t got = wasm_file_read(wh.wasm_handle, (uint64_t)location, (uint32_t)nr_bytes, (uint8_t *)buffer);
		if (got < 0) {
			throw IOException("wasm filesystem: %s", wasm_file_last_error());
		}
		if (got != nr_bytes) {
			throw IOException("wasm filesystem: short read on '%s' (wanted %lld, got %lld at offset %llu)",
			                  wh.path, (long long)nr_bytes, (long long)got, (unsigned long long)location);
		}
	}

	// Streaming read at the handle's current position; advances it (capped at
	// EOF). Returns the byte count actually read; a short read at EOF is fine.
	int64_t Read(FileHandle &handle, void *buffer, int64_t nr_bytes) override {
		auto &wh = handle.Cast<WasmFileHandle>();
		if (wh.position >= wh.file_size) {
			return 0;
		}
		uint64_t remaining = wh.file_size - wh.position;
		uint32_t want = nr_bytes < 0 ? 0 : (uint32_t)nr_bytes;
		if ((uint64_t)want > remaining) {
			want = (uint32_t)remaining;
		}
		int64_t got = wasm_file_read(wh.wasm_handle, wh.position, want, (uint8_t *)buffer);
		if (got < 0) {
			throw IOException("wasm filesystem: %s", wasm_file_last_error());
		}
		wh.position += (uint64_t)got;
		return got;
	}

	void Write(FileHandle &handle, void *buffer, int64_t nr_bytes, idx_t location) override {
		throw NotImplementedException("wasm filesystem: Write not supported");
	}
	int64_t Write(FileHandle &handle, void *buffer, int64_t nr_bytes) override {
		throw NotImplementedException("wasm filesystem: Write not supported");
	}

	int64_t GetFileSize(FileHandle &handle) override {
		return (int64_t)handle.Cast<WasmFileHandle>().file_size;
	}

	// Position-less streaming relies on Seek/SeekPosition for the handle cursor.
	void Seek(FileHandle &handle, idx_t location) override {
		handle.Cast<WasmFileHandle>().position = (uint64_t)location;
	}
	idx_t SeekPosition(FileHandle &handle) override {
		return (idx_t)handle.Cast<WasmFileHandle>().position;
	}
	bool CanSeek() override {
		return true;
	}
	bool OnDiskFile(FileHandle &handle) override {
		return false;
	}

	// Directory / metadata ops are not meaningful for remote http(s) reads.
	bool FileExists(const string &filename, optional_ptr<FileOpener> opener) override {
		return true;
	}
	void CreateDirectory(const string &directory, optional_ptr<FileOpener> opener) override {
		throw NotImplementedException("wasm filesystem: CreateDirectory not supported");
	}
	void RemoveDirectory(const string &directory, optional_ptr<FileOpener> opener) override {
		throw NotImplementedException("wasm filesystem: RemoveDirectory not supported");
	}
	void RemoveFile(const string &filename, optional_ptr<FileOpener> opener) override {
		throw NotImplementedException("wasm filesystem: RemoveFile not supported");
	}
};

} // namespace duckdb

//! Registers the wasm FileSystem as a subsystem of the database's
//! VirtualFileSystem. Idempotent: a process-wide once-guard plus a try/catch
//! (RegisterSubSystem throws InvalidInputException if a FS with the same name is
//! already registered) make repeated calls harmless.
extern "C" void wasm_register_file_system(duckdb_database db) {
	static std::atomic<bool> registered {false};
	if (!db) {
		return;
	}
	bool expected = false;
	if (!registered.compare_exchange_strong(expected, true)) {
		return;
	}
	try {
		auto wrapper = reinterpret_cast<duckdb::DatabaseWrapper *>(db);
		if (!wrapper || !wrapper->database) {
			registered.store(false);
			return;
		}
		auto &instance = *wrapper->database->instance;
		auto &fs = duckdb::FileSystem::GetFileSystem(instance);
		fs.RegisterSubSystem(duckdb::make_uniq<duckdb::WasmFileSystem>());
	} catch (const std::exception &e) {
		fprintf(stderr, "wasm_register_file_system failed: %s\n", e.what());
	} catch (...) {
		fprintf(stderr, "wasm_register_file_system failed: unknown error\n");
	}
}
