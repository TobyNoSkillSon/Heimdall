// Dense 2048 -> 512 BF16/FP16 Metal 4 tensor tile for the trunk's MLP down and
// global-attention output projections (pass-1 B, ported from the Python worker).
// 32x32 outputs per threadgroup, four simdgroups split K, FP32 partials reduced in
// threadgroup memory, one rounding to the activation dtype. Changes only the FP32
// accumulation order. Rows 9...768, matching half dtype, no bias, Apple GPU
// generation 17+ (phones 18+) on macOS 26.2+; stock MLX everywhere else.
import Foundation
import MLX

enum TileLinear {
    static let kernel = MLXFast.metalKernel(
        name: "heimdall_eg2_down", inputNames: ["x", "w"], outputNames: ["out"],
        source: """
            const int M = x_shape[0], K = x_shape[1], N = w_shape[0];
            const int tm = threadgroup_position_in_grid.x, tn = threadgroup_position_in_grid.y;
            const int sg = simdgroup_index_in_threadgroup, kc = K / 4, k0 = sg * kc;
            auto A = tensor<device T, dextents<int32_t, 2>, tensor_inline>((device T*)x + k0, dextents<int32_t, 2>(kc, M), array<int, 2>{1, K});
            auto B = tensor<device T, dextents<int32_t, 2>, tensor_inline>((device T*)w + k0, dextents<int32_t, 2>(kc, N), array<int, 2>{1, K});
            constexpr auto desc = matmul2d_descriptor(32, 32, static_cast<int>(dynamic_extent), false, true, false);
            matmul2d<desc, execution_simdgroup> op;
            auto mA = A.slice(0, tm * 32), mB = B.slice(0, tn * 32);
            auto cT = op.template get_destination_cooperative_tensor<decltype(mA), decltype(mB), float>();
            for (uint16_t i = 0; i < cT.get_capacity(); ++i) { if (cT.is_valid_element(i)) cT[i] = 0; }
            op.run(mA, mB, cT);
            threadgroup float red[4][1024];
            for (uint16_t i = 0; i < cT.get_capacity(); ++i) {
                if (!cT.is_valid_element(i)) continue;
                auto idx = cT.get_multidimensional_index(i);
                red[sg][idx[1] * 32 + idx[0]] = cT[i];
            }
            threadgroup_barrier(mem_flags::mem_threadgroup);
            for (int e = thread_index_in_threadgroup; e < 1024; e += 128) {
                float s = 0.0f;
                for (int j = 0; j < 4; ++j) s += red[j][e];
                const int n = tn * 32 + e % 32, m = tm * 32 + e / 32;
                if (n < N && m < M) out[m * N + n] = static_cast<T>(s);
            }
            """,
        header: """
            #include <metal_tensor>
            #include <MetalPerformancePrimitives/MetalPerformancePrimitives.h>
            using namespace mpp::tensor_ops;
            """)

    /// Caller guarantees: SmallMGEMM.tensorOpsAvailable, rows 9...768, x [..., 2048] and w [512, 2048] in one half dtype.
    static func apply(_ x: MLXArray, _ weight: MLXArray) -> MLXArray {
        let rows = x.size / x.dim(-1)
        let out = Self.kernel([x.reshaped(rows, 2048), weight], template: [("T", x.dtype)],
                              grid: (((rows + 31) / 32) * 128, 16, 1), threadGroup: (128, 1, 1),
                              outputShapes: [[rows, 512]], outputDTypes: [x.dtype])[0]
        return out.reshaped(Array(x.shape.dropLast()) + [512])
    }
}
