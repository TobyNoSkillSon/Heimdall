#pragma once
#include "ownership.h"
#include "server-common.h"
#include <map>
#include <vector>

server_tokens * heimdall_prepared_media();
void heimdall_prepared_media(server_tokens * tokens);
using heimdall_media_embeddings = std::map<std::string, std::vector<float>>;
// Preparation runs on the caller, encoding on the worker. The native ownership
// mutex protects the shared embeddings; only the caller uses the token pointer.
// A scope per request excludes cache hits across audio, handles and retries.
class heimdall_request_media {
    server_tokens tokens;
    server_tokens * previous;
    heimdall_media_embeddings & embeddings;
    void * owner;
public:
    heimdall_request_media(heimdall_media_embeddings & cache, void * handle)
        : previous(heimdall_prepared_media()), embeddings(cache), owner(handle) {
        embeddings.clear();
        heimdall_prepared_media(&tokens);
    }
    ~heimdall_request_media() {
        heimdall_prepared_media(previous);
        heimdall_native_scope ownership(owner);
        embeddings.clear();
    }
};
