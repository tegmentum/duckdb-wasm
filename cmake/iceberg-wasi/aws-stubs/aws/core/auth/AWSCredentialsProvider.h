// Minimal AWS SDK auth-provider stub for wasm (see ../http/HttpRequest.h).
#pragma once

#include <aws/core/auth/AWSCredentials.h>

namespace Aws {
namespace Auth {

class AWSCredentialsProvider {
public:
	virtual ~AWSCredentialsProvider() = default;
	virtual AWSCredentials GetAWSCredentials() {
		return {};
	}
};

} // namespace Auth
} // namespace Aws
