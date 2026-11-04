// The omni encoder is bidirectional: its text prefix cannot be KV-cached.
// The three independently conditioned heads can, however, be selected at runtime.
// Keep the original three-column ABI by repeating the single requested score.
// Only the requested column is consumed. Each token carries its own slot's type.
// A tensor input (rather than a graph construction constant) preserves correct
// same-shape graph reuse across types.
#include "ownership.h"
#include "ggml-backend.h"
#include <cstring>
#include <map>
#include <vector>

// Rows of a decision ubatch that are evaluated. With two slots (two questions
// over one audio clip) the batch carries the same media once per slot: identical
// embedding bytes at identical positions. Media never read text (non-causal
// trunk mask and conv), so such rows are equal in every layer and are evaluated
// once, shared by both sequences. Text rows are never merged. Without a shared
// media prefix this is the identity: every row kept, one sequence per row.
struct heimdall_rows {
    std::vector<int32_t>  keep;    // ubatch row of each evaluated row
    std::vector<int32_t>  row_of;  // ubatch row -> evaluated row
    std::vector<uint32_t> seqs;    // per evaluated row: bit set of its sequences
    std::vector<uint8_t>  media;   // per evaluated row
    bool compact() const { return keep.size() < row_of.size(); }
};
static bool heimdall_is_media(const llama_ubatch & ub, int64_t i) {
    return ub.is_mixed() ? ub.type[i] != 0 : ub.token == nullptr;
}
static heimdall_rows heimdall_compact_rows(const llama_ubatch & ub, int64_t n_embd) {
    heimdall_rows r;
    const int64_t n = ub.n_tokens;
    r.row_of.resize(n);
    std::map<int64_t, int32_t> media_at;  // position -> first evaluated media row
    for (int64_t i = 0; i < n; ++i) {
        const bool media = heimdall_is_media(ub, i);
        const int32_t seq = ub.seq_id[i][0];
        GGML_ASSERT(ub.n_seq_id[i] == 1 && seq >= 0 && seq < 32);
        if (media && ub.embd) {
            auto hit = media_at.find(ub.pos[i]);
            if (hit != media_at.end()) {
                const int32_t j = hit->second;
                const int64_t src = r.keep[j];
                if (!(r.seqs[j] & (1u << seq)) &&
                    std::memcmp(ub.embd + i*n_embd, ub.embd + src*n_embd, n_embd*sizeof(float)) == 0) {
                    r.seqs[j] |= 1u << seq;
                    r.row_of[i] = j;
                    continue;
                }
            } else {
                media_at.emplace(ub.pos[i], (int32_t) r.keep.size());
            }
        }
        r.row_of[i] = (int32_t) r.keep.size();
        r.keep.push_back((int32_t) i);
        r.seqs.push_back(1u << seq);
        r.media.push_back(media);
    }
    return r;
}

class heimdall_head_input final : public llm_graph_input_i {
public:
    ggml_tensor * column;
    const int64_t n_embd;
    heimdall_head_input(ggml_tensor * value, int64_t n_embd) : column(value), n_embd(n_embd) {}
    void set_input(const llama_ubatch * ubatch) override {
        const auto rows = heimdall_compact_rows(*ubatch, n_embd);
        GGML_ASSERT((int64_t) rows.keep.size() == column->ne[0]);
        std::vector<int32_t> values(rows.keep.size());
        for (size_t k = 0; k < rows.keep.size(); ++k) {
            values[k] = heimdall_decision_column(ubatch->seq_id[rows.keep[k]][0]);
            GGML_ASSERT(values[k] >= 0 && values[k] < 3);
        }
        ggml_backend_tensor_set(column, values.data(), 0, values.size()*sizeof(int32_t));
    }
    bool can_reuse(const llm_graph_params &) override { return true; }
};
