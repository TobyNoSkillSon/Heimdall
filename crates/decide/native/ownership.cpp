#include "ownership.h"
struct server_tokens;
static thread_local server_tokens * prepared_media = nullptr;
server_tokens * heimdall_prepared_media() { return prepared_media; }
void heimdall_prepared_media(server_tokens * tokens) { prepared_media = tokens; }
static thread_local void * current_owner = nullptr;
#include <array>
static thread_local std::array<int, 4> decision_column{};
void heimdall_decision_column(int seq, int column) { decision_column.at(seq) = column; }
int heimdall_decision_column(int seq) { return decision_column.at(seq); }
std::recursive_mutex & heimdall_native_mutex() {
    static std::recursive_mutex mutex;
    return mutex;
}
void * heimdall_current_owner() { return current_owner; }
heimdall_native_scope::heimdall_native_scope(void * owner)
    : lock(heimdall_native_mutex()), previous(current_owner) { current_owner = owner; }
heimdall_native_scope::~heimdall_native_scope() { current_owner = previous; }
