// Preserve FP32 operands for the projector's two 1x1 convolutions. The pinned
// stock f32/f32 specialization rounds both operands to half. Other d1 matrices
// retain their original specialization. FP32 is required here for noise-level
// probability parity; shared Whisper kernels remain FP16. The owning CMake
// patch disables relaxed precision ONLY for this float specialization.
// Template/layout: llama.cpp, MIT.
template [[host_name("kernel_mul_mm_heimdall_f32")]] kernel mul_mm_t
kernel_mul_mm<float,float4x4,simdgroup_float8x8,float,float2x4,simdgroup_float8x8,float4x4,1,dequantize_f32,float,float4x4,float,float2x4>;
