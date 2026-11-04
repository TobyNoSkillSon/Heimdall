// Owned d1 centred convolution. Keep every FP32 multiplication/addition boundary
// of the pinned ggml graph (including masked neighbours) and its CPU recovery.
// Neighbours are explicit row indices (-1: none). A single sequence passes the
// physical rows t-1/t+1, which reproduces the pinned arithmetic bit for bit; a
// joint batch that evaluates shared media once points text at its media row.
static void heimdall_centered_cpu(ggml_tensor * dst, int ith, int nth, void *) {
    const auto * a = dst->src[0];
    const auto * w = dst->src[1];
    const auto * left = static_cast<const float *>(dst->src[2]->data);
    const auto * right = static_cast<const float *>(dst->src[3]->data);
    const auto * li = static_cast<const int32_t *>(dst->src[4]->data);
    const auto * ri = static_cast<const int32_t *>(dst->src[5]->data);
    const int64_t h = dst->ne[0], n = dst->ne[1];
    auto row = [&](int64_t t) { return reinterpret_cast<const float *>(static_cast<const char *>(a->data) + t*a->nb[1]); };
    auto weight = [&](int64_t d, int tap) { return *reinterpret_cast<const float *>(static_cast<const char *>(w->data) + d*w->nb[1] + tap*w->nb[0]); };
    auto * out = static_cast<float *>(dst->data);
    for (int64_t i = ith; i < h*n; i += nth) {
        const auto t = i/h, d = i%h;
        const float * x = row(t);
        const float bx = x[d]*x[2*h+d];
        const float prv = li[t] >= 0 ? row(li[t])[d]*row(li[t])[2*h+d] : 0.0f;
        const float nxt = ri[t] >= 0 ? row(ri[t])[d]*row(ri[t])[2*h+d] : 0.0f;
        const float middle = bx*weight(d, 1);
        const float lp = prv*weight(d, 0);
        const float rp = nxt*weight(d, 2);
        const float l = lp*left[t], r = rp*right[t];
        const float first = middle+l;
        const float y = first+r;
        out[i] = x[h+d]*y;
    }
}
static ggml_tensor * heimdall_centered(ggml_context * ctx, ggml_tensor * bcx, ggml_tensor * w, ggml_tensor * left, ggml_tensor * right,
        ggml_tensor * left_row, ggml_tensor * right_row) {
    GGML_ASSERT(bcx->type == GGML_TYPE_F32 && w->type == GGML_TYPE_F32);
    GGML_ASSERT(bcx->ne[0] % 3 == 0 && w->ne[0] == 3 && w->ne[1] == bcx->ne[0]/3);
    GGML_ASSERT(left_row->type == GGML_TYPE_I32 && right_row->type == GGML_TYPE_I32);
    ggml_tensor * args[] = {bcx, w, left, right, left_row, right_row};
    auto * out = ggml_custom_4d(ctx, GGML_TYPE_F32, bcx->ne[0]/3, bcx->ne[1], 1, 1, args, 6, heimdall_centered_cpu, GGML_N_TASKS_MAX, nullptr);
    ggml_set_name(out, "heimdall.centered.fp32");
    return out;
}
