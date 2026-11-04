// non-causal within a sequence, the media never reads the text, so it is a function of the media alone
// in the head, the text and the media only read their own kind
// Heimdall: masks are built over the evaluated rows (heimdall_compact_rows); a
// shared media row belongs to every sequence that carries it.
class llm_graph_input_attn_media : public llm_graph_input_attn_no_cache {
public:
    llm_graph_input_attn_media(const llama_hparams & hparams, const llama_cparams & cparams, bool is_head, int64_t n_embd) :
        llm_graph_input_attn_no_cache(hparams, cparams), is_head(is_head), n_embd(n_embd) {}

    void set_input(const llama_ubatch * ubatch) override {
        const auto rows = heimdall_compact_rows(*ubatch, n_embd);
        const int64_t n = rows.keep.size();
        GGML_ASSERT(self_kq_mask->ne[0] == n && self_kq_mask->ne[1] == n);

        const auto fill_mask = [&](auto * data, auto zero, auto ninf) {
            for (int64_t i1 = 0; i1 < n; ++i1) {
                for (int64_t i0 = 0; i0 < n; ++i0) {
                    bool visible = (rows.seqs[i0] & rows.seqs[i1]) != 0;
                    if (is_head) {
                        visible = visible && rows.media[i0] == rows.media[i1];
                    } else {
                        visible = visible && !(rows.media[i1] && !rows.media[i0]);
                    }
                    data[i1 * n + i0] = visible ? zero : ninf;
                }
            }
        };

        GGML_ASSERT(ggml_backend_buffer_is_host(self_kq_mask->buffer));
        if (self_kq_mask->type == GGML_TYPE_F16) {
            fill_mask((ggml_fp16_t *) self_kq_mask->data, ggml_fp32_to_fp16(0.0f), ggml_fp32_to_fp16(-INFINITY));
        } else {
            fill_mask((float *) self_kq_mask->data, 0.0f, -INFINITY);
        }
    }

    const bool is_head;
    const int64_t n_embd;
};

// Centred conv neighbours of each evaluated row: the row at position -1/+1 that
// shares a sequence; the media never read the text. A single sequence yields
// the physical rows t-1/t+1 and the pinned 0/1 masks exactly. When there is no
// neighbour the mask is 0 and the index stays physical (or -1 at an edge), so
// the masked product is formed as before.
class heimdall_conv_rows final : public llm_graph_input_i {
public:
    explicit heimdall_conv_rows(int64_t n_embd) : n_embd(n_embd) {}
    void set_input(const llama_ubatch * ubatch) override {
        const auto rows = heimdall_compact_rows(*ubatch, n_embd);
        const int64_t n = rows.keep.size();
        GGML_ASSERT(left->ne[1] == n);
        std::map<std::pair<int64_t, int32_t>, int32_t> at;  // (position, sequence) -> row
        for (int64_t k = 0; k < n; ++k) {
            for (int32_t s = 0; s < 32; ++s) if (rows.seqs[k] & (1u << s)) at[{ubatch->pos[rows.keep[k]], s}] = (int32_t) k;
        }
        std::vector<float> data_left(n, 0.0f), data_right(n, 0.0f);
        std::vector<int32_t> rows_left(n), rows_right(n);
        for (int64_t k = 0; k < n; ++k) {
            const int64_t pos = ubatch->pos[rows.keep[k]];
            const int32_t s = __builtin_ctz(rows.seqs[k]);  // text rows have one sequence; shared media agree
            rows_left[k]  = k > 0 ? (int32_t) k - 1 : -1;
            rows_right[k] = k + 1 < n ? (int32_t) k + 1 : -1;
            auto l = at.find({pos - 1, s});
            if (l != at.end() && !(rows.media[k] && !rows.media[l->second])) {
                rows_left[k] = l->second;
                data_left[k] = 1.0f;
            }
            auto r = at.find({pos + 1, s});
            if (r != at.end() && !(rows.media[k] && !rows.media[r->second])) {
                rows_right[k] = r->second;
                data_right[k] = 1.0f;
            }
        }
        ggml_backend_tensor_set(left,  data_left.data(),  0, ggml_nbytes(left));
        ggml_backend_tensor_set(right, data_right.data(), 0, ggml_nbytes(right));
        ggml_backend_tensor_set(left_row,  rows_left.data(),  0, ggml_nbytes(left_row));
        ggml_backend_tensor_set(right_row, rows_right.data(), 0, ggml_nbytes(right_row));
    }

    const int64_t n_embd;
    ggml_tensor * left      = nullptr; // F32 [1, n]
    ggml_tensor * right     = nullptr; // F32 [1, n]
    ggml_tensor * left_row  = nullptr; // I32 [n]
    ggml_tensor * right_row = nullptr; // I32 [n]
};

// Evaluated rows, their positions and the output rows, when shared media rows
// were merged (otherwise the pinned position/output inputs are used).
class heimdall_rows_input final : public llm_graph_input_i {
public:
    explicit heimdall_rows_input(int64_t n_embd) : n_embd(n_embd) {}
    void set_input(const llama_ubatch * ubatch) override {
        const auto rows = heimdall_compact_rows(*ubatch, n_embd);
        GGML_ASSERT((int64_t) rows.keep.size() == keep->ne[0]);
        std::vector<int32_t> pos(rows.keep.size()), outs;
        for (size_t k = 0; k < rows.keep.size(); ++k) pos[k] = ubatch->pos[rows.keep[k]];
        for (uint32_t i = 0; i < ubatch->n_tokens; ++i) if (ubatch->output[i]) outs.push_back(rows.row_of[i]);
        GGML_ASSERT((int64_t) outs.size() == out_ids->ne[0]);
        ggml_backend_tensor_set(keep, rows.keep.data(), 0, ggml_nbytes(keep));
        ggml_backend_tensor_set(positions, pos.data(), 0, ggml_nbytes(positions));
        ggml_backend_tensor_set(out_ids, outs.data(), 0, ggml_nbytes(out_ids));
    }

    const int64_t n_embd;
    ggml_tensor * keep      = nullptr; // I32 [n]
    ggml_tensor * positions = nullptr; // I32 [n]
    ggml_tensor * out_ids   = nullptr; // I32 [n_outputs]
};

// Segments of a joint batch: shared media rows [0, shared) and each sequence's
// own rows. Attention runs per sequence over keys [shared | own], the key rows,
// order and mask of that sequence's own forward. Everything else is row-wise
// (matmuls, norms, the centred conv with explicit neighbours), so the joint
// forward reproduces the separate forwards. A layout that does not fit falls
// back to one masked attention over all rows.
struct heimdall_segments {
    int64_t shared = 0;
    std::vector<std::pair<int64_t, int64_t>> own;  // [begin, end) per sequence, in row order
    bool split() const { return own.size() > 1; }
    int64_t q_begin(size_t s) const { return s == 0 ? 0 : own[s].first; }
    std::vector<int64_t> keys(size_t s) const {
        std::vector<int64_t> k;
        for (int64_t i = 0; i < shared; ++i) k.push_back(i);
        for (int64_t i = own[s].first; i < own[s].second; ++i) k.push_back(i);
        if (s == 0) { k.clear(); for (int64_t i = 0; i < own[0].second; ++i) k.push_back(i); }
        return k;
    }
};
static heimdall_segments heimdall_segment_rows(const heimdall_rows & r) {
    const int64_t n = r.keep.size();
    heimdall_segments whole; whole.own.push_back({0, n});
    uint32_t all = 0;
    for (auto s : r.seqs) all |= s;
    heimdall_segments g;
    while (g.shared < n && r.seqs[g.shared] == all && __builtin_popcount(all) > 1) ++g.shared;
    uint32_t seen = 0;
    for (int64_t i = g.shared; i < n;) {
        const uint32_t s = r.seqs[i];
        if (__builtin_popcount(s) != 1 || (seen & s)) return whole;
        seen |= s;
        int64_t j = i;
        while (j < n && r.seqs[j] == s) ++j;
        g.own.push_back({i, j});
        i = j;
    }
    if (g.own.empty() || (seen | (g.shared ? all : 0)) != all) return whole;
    return g;
}

class heimdall_segment_masks final : public llm_graph_input_i {
public:
    heimdall_segment_masks(int64_t n_embd, bool head) : n_embd(n_embd), head(head) {}
    void set_input(const llama_ubatch * ubatch) override {
        const auto rows = heimdall_compact_rows(*ubatch, n_embd);
        const auto seg = heimdall_segment_rows(rows);
        GGML_ASSERT(seg.own.size() == masks.size());
        for (size_t s = 0; s < masks.size(); ++s) {
            const auto keys = seg.keys(s);
            const int64_t q0 = seg.q_begin(s), nq = seg.own[s].second - q0, nk = keys.size();
            ggml_tensor * m = masks[s];
            GGML_ASSERT(m->ne[0] == nk && m->ne[1] == nq && ggml_backend_buffer_is_host(m->buffer));
            const auto fill = [&](auto * data, auto zero, auto ninf) {
                for (int64_t i1 = 0; i1 < nq; ++i1) {
                    const int64_t q = q0 + i1;
                    for (int64_t i0 = 0; i0 < nk; ++i0) {
                        const int64_t k = keys[i0];
                        bool visible = (rows.seqs[k] & rows.seqs[q]) != 0;
                        visible = visible && (head ? rows.media[k] == rows.media[q] : !(rows.media[q] && !rows.media[k]));
                        data[i1 * nk + i0] = visible ? zero : ninf;
                    }
                }
            };
            if (m->type == GGML_TYPE_F16) {
                fill((ggml_fp16_t *) m->data, ggml_fp32_to_fp16(0.0f), ggml_fp32_to_fp16(-INFINITY));
            } else {
                fill((float *) m->data, 0.0f, -INFINITY);
            }
        }
    }
    const int64_t n_embd;
    const bool head;
    std::vector<ggml_tensor *> masks;  // per segment [n_keys, n_queries]
};

struct heimdall_joint_graph {
    heimdall_segments seg;
    std::vector<ggml_tensor *> trunk, head;
};
static thread_local const heimdall_joint_graph * heimdall_joint = nullptr;

static ggml_tensor * heimdall_attend(const llm_graph_context & g, const std::vector<ggml_tensor *> & masks,
        ggml_tensor * wo, ggml_tensor * wo_b, ggml_tensor * wo_s,
        ggml_tensor * q, ggml_tensor * k, ggml_tensor * v, float kq_scale, int il) {
    const auto & seg = heimdall_joint->seg;
    ggml_context * ctx = g.ctx0;
    const auto rows = [&](ggml_tensor * t, int64_t b, int64_t e) {
        return ggml_view_3d(ctx, t, t->ne[0], t->ne[1], e - b, t->nb[1], t->nb[2], b*t->nb[2]);
    };
    ggml_tensor * out = nullptr;
    for (size_t s = 0; s < seg.own.size(); ++s) {
        const int64_t b = seg.own[s].first, e = seg.own[s].second;
        ggml_tensor * qs = rows(q, seg.q_begin(s), e);
        ggml_tensor * ks = s == 0 || seg.shared == 0 ? rows(k, s == 0 ? 0 : b, e) : ggml_concat(ctx, rows(k, 0, seg.shared), rows(k, b, e), 2);
        ggml_tensor * vs = s == 0 || seg.shared == 0 ? rows(v, s == 0 ? 0 : b, e) : ggml_concat(ctx, rows(v, 0, seg.shared), rows(v, b, e), 2);
        ggml_tensor * cur = g.build_attn_mha(qs, ks, vs, nullptr, masks[s], nullptr, nullptr, 0, kq_scale, il);
        out = out ? ggml_concat(ctx, out, cur, 1) : cur;
    }
    if (wo) {
        out = g.build_lora_mm(wo, out, wo_s);
    }
    if (wo_b) {
        out = ggml_add(ctx, out, wo_b);
    }
    return out;
}

llama_model_lfm2::graph_decision::graph_decision(const llama_model & model, const llm_graph_params & params) :
    llm_graph_context(params) {
    const int64_t n_embd_head = hparams.n_embd_head_v();
    const int     n_layer_enc = n_layer - hparams.n_layer_decision;

    const auto rows = heimdall_compact_rows(ubatch, n_embd);
    const int64_t n_rows = rows.keep.size();

    ggml_tensor * cur = build_inp_embd(model.tok_embd);
    cb(cur, "model.embed_tokens", -1);

    ggml_tensor * inp_pos     = nullptr;
    ggml_tensor * inp_out_ids = nullptr;
    if (rows.compact()) {
        GGML_ASSERT(hparams.n_pos_per_embd() == 1);
        auto inp = std::make_unique<heimdall_rows_input>(n_embd);
        inp->keep      = ggml_new_tensor_1d(ctx0, GGML_TYPE_I32, n_rows);
        inp->positions = ggml_new_tensor_1d(ctx0, GGML_TYPE_I32, n_rows);
        inp->out_ids   = ggml_new_tensor_1d(ctx0, GGML_TYPE_I32, n_outputs);
        ggml_set_input(inp->keep);
        ggml_set_input(inp->positions);
        ggml_set_input(inp->out_ids);
        cur         = ggml_get_rows(ctx0, cur, inp->keep);
        inp_pos     = inp->positions;
        inp_out_ids = inp->out_ids;
        res->add_input(std::move(inp));
    } else {
        inp_pos     = build_inp_pos();
        inp_out_ids = build_inp_out_ids();
    }
    // build_qkv derives its effective row count from cur->ne[1]. The original
    // immutable batch count remains available to the full-batch input builders.

    const auto type_mask = cparams.flash_attn ? GGML_TYPE_F16 : GGML_TYPE_F32;

    heimdall_joint_graph joint;
    joint.seg = heimdall_segment_rows(rows);
    if (joint.seg.split()) {
        for (bool is_head : {false, true}) {
            auto inp = std::make_unique<heimdall_segment_masks>(n_embd, is_head);
            for (size_t s = 0; s < joint.seg.own.size(); ++s) {
                const int64_t nq = joint.seg.own[s].second - joint.seg.q_begin(s);
                const int64_t nk = joint.seg.keys(s).size();
                ggml_tensor * m = ggml_new_tensor_4d(ctx0, type_mask, nk, nq, 1, 1);
                ggml_set_input(m);
                inp->masks.push_back(m);
            }
            (is_head ? joint.head : joint.trunk) = inp->masks;
            res->add_input(std::move(inp));
        }
    }
    heimdall_joint = joint.seg.split() ? &joint : nullptr;

    llm_graph_input_attn_no_cache * inp_attn[2] = {nullptr, nullptr};
    for (bool is_head : {false, true}) {
        if (joint.seg.split()) break;  // per-sequence masks above
        auto inp = std::make_unique<llm_graph_input_attn_media>(hparams, cparams, is_head, n_embd);
        inp->self_kq_mask = ggml_new_tensor_4d(ctx0, type_mask, n_rows, n_rows, 1, 1);
        ggml_set_input(inp->self_kq_mask);
        inp->self_kq_mask_cnv = inp->self_kq_mask;
        inp_attn[is_head] = (llm_graph_input_attn_no_cache *) res->add_input(std::move(inp));
    }

    auto inp_conv = std::make_unique<heimdall_conv_rows>(n_embd);
    inp_conv->left      = ggml_new_tensor_2d(ctx0, GGML_TYPE_F32, 1, n_rows);
    inp_conv->right     = ggml_new_tensor_2d(ctx0, GGML_TYPE_F32, 1, n_rows);
    inp_conv->left_row  = ggml_new_tensor_1d(ctx0, GGML_TYPE_I32, n_rows);
    inp_conv->right_row = ggml_new_tensor_1d(ctx0, GGML_TYPE_I32, n_rows);
    for (auto * t : {inp_conv->left, inp_conv->right, inp_conv->left_row, inp_conv->right_row}) ggml_set_input(t);
    ggml_tensor * conv_left  = inp_conv->left;
    ggml_tensor * conv_right = inp_conv->right;
    ggml_tensor * row_left   = inp_conv->left_row;
    ggml_tensor * row_right  = inp_conv->right_row;
    res->add_input(std::move(inp_conv));

    for (int il = 0; il < n_layer_enc; ++il) {
        const auto & layer = model.layers[il];

        ggml_tensor * inpL = cur;
        cur = build_norm(cur, layer.attn_norm, NULL, LLM_NORM_RMS, il);
        cb(cur, "model.layers.{}.operator_norm", il);

        if (hparams.is_recr(il)) {
            ggml_tensor * bcx = build_lora_mm(layer.shortconv.in_proj, cur);
            cb(bcx, "model.layers.{}.conv.in_proj", il);

            // centred 3-tap conv and gate in one FP32 kernel (centered-conv.inc)
            GGML_ASSERT(hparams.n_shortconv_l_cache == 3);
            auto * gated = heimdall_centered(ctx0, bcx, layer.shortconv.conv, conv_left, conv_right, row_left, row_right);
            cur = build_lora_mm(layer.shortconv.out_proj, gated);
            cb(cur, "model.layers.{}.conv.out_proj", il);
        } else {
            auto [q, k, v] = build_qkv(layer, cur, n_embd_head, n_head, hparams.n_head_kv(il), il);

            q = build_norm(q, layer.attn_q_norm, NULL, LLM_NORM_RMS, il);
            k = build_norm(k, layer.attn_k_norm, NULL, LLM_NORM_RMS, il);

            q = ggml_rope_ext(ctx0, q, inp_pos, nullptr, n_rot, rope_type, n_ctx_orig, freq_base, freq_scale, ext_factor,
                              attn_factor, beta_fast, beta_slow);
            k = ggml_rope_ext(ctx0, k, inp_pos, nullptr, n_rot, rope_type, n_ctx_orig, freq_base, freq_scale, ext_factor,
                              attn_factor, beta_fast, beta_slow);

            cur = heimdall_joint
                ? heimdall_attend(*this, joint.trunk, layer.wo, NULL, layer.wo_s, q, k, v, 1.0f / sqrtf(float(n_embd_head)), il)
                : build_attn(inp_attn[0],
                    layer.wo, NULL, layer.wo_s,
                    q, k, v, nullptr, nullptr, nullptr, 1.0f / sqrtf(float(n_embd_head)), il);
            cb(cur, "model.layers.{}.self_attn.out_proj", il);
        }

        cur = ggml_add(ctx0, cur, inpL);

        ggml_tensor * ffn_out = build_norm(cur, layer.ffn_norm, NULL, LLM_NORM_RMS, il);
        ffn_out = build_ffn(ffn_out,
                layer.ffn_up,   NULL, NULL,
                layer.ffn_gate, NULL, NULL,
                layer.ffn_down, NULL, NULL,
                NULL, LLM_FFN_SILU, LLM_FFN_PAR, il);

        cur = ggml_add(ctx0, cur, ffn_out);
        cb(cur, "l_out", il);
    }

    cur = build_norm(cur, model.output_norm, NULL, LLM_NORM_RMS, -1);
    cb(cur, "result_norm", -1);

    cur = build_decision_head(model, cur, inp_attn[1], inp_out_ids);
    heimdall_joint = nullptr;

    res->t_embd = cur;
    ggml_build_forward_expand(gf, cur);
}
