// Minimal AWS SDK provider-chain stub for wasm (see ../http/HttpRequest.h).
#pragma once

#include <aws/core/auth/AWSCredentialsProvider.h>

namespace Aws {
namespace Auth {

class AWSCredentialsProviderChain : public AWSCredentialsProvider {
public:
	~AWSCredentialsProviderChain() override = default;
};

} // namespace Auth
} // namespace Aws
