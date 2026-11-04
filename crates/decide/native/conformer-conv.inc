// FP32 tensor GEMM for the two 1x1 FP32-weight convolutions. See pointwise.metal.
static ggml_tensor * heimdall_pointwise_conv(ggml_context * ctx, ggml_tensor * a,
        ggml_tensor * b, int s0, int s1, int p0, int p1, int d0, int d1) {
    if ((getenv("HEIMDALL_D1_CONV") && std::string(getenv("HEIMDALL_D1_CONV")) == "direct") ||
            a->type != GGML_TYPE_F32 || a->ne[0] != 1 || a->ne[1] != 1) {
        return ggml_conv_2d_direct(ctx,a,b,s0,s1,p0,p1,d0,d1);
    }
    auto * patches=ggml_im2col(ctx,a,b,s0,s1,p0,p1,d0,d1,true,GGML_TYPE_F32);
    const auto rows=patches->ne[1]*patches->ne[2]*patches->ne[3];
    auto * w=ggml_reshape_2d(ctx,a,patches->ne[0],a->ne[3]);
    auto * result=ggml_mul_mat(ctx,ggml_reshape_2d(ctx,patches,patches->ne[0],rows),w);
    ggml_set_name(result,"heimdall.pointwise.fp32");
    result=ggml_reshape_4d(ctx,result,patches->ne[1],patches->ne[2],patches->ne[3],a->ne[3]);
    result=ggml_cont(ctx,ggml_permute(ctx,result,0,1,3,2));
    return result;
}
