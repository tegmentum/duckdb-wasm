// Minimal AWS SDK HTTP-client stub for wasm. The real client is only used by the
// AWS-SDK legacy path, which the build patch compiles out under __wasi__, so this
// just needs to exist for the #include.
#pragma once

#include <aws/core/http/HttpRequest.h>
