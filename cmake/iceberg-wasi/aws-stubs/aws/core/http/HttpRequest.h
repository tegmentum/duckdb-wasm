// Minimal AWS SDK type stubs for building duckdb-iceberg on wasm32-wasip2.
//
// The AWS C++ SDK does not build for wasm. duckdb-iceberg's REST-catalog code is
// pervasively AWS-typed (Aws::Http::HttpMethod/URI/Scheme + the credentials/HTTP
// client machinery), but its DEFAULT request path (AWSInput::ExecuteRequest with
// the `iceberg_via_aws_sdk_for_catalog_interactions` setting off, which is the
// default) signs requests with SigV4 by hand (mbedtls) and issues them through
// DuckDB's own HTTPUtil (curl) -- no AWS SDK. The AWS SDK is only used by the
// opt-in legacy path (CreateSignedRequest / ExecuteRequestLegacy), which the
// build patch compiles out under __wasi__.
//
// So these stubs only need to provide: the HttpMethod/Scheme enums + the name
// mapper, a real-enough URI (the default path builds + reads it), and otherwise
// just enough type surface for the (compiled-out) legacy declarations to parse.
#pragma once

#include <cstdint>
#include <map>
#include <memory>
#include <sstream>
#include <string>
#include <utility>
#include <vector>

namespace Aws {
namespace Http {

enum class Scheme { HTTP, HTTPS };

enum class HttpMethod {
	HTTP_GET,
	HTTP_POST,
	HTTP_DELETE,
	HTTP_PUT,
	HTTP_HEAD,
	HTTP_PATCH
};

struct HttpMethodMapper {
	static const char *GetNameForHttpMethod(HttpMethod method) {
		switch (method) {
		case HttpMethod::HTTP_GET:
			return "GET";
		case HttpMethod::HTTP_POST:
			return "POST";
		case HttpMethod::HTTP_DELETE:
			return "DELETE";
		case HttpMethod::HTTP_PUT:
			return "PUT";
		case HttpMethod::HTTP_HEAD:
			return "HEAD";
		case HttpMethod::HTTP_PATCH:
			return "PATCH";
		default:
			return "GET";
		}
	}
};

enum class HttpResponseCode : int { REQUEST_NOT_MADE = -1 };

// A minimal AWS-compatible URI. The iceberg default path builds it (SetScheme,
// SetAuthority, AddPathSegment, AddQueryStringParameter) and reads it back
// (GetAuthority, GetURLEncodedPath, GetQueryString, GetURIString) to assemble the
// SigV4 canonical request + the final request URL. Query parameters are kept in
// insertion order (AWS sorts them in the canonical request; iceberg currently
// adds at most one, so ordering is not yet observable here).
class URI {
public:
	void SetScheme(Scheme s) {
		scheme_ = s;
	}
	Scheme GetScheme() const {
		return scheme_;
	}
	void SetAuthority(const std::string &authority) {
		authority_ = authority;
	}
	const std::string &GetAuthority() const {
		return authority_;
	}
	void AddPathSegment(const std::string &segment) {
		segments_.push_back(segment);
	}
	void AddQueryStringParameter(const char *key, const char *value) {
		query_.emplace_back(key ? key : "", value ? value : "");
	}

	// Raw (un-encoded) "/a/b/c" path.
	std::string GetPath() const {
		std::string path;
		for (auto &seg : segments_) {
			path += "/";
			path += seg;
		}
		return path.empty() ? "/" : path;
	}

	// Percent-encoded path: each segment RFC-3986-encoded, joined with '/'.
	std::string GetURLEncodedPath() const {
		std::string path;
		for (auto &seg : segments_) {
			path += "/";
			path += Encode(seg);
		}
		return path.empty() ? "/" : path;
	}

	// "?k1=v1&k2=v2" (encoded), or "" when there are no parameters. The iceberg
	// code checks .size() and uses .substr(1), so the leading '?' is included.
	std::string GetQueryString() const {
		if (query_.empty()) {
			return "";
		}
		std::string out = "?";
		bool first = true;
		for (auto &kv : query_) {
			if (!first) {
				out += "&";
			}
			first = false;
			out += Encode(kv.first);
			out += "=";
			out += Encode(kv.second);
		}
		return out;
	}

	std::string GetURIString() const {
		std::string out = (scheme_ == Scheme::HTTPS) ? "https://" : "http://";
		out += authority_;
		out += GetURLEncodedPath();
		out += GetQueryString();
		return out;
	}

private:
	static std::string Encode(const std::string &in) {
		static const char *hex = "0123456789ABCDEF";
		std::string out;
		for (unsigned char c : in) {
			if ((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-' || c == '_' ||
			    c == '.' || c == '~') {
				out += static_cast<char>(c);
			} else {
				out += '%';
				out += hex[(c >> 4) & 0xF];
				out += hex[c & 0xF];
			}
		}
		return out;
	}

	Scheme scheme_ = Scheme::HTTPS;
	std::string authority_;
	std::vector<std::string> segments_;
	std::vector<std::pair<std::string, std::string>> query_;
};

// Only referenced from the legacy (AWS-SDK) path that the build patch compiles
// out under __wasi__; kept as an opaque-enough type so the declarations parse.
class HttpRequest {
public:
	const URI &GetUri() const {
		return uri_;
	}
	std::map<std::string, std::string> GetHeaders() const {
		return {};
	}
	void SetUserAgent(const std::string &) {
	}
	void SetContentLength(const std::string &) {
	}
	void SetHeaderValue(const std::string &, const std::string &) {
	}
	template <typename T>
	void AddContentBody(const T &) {
	}

private:
	URI uri_;
};

} // namespace Http
} // namespace Aws
