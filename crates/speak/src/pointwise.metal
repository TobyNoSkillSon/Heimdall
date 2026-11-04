// Copyright (c) 2026 Vella contributors. Adapted under MIT; see ../VELLA-LICENSE.
// Adapted from Vella lab/models/Shared/vk-formats27/kernels.metal (tile).
// Vendored arithmetic/geometry only; no Vella runtime dependency.
#include <metal_stdlib>
#include <metal_tensor>
#include <MetalPerformancePrimitives/MetalPerformancePrimitives.h>
using namespace metal;
using namespace mpp::tensor_ops;

// ONNX NCT -> padded MK. Padding belongs to each batch, never between channels.
kernel void pack(device const float* x [[buffer(0)]], device half* a [[buffer(1)]],
                 constant uint4& d [[buffer(2)]], uint i [[thread_position_in_grid]]) {
    uint K=d.x,T=d.y,P=d.z;
    uint k=i%K,m=i/K,b=m/P,t=m%P;
    a[i]=t<T ? half(x[(b*K+k)*T+t]) : half(0);
}

kernel void pointwise(device half* a [[buffer(0)]], device half* w [[buffer(1)]],
                      device const float* bias [[buffer(2)]], device float* y [[buffer(3)]],
                      constant uint4& d [[buffer(4)]], constant uint2& bt [[buffer(5)]],
                      uint3 tg [[threadgroup_position_in_grid]],
                      uint sg [[simdgroup_index_in_threadgroup]], uint tid [[thread_index_in_threadgroup]]) {
    int M=d.x,K=d.y,N=d.z,P=d.w,T=bt.y;
    using View=tensor<device half,dextents<int32_t,2>,tensor_inline>;
    View A(a,dextents<int32_t,2>(K,M),array<int,2>{1,K});
    View W(w,dextents<int32_t,2>(K,N),array<int,2>{1,K});
    constexpr auto desc=matmul2d_descriptor(32,32,32,false,true,false,matmul2d_descriptor::mode::multiply_accumulate);
    matmul2d<desc,execution_simdgroup> op;
    auto c=op.get_destination_cooperative_tensor<View,View,float>();
    for(uint16_t i=0;i<c.get_capacity();++i) if(c.is_valid_element(i)) c[i]=0.f;
    int chunks=K/32,base=chunks/4,extra=chunks%4;
    int start=sg*base+min(int(sg),extra),count=base+(int(sg)<extra);
    for(int k=0;k<count;++k) {auto aa=A.slice(32*(start+k),tg.x*32);auto ww=W.slice(32*(start+k),tg.y*32);op.run(aa,ww,c);}
    threadgroup float red[4][1024];
    for(uint16_t i=0;i<c.get_capacity();++i) if(c.is_valid_element(i)) {
        auto idx=c.get_multidimensional_index(i);red[sg][idx[1]*32+idx[0]]=c[i];
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for(int e=tid;e<1024;e+=128) {
        int m=tg.x*32+e/32,n=tg.y*32+e%32,b=m/P,t=m%P;
        if(m<M && n<N && t<T) {
            float v=red[0][e]+red[1][e]+red[2][e]+red[3][e];
            y[(b*N+n)*T+t]=v+bias[n];
        }
    }
}
