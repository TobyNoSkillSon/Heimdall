#pragma once
#include <mutex>
// One mutex owns every native load, worker callback, sync, reset and teardown.
std::recursive_mutex & heimdall_native_mutex();
void * heimdall_current_owner();
// Question type per server slot (= sequence id). Set on the worker thread before
// filling a batch; a graph input uploads one value per token on every forward,
// including graph reuse, so slots with different question types share a batch.
void heimdall_decision_column(int seq, int column);
int heimdall_decision_column(int seq);
class heimdall_native_scope {
    std::unique_lock<std::recursive_mutex> lock;
    void * previous;
public:
    explicit heimdall_native_scope(void * owner);
    ~heimdall_native_scope();
};
