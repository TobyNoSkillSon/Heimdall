// vk-formats27: one matmul2d tile structure per regime, instantiated for every operand format.
// Only the operand element types (and the optional MX block-scale planes) differ between variants.
//
//   tile_<SG>_<variant>   small M (decode, speech encoders): 32x32 output tile per threadgroup, K split over SG
//                         simdgroups in 32-wide steps, float reduction in threadgroup memory (SmallMGEMM / naxint shape)
//   big_<cfg>_<variant>   large M (DiT, Whisper encoder, peak): WMxWN simdgroups, each owns an SMxSN output sub-tile and
//                         loops over the full K in KT steps (MLX steel_gemm_fused_nax geometry on g17s: 64x128, 8 SG)
//   alu_<variant>         register-resident tensor-unit rate: operands loaded once into cooperative tensors, then R
//                         back-to-back matmul2d runs; no memory traffic in the loop
//
// Operands: A = activations [M][K] (K contiguous), B = weights [N][K] (K contiguous, transpose_right).
// MX planes: tensor_blockwise<tensor_plane_scales, ue8m0, 32, 1> = one E8M0 byte per 32 K-elements, stored [rows][K/32].
// Epilogue: out[m][n] = acc * (flags&1 ? rs[m] : 1) * (flags&2 ? cs[n] : 1), stored as half.
//
// Language: Metal 4.0 (macOS 26) builds the 16-bit / int8 / int4 variants; Metal 4.1 (macOS 27; implies deployment
// target 27.0 and the _v2 tensor-op entry points) adds FP8 E4M3/E5M2, FP4 E2M1 and the scale planes.
#include <metal_stdlib>
#include <metal_tensor>
#include <MetalPerformancePrimitives/MetalPerformancePrimitives.h>
using namespace metal;
using namespace mpp::tensor_ops;

#if __METAL_VERSION__ >= 410
#define F27 1
#if !__TENSOR_OPS_SUPPORT_DEPLOYMENT_TARGET_27_0
#error "vk-formats27: Metal 4.1 without deployment target 27.0 would take the v1 tensor-op entry points"
#endif
#else
#define F27 0
#endif

template <typename T> struct Store { using type = T; };
template <> struct Store<int4b_format> { using type = uchar; };
#if F27
template <> struct Store<metal_fp8_e4m3_format> { using type = uchar; };
template <> struct Store<metal_fp8_e5m2_format> { using type = uchar; };
template <> struct Store<metal_fp4_e2m1_format> { using type = uchar; };
#endif

// Rank-2 operand view over a whole [rows][K] array, optionally with its MX scale plane.
template <typename T, bool SC> struct Operand {
  using type = tensor<device T, dextents<int32_t, 2>, tensor_inline>;
  static type make(device uchar* p, int K, int rows, device uchar*) {
    return type((device typename Store<T>::type*)p, dextents<int32_t, 2>(K, rows), array<int, 2>{1, K});
  }
};
#if F27
using MXPlane = tensor_blockwise<tensor_plane_scales, device metal_fp8_ue8m0_format, 32, 1>;
template <typename T> struct Operand<T, true> {
  using type = tensor<device T, dextents<int32_t, 2>, tensor_inline, MXPlane>;
  static type make(device uchar* p, int K, int rows, device uchar* sc) {
    return type((device typename Store<T>::type*)p, dextents<int32_t, 2>(K, rows), array<int, 2>{1, K}, MXPlane(sc));
  }
};
#endif

template <typename AT> inline float epi(AT v, int m, int n, int fl, device const float* rs, device const float* cs) {
  float r = static_cast<float>(v);
  if (fl & 1) r *= rs[m];
  if (fl & 2) r *= cs[n];
  return r;
}

// Heimdall epilogues preserve the original materialized FP16 boundaries.
// Bias/relu and NCHW stores remove copies without changing the dot-product tree.
template <typename AT> inline half heimdall_epi(AT v, int m, int n, int fl,
    device const float* rs, device const float* cs, device const half* bias) {
  half r = half(epi(v, m, n, fl, rs, cs));
  if (fl & 8) r = half(float(r) + float(bias[n]));
  if (fl & 16) r = max(r, half(0));
  return r;
}

// ------------------------------------------------------------------------------------------------ small M: split-K tile
template <typename LT, typename RT, typename AT, bool LS, bool RS, int SG>
kernel void tile(device uchar* x [[buffer(0)]], device uchar* w [[buffer(1)]],
                 device uchar* xs [[buffer(2)]], device uchar* ws [[buffer(3)]],
                 device const float* rs [[buffer(4)]], device const float* cs [[buffer(5)]],
                 device half* out [[buffer(6)]], constant int* dims [[buffer(7)]],
                 device const half* bias [[buffer(8)]],
                 uint3 tgp [[threadgroup_position_in_grid]], uint sgi [[simdgroup_index_in_threadgroup]],
                 uint tid [[thread_index_in_threadgroup]]) {
  const int M = dims[0], K = dims[1], N = dims[2], fl = dims[3];
  // Heimdall: independent batch planes, same Vella tile arithmetic.
  x += tgp.z * M * K * 2; w += (tgp.z % uint(dims[5])) * N * K * 2; out += tgp.z * M * N;
  const int tm = tgp.x, tn = tgp.y, sg = sgi;
  auto A = Operand<LT, LS>::make(x, K, M, xs);
  auto B = Operand<RT, RS>::make(w, K, N, ws);
  constexpr auto desc = matmul2d_descriptor(32, 32, 32, false, true, false, matmul2d_descriptor::mode::multiply_accumulate);
  matmul2d<desc, execution_simdgroup> op;
  auto cT = op.template get_destination_cooperative_tensor<decltype(A), decltype(B), AT>();
  #pragma unroll
  for (uint16_t i = 0; i < cT.get_capacity(); ++i) { if (cT.is_valid_element(i)) cT[i] = AT(0); }
  const int chunks = K / 32, base = chunks / SG, extra = chunks % SG;
  const int c0 = sg * base + min(sg, extra), nc = base + (sg < extra ? 1 : 0);
  for (int c = 0; c < nc; ++c) {
    const int k = 32 * (c0 + c);
    auto mA = A.slice(k, tm * 32);
    auto mB = B.slice(k, tn * 32);
    op.run(mA, mB, cT);
  }
  threadgroup float red[SG][32 * 32];
  #pragma unroll
  for (uint16_t i = 0; i < cT.get_capacity(); ++i) {
    if (!cT.is_valid_element(i)) continue;
    auto idx = cT.get_multidimensional_index(i);
    red[sg][idx[1] * 32 + idx[0]] = static_cast<float>(cT[i]);
  }
  threadgroup_barrier(mem_flags::mem_threadgroup);
  for (int e = tid; e < 32 * 32; e += SG * 32) {
    float s = 0.0f;
    #pragma unroll
    for (int j = 0; j < SG; ++j) s += red[j][e];
    const int n = tn * 32 + e % 32, m = tm * 32 + e / 32;
    if (n < N && m < M) out[(fl & 32) ? (m / dims[4] * N + n) * dims[4] + m % dims[4] : m * N + n] = heimdall_epi(s, m, n, fl, rs, cs, bias);
  }
}

// ------------------------------------------------------------------------------------------------ large M: full-K tiles
template <typename LT, typename RT, typename AT, bool LS, bool RS, int SM, int SN, int KT, int WM, int WN>
kernel void big(device uchar* x [[buffer(0)]], device uchar* w [[buffer(1)]],
                device uchar* xs [[buffer(2)]], device uchar* ws [[buffer(3)]],
                device const float* rs [[buffer(4)]], device const float* cs [[buffer(5)]],
                device half* out [[buffer(6)]], constant int* dims [[buffer(7)]],
                 device const half* bias [[buffer(8)]],
                uint3 tgp [[threadgroup_position_in_grid]], uint sgi [[simdgroup_index_in_threadgroup]],
                uint tid [[thread_index_in_threadgroup]]) {
  const int M = dims[0], K = dims[1], N = dims[2], fl = dims[3];
  // Heimdall: independent batch planes, same Vella tile arithmetic.
  x += tgp.z * M * K * 2; w += (tgp.z % uint(dims[5])) * N * K * 2; out += tgp.z * M * N;
  const int m0 = tgp.y * (SM * WM) + (sgi / WN) * SM;
  const int n0 = tgp.x * (SN * WN) + (sgi % WN) * SN;
  auto A = Operand<LT, LS>::make(x, K, M, xs);
  auto B = Operand<RT, RS>::make(w, K, N, ws);
  constexpr auto desc = matmul2d_descriptor(SM, SN, KT, false, true, false, matmul2d_descriptor::mode::multiply_accumulate);
  matmul2d<desc, execution_simdgroup> op;
  auto cT = op.template get_destination_cooperative_tensor<decltype(A), decltype(B), AT>();
  #pragma unroll
  for (uint16_t i = 0; i < cT.get_capacity(); ++i) { if (cT.is_valid_element(i)) cT[i] = AT(0); }
  if (m0 < M && n0 < N) {
    for (int k = 0; k < K; k += KT) {
      auto mA = A.slice(k, m0);
      auto mB = B.slice(k, n0);
      op.run(mA, mB, cT);
    }
  }
  #pragma unroll
  for (uint16_t i = 0; i < cT.get_capacity(); ++i) {
    if (!cT.is_valid_element(i)) continue;
    auto idx = cT.get_multidimensional_index(i);
    const int n = n0 + idx[0], m = m0 + idx[1];
    if (n < N && m < M) out[(fl & 32) ? (m / dims[4] * N + n) * dims[4] + m % dims[4] : m * N + n] = heimdall_epi(cT[i], m, n, fl, rs, cs, bias);
  }
}

// ------------------------------------------------------------------------------------------------ tensor-unit rate
// One SMxSNxKT matmul per simdgroup per iteration on threadgroup-resident operands (format types cannot be
// cooperative-tensor inputs, so threadgroup memory is the closest-to-the-unit operand source every format shares).
// Tiles are copied once from device memory; the loop has no device traffic. FLOPs per run: 2*SM*SN*KT.
// Two shapes (32x32 and 64x64 per simdgroup) halve the operand bytes per FLOP: if a format's ratio to bf16 is the
// same at both, operand feeding is not what limits it.
template <typename LT, typename RT, typename AT, int SM, int SN, int KT>
kernel void alu(device uchar* x [[buffer(0)]], device uchar* w [[buffer(1)]], device float* out [[buffer(2)]],
                constant int& R [[buffer(3)]],
                uint3 tgp [[threadgroup_position_in_grid]], uint sgi [[simdgroup_index_in_threadgroup]],
                uint lane [[thread_index_in_simdgroup]], uint tid [[thread_index_in_threadgroup]],
                uint3 tptg [[threads_per_threadgroup]]) {
  threadgroup uint4 tA4[SM * KT * 2 / 16], tB4[SN * KT * 2 / 16];
  threadgroup uchar* tA = (threadgroup uchar*)tA4;
  threadgroup uchar* tB = (threadgroup uchar*)tB4;
  for (uint i = tid; i < SM * KT * 2; i += tptg.x) tA[i] = x[i];
  for (uint i = tid; i < SN * KT * 2; i += tptg.x) tB[i] = w[i];
  threadgroup_barrier(mem_flags::mem_threadgroup);
  constexpr auto desc = matmul2d_descriptor(SM, SN, KT, false, true, false, matmul2d_descriptor::mode::multiply_accumulate);
  matmul2d<desc, execution_simdgroup> op;
  auto A = tensor<threadgroup LT, dextents<int32_t, 2>, tensor_inline>((threadgroup typename Store<LT>::type*)tA, dextents<int32_t, 2>(KT, SM), array<int, 2>{1, KT});
  auto B = tensor<threadgroup RT, dextents<int32_t, 2>, tensor_inline>((threadgroup typename Store<RT>::type*)tB, dextents<int32_t, 2>(KT, SN), array<int, 2>{1, KT});
  auto cT = op.template get_destination_cooperative_tensor<decltype(A), decltype(B), AT>();
  #pragma unroll
  for (uint16_t i = 0; i < cT.get_capacity(); ++i) { if (cT.is_valid_element(i)) cT[i] = AT(0); }
  for (int r = 0; r < R; ++r) op.run(A, B, cT);
  float s = 0.0f;
  #pragma unroll
  for (uint16_t i = 0; i < cT.get_capacity(); ++i) { if (cT.is_valid_element(i)) s += static_cast<float>(cT[i]); }
  out[(tgp.x * (tptg.x / 32) + sgi) * 32 + lane] = s;
}

// Empty dispatch: per-dispatch launch cost of the harness (reported, not subtracted).
kernel void nop(device float* o [[buffer(0)]], uint t [[thread_position_in_grid]]) { if (t == 0xFFFFFFFF) o[0] = 0; }

// ------------------------------------------------------------------------------------------------ instantiations
#define ARGS device uchar*, device uchar*, device uchar*, device uchar*, device const float*, device const float*, \
             device half*, constant int*, device const half*, uint3, uint, uint
#define TILE(name, LT, RT, AT, LS, RS) \
  template [[host_name("tile8_" #name)]] kernel void tile<LT, RT, AT, LS, RS, 8>(ARGS); \
  template [[host_name("tile4_" #name)]] kernel void tile<LT, RT, AT, LS, RS, 4>(ARGS);
// big configs: a = 32x32 per SG, KT 32, 2x4 SG (64x128 tile); b = same with KT 64; c = 64x32 per SG, KT 32, 2x4 SG (128x128)
#define BIG(name, LT, RT, AT, LS, RS) \
  template [[host_name("biga_" #name)]] kernel void big<LT, RT, AT, LS, RS, 32, 32, 32, 2, 4>(ARGS); \
  template [[host_name("bigb_" #name)]] kernel void big<LT, RT, AT, LS, RS, 32, 32, 64, 2, 4>(ARGS); \
  template [[host_name("bigc_" #name)]] kernel void big<LT, RT, AT, LS, RS, 64, 32, 32, 2, 4>(ARGS);
#define ALUARGS device uchar*, device uchar*, device float*, constant int&, uint3, uint, uint, uint, uint3
#define ALU(name, LT, RT, AT, KT) \
  template [[host_name("alu_" #name)]] kernel void alu<LT, RT, AT, 32, 32, KT>(ALUARGS); \
  template [[host_name("alub_" #name)]] kernel void alu<LT, RT, AT, 64, 64, KT>(ALUARGS);
#define GEMM(name, LT, RT, AT, LS, RS) TILE(name, LT, RT, AT, LS, RS) BIG(name, LT, RT, AT, LS, RS)

// macOS 26 operand types (controls; run on 26.6 and 27)
GEMM(bf16,    bfloat, bfloat,       float,   false, false)
GEMM(f16,     half,   half,         float,   false, false)
GEMM(i8,      half,   int8_t,       float,   false, false)
GEMM(i4,      half,   int4b_format, float,   false, false)
GEMM(i8i8,    int8_t, int8_t,       int32_t, false, false)
ALU(bf16,     bfloat, bfloat,       float,   32)
ALU(f16,      half,   half,         float,   32)
ALU(i8,       half,   int8_t,       float,   32)
ALU(i4,       half,   int4b_format, float,   32)
ALU(i8i8,     int8_t, int8_t,       int32_t, 32)

#if F27
// macOS 27 formats. Weight-only: half activations (the only 16-bit left type the FP8/FP4 table allows).
GEMM(e4m3,    half,                  metal_fp8_e4m3_format, float, false, false)
GEMM(e5m2,    half,                  metal_fp8_e5m2_format, float, false, false)
GEMM(e2m1,    half,                  metal_fp4_e2m1_format, float, false, false)
GEMM(e4m3mx,  half,                  metal_fp8_e4m3_format, float, false, true)
GEMM(e5m2mx,  half,                  metal_fp8_e5m2_format, float, false, true)
GEMM(e2m1mx,  half,                  metal_fp4_e2m1_format, float, false, true)
// Both operands narrow (W8A8 / W4A4).
GEMM(e4m3x2,   metal_fp8_e4m3_format, metal_fp8_e4m3_format, float, false, false)
GEMM(e5m2x2,   metal_fp8_e5m2_format, metal_fp8_e5m2_format, float, false, false)
GEMM(e2m1x2,   metal_fp4_e2m1_format, metal_fp4_e2m1_format, float, false, false)
GEMM(e4m3x2mx, metal_fp8_e4m3_format, metal_fp8_e4m3_format, float, true,  true)
GEMM(e5m2x2mx, metal_fp8_e5m2_format, metal_fp8_e5m2_format, float, true,  true)
GEMM(e2m1x2mx, metal_fp4_e2m1_format, metal_fp4_e2m1_format, float, true,  true)
// Exploratory: MXINT8 (int8 elements + E8M0 block scales). The tensor API accepts a scale plane on int8 operands;
// whether the unit applies it is exactly what the check measures.
GEMM(i8mx,     half,                  int8_t,                float,   false, true)
GEMM(i8i8mx,   int8_t,                int8_t,                int32_t, true,  true)
ALU(e4m3,     half,                  metal_fp8_e4m3_format, float, 32)
ALU(e5m2,     half,                  metal_fp8_e5m2_format, float, 32)
ALU(e2m1,     half,                  metal_fp4_e2m1_format, float, 32)
ALU(e4m3x2,   metal_fp8_e4m3_format, metal_fp8_e4m3_format, float, 32)
ALU(e5m2x2,   metal_fp8_e5m2_format, metal_fp8_e5m2_format, float, 32)
ALU(e2m1x2,   metal_fp4_e2m1_format, metal_fp4_e2m1_format, float, 32)
#endif
