// Minimal AWS SDK core stubs for wasm (see http/HttpRequest.h for the rationale).
#pragma once

#include <memory>
#include <sstream>
#include <string>

namespace Aws {

struct SDKOptions {};

// The real InitAPI is only reached from the AWS-SDK legacy path, which the build
// patch compiles out under __wasi__; provide a no-op so any stray reference links.
inline void InitAPI(const SDKOptions &) {
}

using StringStream = std::stringstream;

template <typename T, typename... Args>
std::shared_ptr<T> MakeShared(const char *, Args &&...args) {
	return std::make_shared<T>(std::forward<Args>(args)...);
}

namespace Client {

// Returned by AWSInput::BuildClientConfig (defined unconditionally; only *called*
// from the compiled-out legacy path). Just needs the fields the code sets.
struct ClientConfiguration {
	std::string caFile;
	long requestTimeoutMs = 0;
	long httpRequestTimeoutMs = 0;
};

} // namespace Client
} // namespace Aws
