// Unquantized F16/BF16 weights x F32 activations on the tensor units, with the
// weight tile read straight from device memory instead of staged through
// threadgroup memory 32 columns at a time. Same matmul2d descriptor (operand
// order, transposes, relaxed precision, FP32 accumulation) as kernel_mul_mm:
// outputs are bit-identical on the d1 shapes; only the tiling changes. A
// following bias/residual ADD can be applied in the store (same FP32 add).
// Template/layout: llama.cpp kernel_mul_mm, MIT.
#ifdef GGML_METAL_HAS_TENSOR
// 0: plain; 1: + bias[row]; 2: + same-shape residual (a fused following ADD).
constant short FC_heimdall_epi [[function_constant(FC_MUL_MM + 8)]];
template<typename T0, int NRB, int NSG>
kernel void kernel_mul_mm_heimdall_direct(
        constant ggml_metal_kargs_mul_mm & args,
        device const char * srcA,
        device const char * srcB,
        device       char * dst,
        device const float * add [[buffer(4)]],
        uint3 tgpig [[threadgroup_position_in_grid]]) {
    constexpr int NRA = 64;
    const int K = args.ne00;
    const int M = args.ne0;
    const int N = args.ne1;
    const int im  = tgpig.z;
    const int i12 = im % FC_mul_mm_ne12;
    const int i13 = im / FC_mul_mm_ne12;
    const int ra = tgpig.y * NRA;
    const int rb = tgpig.x * NRB;
    const uint64_t offset0 = (i12/FC_mul_mm_r2)*args.nb02 + (i13/FC_mul_mm_r3)*args.nb03;
    device T0    * ptrA = (device T0    *)(srcA + offset0 + args.nb01*(uint64_t) ra);
    device float * ptrB = (device float *)(srcB + args.nb12*i12 + args.nb13*i13 + args.nb11*(uint64_t) rb);
    auto tA = tensor(ptrA, dextents<int32_t, 2>(K, M - ra), array<int, 2>({1, (int)(args.nb01/sizeof(T0))}));
    auto tB = tensor(ptrB, dextents<int32_t, 2>(K, N - rb), array<int, 2>({1, (int)(args.nb11/sizeof(float))}));
    mpp::tensor_ops::matmul2d<
        mpp::tensor_ops::matmul2d_descriptor(
            NRB, NRA, static_cast<int>(dynamic_extent), false, true, true,
            mpp::tensor_ops::matmul2d_descriptor::mode::multiply_accumulate),
        execution_simdgroups<NSG>> mm;
    auto cT = mm.template get_destination_cooperative_tensor<decltype(tB), decltype(tA), float>();
    mm.run(tB, tA, cT);
    if (FC_heimdall_epi != 0) {
        // FP32 a + b, the separate ADD kernel's arithmetic (index 0: row, 1: token)
        device const float * addBatch = add + (FC_heimdall_epi == 2 ? im*N*M : 0);
        for (short i = 0; i < cT.get_capacity(); ++i) {
            if (!cT.is_valid_element(i)) continue;
            const auto at = cT.get_multidimensional_index(i);
            const int m = ra + at[0], n = rb + at[1];
            if (m < M && n < N) cT[i] += FC_heimdall_epi == 1 ? addBatch[m] : addBatch[n*M + m];
        }
    }
    device float * dstBatch = (device float *)dst + im*N*M;
    auto tD = tensor(dstBatch, dextents<int32_t, 2>(M, N), array<int, 2>({1, M}));
    cT.store(tD.slice(ra, rb));
}
typedef decltype(kernel_mul_mm_heimdall_direct<half, 32, 4>) heimdall_direct_t;
template [[host_name("kernel_mul_mm_heimdall_direct_f16_32_4")]] kernel heimdall_direct_t kernel_mul_mm_heimdall_direct<half, 32, 4>;
template [[host_name("kernel_mul_mm_heimdall_direct_f16_64_4")]] kernel heimdall_direct_t kernel_mul_mm_heimdall_direct<half, 64, 4>;
template [[host_name("kernel_mul_mm_heimdall_direct_f16_32_2")]] kernel heimdall_direct_t kernel_mul_mm_heimdall_direct<half, 32, 2>;
template [[host_name("kernel_mul_mm_heimdall_direct_f16_64_2")]] kernel heimdall_direct_t kernel_mul_mm_heimdall_direct<half, 64, 2>;
#if defined(GGML_METAL_HAS_BF16)
template [[host_name("kernel_mul_mm_heimdall_direct_bf16_32_4")]] kernel heimdall_direct_t kernel_mul_mm_heimdall_direct<bfloat, 32, 4>;
template [[host_name("kernel_mul_mm_heimdall_direct_bf16_64_4")]] kernel heimdall_direct_t kernel_mul_mm_heimdall_direct<bfloat, 64, 4>;
template [[host_name("kernel_mul_mm_heimdall_direct_bf16_32_2")]] kernel heimdall_direct_t kernel_mul_mm_heimdall_direct<bfloat, 32, 2>;
template [[host_name("kernel_mul_mm_heimdall_direct_bf16_64_2")]] kernel heimdall_direct_t kernel_mul_mm_heimdall_direct<bfloat, 64, 2>;
#endif
#endif
