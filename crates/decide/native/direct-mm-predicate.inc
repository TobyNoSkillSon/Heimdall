// Eligibility of a MUL_MAT for kernel_mul_mm_heimdall_direct. Shared by the
// encoder and by the fusion table, so a fused bias/residual ADD is claimed only
// when the direct kernel (which applies it in its store) will run. Mirrors the
// branch order of ggml_metal_op_mul_mat: few-row MMA and mat-vec paths first.
extern "C" int heimdall_metal_direct_caps;  // bit 0: tensor matmul, bit 1: bfloat
static bool heimdall_mul_mm_direct_ok(const ggml_tensor * op) {
    if (op->op != GGML_OP_MUL_MAT || !(heimdall_metal_direct_caps & 1)) return false;
    const ggml_tensor * a = op->src[0];
    const ggml_tensor * b = op->src[1];
    if (op->type != GGML_TYPE_F32 || b->type != GGML_TYPE_F32) return false;
    if (a->type != GGML_TYPE_F16 && !(a->type == GGML_TYPE_BF16 && (heimdall_metal_direct_caps & 2))) return false;
    if (a->nb[0] != ggml_type_size(a->type) || b->nb[0] != sizeof(float) || !ggml_is_contiguous(op)) return false;
    if (a->ne[0] % 32 != 0 || a->nb[1] % 16 != 0 || b->nb[1] % 16 != 0) return false;
    if (b->ne[2] > INT16_MAX || b->ne[3] > INT16_MAX) return false;
    if (ggml_metal_op_mul_mat_use_mma(op)) return false;
    return ggml_metal_op_mul_mat_use_mm(op, true);
}
