// Copyright (c) 2026 Vella contributors. Adapted under MIT; see ../VELLA-LICENSE.
// Heimdall FP16 convolution / projection kernels, FP32 accumulation and output.
// GEMM geometry adapted from Vella lab/models/Shared/vk-formats27/kernels.metal
// (the user's Vault, 11 Oct 2026); no runtime dependency on Vella.
#include <metal_stdlib>
#include <metal_tensor>
#include <MetalPerformancePrimitives/MetalPerformancePrimitives.h>
using namespace metal;
using namespace mpp::tensor_ops;

// NCHW; the final dimension is time in the ReDimNet spectrogram. A 1-D
// convolution is represented with H=KH=OH=1. Batch remains explicit.
struct Conv { uint B, C, H, W, O, OH, OW, KH, KW, SH, SW, PH, PW, DH, DW, G, KP, MP; };
kernel void relu_bn(device float* x [[buffer(0)]],device const float* scale [[buffer(1)]],device const float* bias [[buffer(2)]],constant uint4& d [[buffer(3)]],uint i [[thread_position_in_grid]]) {
    if(i<d.x) {uint c=(i/d.z)%d.y;x[i]=max(x[i],0.f)*scale[c]+bias[c];}
}
kernel void pack_conv(device const float* x [[buffer(0)]], device half* a [[buffer(1)]],
                      constant Conv& p [[buffer(2)]], uint i [[thread_position_in_grid]]) {
    if (i >= p.KP*p.MP) return;
    uint m=i/p.KP, k=i%p.KP, K=p.C*p.KH*p.KW;
    if (m >= p.B*p.OH*p.OW || k >= K) { a[i]=half(0); return; }
    uint b=m/(p.OH*p.OW), h=(m/p.OW)%p.OH, w=m%p.OW;
    uint c=k/(p.KH*p.KW), kh=(k/p.KW)%p.KH, kw=k%p.KW;
    int ih=int(h*p.SH+kh*p.DH)-int(p.PH), iw=int(w*p.SW+kw*p.DW)-int(p.PW);
    a[i]=(ih>=0 && iw>=0 && ih<int(p.H) && iw<int(p.W)) ? half(x[((b*p.C+c)*p.H+ih)*p.W+iw]) : half(0);
}
kernel void pack_linear(device const float* x [[buffer(0)]], device half* a [[buffer(1)]],
                        constant uint4& d [[buffer(2)]], uint i [[thread_position_in_grid]]) {
    if (i<d.w*d.y) a[i]=i/d.y<d.x ? half(x[i]) : half(0);
}
// Streaming projections with <=8 rows: avoid padding short rows to a 32-row
// tensor-unit tile or allocating/dispatching an activation staging buffer.
kernel void projection_mv(device const float* x [[buffer(0)]],device const half* w [[buffer(1)]],
                           device float* y [[buffer(2)]],constant uint4& d [[buffer(3)]],
                           uint3 tg [[threadgroup_position_in_grid]],uint sg [[simdgroup_index_in_threadgroup]],uint lane [[thread_index_in_simdgroup]]) {
    uint n=tg.x*4+sg;
    if(n>=d.z) return;
    float4 sum=0.f, tail=0.f;
    for(uint k=lane;k<d.y;k+=32) {
        // GGML's skinny Q8 route keeps FP32 activations/dequantization. Keep
        // that arithmetic boundary: two half planes retain the exact Q8 scale
        // product instead of introducing an extra half rounding in every block.
        uint low=((d.z+31)/32)*32*d.y;
        float v=float(w[n*d.y+k])+float(w[low+n*d.y+k]);
        sum.x=fma(v,x[k],sum.x);
        if(d.x>1)sum.y=fma(v,x[d.y+k],sum.y);
        if(d.x>2)sum.z=fma(v,x[2*d.y+k],sum.z);
        if(d.x>3)sum.w=fma(v,x[3*d.y+k],sum.w);
        if(d.x>4)tail.x=fma(v,x[4*d.y+k],tail.x);
        if(d.x>5)tail.y=fma(v,x[5*d.y+k],tail.y);
        if(d.x>6)tail.z=fma(v,x[6*d.y+k],tail.z);
        if(d.x>7)tail.w=fma(v,x[7*d.y+k],tail.w);
    }
    sum=simd_sum(sum);tail=simd_sum(tail);
    if(lane==0) { y[n]=sum.x;if(d.x>1)y[d.z+n]=sum.y;if(d.x>2)y[2*d.z+n]=sum.z;if(d.x>3)y[3*d.z+n]=sum.w;if(d.x>4)y[4*d.z+n]=tail.x;if(d.x>5)y[5*d.z+n]=tail.y;if(d.x>6)y[6*d.z+n]=tail.z;if(d.x>7)y[7*d.z+n]=tail.w; }
}

// Shared half-operand, float-accumulator GEMM. Shapes are padded to 32 at
// the input boundaries, never inferred from an out-of-bounds tensor slice.
kernel void gemm(device half* a [[buffer(0)]], device half* w [[buffer(1)]],
                 device const float* bias [[buffer(2)]], device float* out [[buffer(3)]],
                 constant uint4& d [[buffer(4)]], constant uint4& layout [[buffer(5)]],
                 uint3 tg [[threadgroup_position_in_grid]]) {
    auto A=tensor<device half,dextents<int32_t,2>,tensor_inline>(a,dextents<int32_t,2>(d.y,d.w),array<int,2>{1,int(d.y)});
    auto W=tensor<device half,dextents<int32_t,2>,tensor_inline>(w,dextents<int32_t,2>(d.y,((d.z+31)/32)*32),array<int,2>{1,int(d.y)});
    constexpr auto desc=matmul2d_descriptor(32,32,32,false,true,false,matmul2d_descriptor::mode::multiply_accumulate);
    matmul2d<desc,execution_simdgroup> op;
    auto acc=op.get_destination_cooperative_tensor<decltype(A),decltype(W),float>();
    for(uint16_t e=0;e<acc.get_capacity();++e) if(acc.is_valid_element(e)) acc[e]=0.f;
    for(uint k=0;k<d.y;k+=32) { auto aa=A.slice(k,tg.y*32); auto ww=W.slice(k,tg.x*32); op.run(aa,ww,acc); }
    for(uint16_t e=0;e<acc.get_capacity();++e) {
        if(!acc.is_valid_element(e)) continue;
        auto ix=acc.get_multidimensional_index(e);
        uint n=tg.x*32+ix[0],m=tg.y*32+ix[1];
        if(m>=d.x || n>=d.z) continue;
        // layout.x==0: [M,N]; otherwise NCHW, spatial size layout.x.
        uint oi=layout.x ? ((m/layout.x)*d.z+n)*layout.x+m%layout.x : m*d.z+n;
        out[oi]=acc[e]+(layout.y ? bias[n] : 0.f);
    }
}
// Vella biga geometry: eight independent 32x32 SIMD tiles per 64x128 group.
kernel void gemm_big(device half* a [[buffer(0)]], device half* w [[buffer(1)]],
                 device const float* bias [[buffer(2)]], device float* out [[buffer(3)]],
                 constant uint4& d [[buffer(4)]], constant uint4& layout [[buffer(5)]],
                 uint3 tg [[threadgroup_position_in_grid]],uint sg [[simdgroup_index_in_threadgroup]]) {
    uint n0=tg.x*128+(sg%4)*32,m0=tg.y*64+(sg/4)*32;
    if(n0>=d.z || m0>=d.x) return;
    auto A=tensor<device half,dextents<int32_t,2>,tensor_inline>(a,dextents<int32_t,2>(d.y,d.w),array<int,2>{1,int(d.y)});
    auto W=tensor<device half,dextents<int32_t,2>,tensor_inline>(w,dextents<int32_t,2>(d.y,((d.z+31)/32)*32),array<int,2>{1,int(d.y)});
    constexpr auto desc=matmul2d_descriptor(32,32,32,false,true,false,matmul2d_descriptor::mode::multiply_accumulate);
    matmul2d<desc,execution_simdgroup> op;
    auto acc=op.get_destination_cooperative_tensor<decltype(A),decltype(W),float>();
    for(uint16_t e=0;e<acc.get_capacity();++e) if(acc.is_valid_element(e)) acc[e]=0.f;
    for(uint k=0;k<d.y;k+=32) { auto aa=A.slice(k,m0); auto ww=W.slice(k,n0); op.run(aa,ww,acc); }
    for(uint16_t e=0;e<acc.get_capacity();++e) {
        if(!acc.is_valid_element(e)) continue;
        auto ix=acc.get_multidimensional_index(e);
        uint n=n0+ix[0],m=m0+ix[1];
        if(m>=d.x || n>=d.z) continue;
        // layout.x==0: [M,N]; otherwise NCHW, spatial size layout.x.
        uint oi=layout.x ? ((m/layout.x)*d.z+n)*layout.x+m%layout.x : m*d.z+n;
        out[oi]=acc[e]+(layout.y ? bias[n] : 0.f);
    }
}
kernel void grouped_conv(device const float* x [[buffer(0)]], device const half* w [[buffer(1)]],
                         device const float* bias [[buffer(2)]], device float* out [[buffer(3)]],
                         constant Conv& p [[buffer(4)]], constant uint& has_bias [[buffer(5)]],
                         uint i [[thread_position_in_grid]]) {
    if(i>=p.B*p.O*p.OH*p.OW) return;
    uint ow=i%p.OW,oh=(i/p.OW)%p.OH,o=(i/(p.OW*p.OH))%p.O,b=i/(p.O*p.OH*p.OW);
    uint ci=p.C/p.G, group=o/(p.O/p.G);
    float v=0.f;
    for(uint c=0;c<ci;++c) for(uint kh=0;kh<p.KH;++kh) for(uint kw=0;kw<p.KW;++kw) {
        int ih=int(oh*p.SH+kh*p.DH)-int(p.PH), iw=int(ow*p.SW+kw*p.DW)-int(p.PW);
        if(ih>=0 && iw>=0 && ih<int(p.H) && iw<int(p.W))
            v+=float(half(x[((b*p.C+group*ci+c)*p.H+ih)*p.W+iw]))*float(w[((o*ci+c)*p.KH+kh)*p.KW+kw]);
    }
    out[i]=v+(has_bias ? bias[o] : 0.f);
}
