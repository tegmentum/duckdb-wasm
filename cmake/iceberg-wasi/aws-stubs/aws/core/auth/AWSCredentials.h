// Minimal AWS SDK auth stub for wasm (see ../http/HttpRequest.h for the rationale).
// Only referenced from the AWS-SDK legacy path, which the build patch compiles out
// under __wasi__; kept just complete enough that declarations parse.
#pragma once

#include <string>

namespace Aws {
namespace Auth {

class AWSCredentials {
public:
	void SetAWSAccessKeyId(const std::string &v) {
		access_key_id = v;
	}
	void SetAWSSecretKey(const std::string &v) {
		secret_key = v;
	}
	void SetSessionToken(const std::string &v) {
		session_token = v;
	}
	const std::string &GetAWSAccessKeyId() const {
		return access_key_id;
	}
	const std::string &GetAWSSecretKey() const {
		return secret_key;
	}
	const std::string &GetSessionToken() const {
		return session_token;
	}

private:
	std::string access_key_id;
	std::string secret_key;
	std::string session_token;
};

} // namespace Auth
} // namespace Aws
