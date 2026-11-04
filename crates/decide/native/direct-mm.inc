// Unquantized F16/BF16 x F32 tensor-unit matmul without threadgroup staging
// (see direct-mm.metal). Narrower token tiles keep the GPU occupied on d1's
// short sequences; the arithmetic per output element is unchanged.
static int heimdall_mul_mm_direct(ggml_metal_op_t ctx, int idx) {
    ggml_tensor * op = ctx->node(idx);
    if (!heimdall_mul_mm_direct_ok(op)) return 0;
    const ggml_tensor * a = op->src[0];
    const ggml_tensor * b = op->src[1];
    // A following bias (row vector) or same-shape residual ADD is added in the
    // store, in FP32 exactly as the separate ADD kernel would.
    const int n_fuse = ggml_metal_op_try_fusion(ctx, idx, GGML_METAL_FUSION_MUL_MAT_ADD);
    const ggml_tensor * dst = op;
    const ggml_tensor * x = nullptr;
    int16_t epi = 0;
    if (n_fuse > 1) {
        dst = ctx->node(idx + n_fuse - 1);
        x = dst->src[0] == op ? dst->src[1] : dst->src[0];
        epi = ggml_nelements(x) == op->ne[0] ? 1 : 2;
    }
    const int64_t M = op->ne[0], N = op->ne[1], batches = op->ne[2]*op->ne[3];
    // Measured on M5 Max over the 600M/3B/Conformer shapes (pass-2 report):
    // within ~6% of the best of 30 tile/simdgroup variants on every shape.
    const int64_t rows = (M + 63)/64 * batches;
    const int64_t t32 = rows*((N + 31)/32), t64 = rows*((N + 63)/64);
    const int nrb = t32 <= 192 ? 32 : t64 <= 192 ? 64 : t32 <= 480 ? 32 : 64;
    const int nsg = t32 <= 192 || t64 <= 192 ? 4 : 2;
    char base[96], name[160];
    snprintf(base, sizeof(base), "kernel_mul_mm_heimdall_direct_%s_%d_%d", a->type == GGML_TYPE_F16 ? "f16" : "bf16", nrb, nsg);
    const int16_t ne12 = b->ne[2], ne13 = b->ne[3], r2 = ne12/a->ne[2], r3 = ne13/a->ne[3];
    snprintf(name, sizeof(name), "%s_ne12=%d_ne13=%d_r2=%d_r3=%d_epi=%d", base, ne12, ne13, r2, r3, epi);
    auto pipeline = ggml_metal_library_get_pipeline(ctx->lib, name);
    if (!pipeline.pipeline) {
        ggml_metal_cv_t cv = ggml_metal_cv_init();
        ggml_metal_cv_set_int16(cv, ne12, FC_MUL_MM + 2);
        ggml_metal_cv_set_int16(cv, ne13, FC_MUL_MM + 3);
        ggml_metal_cv_set_int16(cv, r2,   FC_MUL_MM + 4);
        ggml_metal_cv_set_int16(cv, r3,   FC_MUL_MM + 5);
        ggml_metal_cv_set_int16(cv, epi,  FC_MUL_MM + 8);
        pipeline = ggml_metal_library_compile_pipeline(ctx->lib, base, name, cv);
        ggml_metal_cv_free(cv);
    }
    GGML_ASSERT(pipeline.pipeline && "Heimdall direct matmul pipeline");
    ggml_metal_kargs_mul_mm args = {
        /*.ne00 =*/ (int32_t) a->ne[0], /*.ne02 =*/ (int32_t) a->ne[2],
        /*.nb01 =*/ a->nb[1], /*.nb02 =*/ a->nb[2], /*.nb03 =*/ a->nb[3],
        /*.ne12 =*/ (int32_t) b->ne[2],
        /*.nb10 =*/ b->nb[0], /*.nb11 =*/ b->nb[1], /*.nb12 =*/ b->nb[2], /*.nb13 =*/ b->nb[3],
        /*.ne0  =*/ (int32_t) op->ne[0], /*.ne1  =*/ (int32_t) op->ne[1],
        /*.r2   =*/ r2, /*.r3   =*/ r3,
    };
    ggml_metal_encoder_set_pipeline(ctx->enc, pipeline);
    ggml_metal_encoder_set_bytes   (ctx->enc, &args, sizeof(args), 0);
    ggml_metal_encoder_set_buffer  (ctx->enc, ggml_metal_get_buffer_id(a),  1);
    ggml_metal_encoder_set_buffer  (ctx->enc, ggml_metal_get_buffer_id(b),  2);
    ggml_metal_encoder_set_buffer  (ctx->enc, ggml_metal_get_buffer_id(dst), 3);
    ggml_metal_encoder_set_buffer  (ctx->enc, ggml_metal_get_buffer_id(x ? x : dst), 4);
    ggml_metal_encoder_dispatch_threadgroups(ctx->enc, (N + nrb - 1)/nrb, (M + 63)/64, batches, 32, nsg, 1);
    return n_fuse;
}
