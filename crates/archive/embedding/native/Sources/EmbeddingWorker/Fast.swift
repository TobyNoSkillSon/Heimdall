// Optional fast paths of the helper. Each one is a switch in `Fast`; the gate (Gate.swift)
// turns a switch on only after its self-test passed on this Mac, and `.plain` is stock MLX.
import Foundation
import MLX
import MLXNN

struct Fast: Codable, Equatable {
    /// Vella SmallMGEMM tile/GEMV kernels for small-row projections.
    var smallM = false
    /// The 2048 -> 512 Metal 4 tensor tile (M5-class GPU, macOS 26.2+).
    var tile = false
    /// One Metal kernel for the audio encoder's chunked local attention.
    var audioKernel = false
    /// Shapeless compiled element-wise steps in the trunk (GELU gate, residual + norm).
    var compiled = false
    /// Shape-compiled vision tower (screens share a few geometries).
    var compiledVision = false
    /// MLX's fused (flash) attention kernel forced for head dim 256; MLX picks unfused there by default.
    var fused256 = false
    /// Heimdall flash attention kernel for head dim 512 (global layers, long inputs). MLX has none.
    var flash512 = false
    /// Long global attention computed in query blocks: BF16-rounding-equal values, bounded score memory.
    var blocked = false
    /// Several chunks per trunk/vision call.
    var batch = false

    static let plain = Fast()
    static let names = ["smallM", "tile", "audioKernel", "compiled", "compiledVision", "fused256", "flash512", "blocked", "batch"]
    subscript(name: String) -> Bool {
        get {
            switch name {
            case "smallM": smallM
            case "tile": tile
            case "audioKernel": audioKernel
            case "compiled": compiled
            case "compiledVision": compiledVision
            case "fused256": fused256
            case "flash512": flash512
            case "blocked": blocked
            case "batch": batch
            default: false
            }
        }
        set {
            switch name {
            case "smallM": smallM = newValue
            case "tile": tile = newValue
            case "audioKernel": audioKernel = newValue
            case "compiled": compiled = newValue
            case "compiledVision": compiledVision = newValue
            case "fused256": fused256 = newValue
            case "flash512": flash512 = newValue
            case "blocked": blocked = newValue
            case "batch": batch = newValue
            default: break
            }
        }
    }
    var enabled: [String] { Self.names.filter { self[$0] } }
    var isPlain: Bool { enabled.isEmpty }
}

/// Shapeless compiled element-wise chains of one trunk layer. Same operations and dtypes as the
/// eager graph, fused into fewer kernels.
enum Compiled {
    nonisolated(unsafe) static var eps: Float = 1e-6
    /// gelu_tanh(a) * b
    nonisolated(unsafe) static let geluGateFunction = compile(shapeless: true) { (a: MLXArray, b: MLXArray) in
        geluApproximate(a) * b
    }
    /// x + rmsNorm(h; w32) in h's dtype
    nonisolated(unsafe) static let residualNormFunction = compile(shapeless: true) { (a: [MLXArray]) -> [MLXArray] in
        [a[0] + MLXFast.rmsNorm(a[1], weight: a[2], eps: Compiled.eps).asType(a[1].dtype)]
    }
    /// (x + rmsNorm(h; w32)) * scalar
    nonisolated(unsafe) static let residualNormScaleFunction = compile(shapeless: true) { (a: [MLXArray]) -> [MLXArray] in
        [(a[0] + MLXFast.rmsNorm(a[1], weight: a[2], eps: Compiled.eps).asType(a[1].dtype)) * a[3]]
    }
    static func geluGate(_ a: MLXArray, _ b: MLXArray) -> MLXArray { geluGateFunction(a, b) }
    static func residualNorm(_ x: MLXArray, _ h: MLXArray, _ w: MLXArray) -> MLXArray { residualNormFunction([x, h, w])[0] }
    static func residualNormScale(_ x: MLXArray, _ h: MLXArray, _ w: MLXArray, _ s: MLXArray) -> MLXArray {
        residualNormScaleFunction([x, h, w, s])[0]
    }
}

enum Attention {
    /// Global layers switch to the flash kernel or query blocks from this many query rows on.
    nonisolated(unsafe) static var longRows = 1024
    static let blockRows = 512

    /// Bidirectional attention, scale 1; `mask` bool, broadcastable to [B, H, Lq, Lk].
    static func full(queries: MLXArray, keys: MLXArray, values: MLXArray, mask: MLXArray?, fast: Fast) -> MLXArray {
        let headDim = queries.dim(-1), rows = queries.dim(2)
        if fast.flash512, headDim == 512, rows >= longRows, let out = Flash.attention(queries, keys, values, mask: mask) {
            return out
        }
        if fast.fused256, headDim == 256, rows > 8 {
            return MLXFast.scaledDotProductAttention(queries: queries, keys: keys, values: values, scale: 1,
                                                     mask: mask.map { .array($0) } ?? .none, forceFused: true)
        }
        if fast.blocked, rows >= longRows {
            // Softmax is per query row; only the GEMM tiling (BF16 rounding) differs from one call.
            let perRowMask = mask.map { $0.ndim == 4 && $0.dim(2) == rows } ?? false
            var outputs: [MLXArray] = []
            for start in stride(from: 0, to: rows, by: blockRows) {
                let end = min(rows, start + blockRows)
                let blockMask = perRowMask ? mask![0..., 0..., start..<end, 0...] : mask
                outputs.append(MLXFast.scaledDotProductAttention(
                    queries: queries[0..., 0..., start..<end, 0...], keys: keys, values: values, scale: 1,
                    mask: blockMask.map { .array($0) } ?? .none))
            }
            return concatenated(outputs, axis: 2)
        }
        return MLXFast.scaledDotProductAttention(queries: queries, keys: keys, values: values, scale: 1,
                                                 mask: mask.map { .array($0) } ?? .none)
    }
}

/// Flash attention for head dims MLX has no fused kernel for (512) or skips (256): one simdgroup
/// per (query row, head), every head of a row in one threadgroup so grouped heads share K/V
/// reads; online softmax in FP32, scores never stored. Key-only masks ([B, 1, 1, Lk]) or none.
enum Flash {
    static let kernel = MLXFast.metalKernel(
        name: "heimdall_eg2_flash", inputNames: ["q", "k", "v", "valid"], outputNames: ["out"],
        source: """
            const uint lane = thread_index_in_simdgroup;
            const int h = simdgroup_index_in_threadgroup;
            const int t = thread_position_in_grid.y, b = thread_position_in_grid.z;
            const int H = q_shape[1], L = q_shape[2], HK = k_shape[1], LK = k_shape[2];
            const int hk = h / (H / HK);
            device const T* qp = q + (((size_t)b * H + h) * L + t) * D + lane * DL;
            device const T* kp = k + ((size_t)b * HK + hk) * (size_t)LK * D + lane * DL;
            device const T* vp = v + ((size_t)b * HK + hk) * (size_t)LK * D + lane * DL;
            float qv[DL], acc[DL];
            for (int i = 0; i < DL; ++i) { qv[i] = float(qp[i]); acc[i] = 0.0f; }
            float m = -INFINITY, l = 0.0f;
            for (int j = 0; j < LK; ++j) {
                if (HAS_MASK && !valid[(size_t)b * LK + j]) continue;
                device const T* kj = kp + (size_t)j * D;
                float s = 0.0f;
                for (int i = 0; i < DL; ++i) s += qv[i] * float(kj[i]);
                s = simd_sum(s);
                const float mn = max(m, s);
                const float c = metal::precise::exp(m - mn), p = metal::precise::exp(s - mn);
                l = l * c + p;
                device const T* vj = vp + (size_t)j * D;
                for (int i = 0; i < DL; ++i) acc[i] = acc[i] * c + p * float(vj[i]);
                m = mn;
            }
            device T* op = out + (((size_t)b * H + h) * L + t) * D + lane * DL;
            const float inv = l > 0.0f ? 1.0f / l : 0.0f;
            for (int i = 0; i < DL; ++i) op[i] = T(acc[i] * inv);
            """)

    /// q [B, H, L, D], k/v [B, HK, LK, D]; nil when the call is outside the kernel's shapes.
    static func attention(_ q: MLXArray, _ k: MLXArray, _ v: MLXArray, mask: MLXArray?) -> MLXArray? {
        let (batch, heads, rows, dim) = (q.dim(0), q.dim(1), q.dim(2), q.dim(3))
        guard [256, 512].contains(dim), k.dim(3) == dim, v.dim(3) == dim, heads % k.dim(1) == 0, heads <= 32,
              k.dim(0) == batch, q.dtype == k.dtype, q.dtype == v.dtype else { return nil }
        var valid = MLXArray([true])
        if let mask {
            guard mask.ndim == 4, mask.dim(0) == batch, mask.dim(1) == 1, mask.dim(2) == 1, mask.dim(3) == k.dim(2) else { return nil }
            valid = mask.reshaped(batch, k.dim(2)).asType(.bool)
        }
        return kernel([q, k, v, valid], template: [("T", q.dtype), ("D", dim), ("DL", dim / 32), ("HAS_MASK", mask != nil)],
                      grid: (32 * heads, rows, batch), threadGroup: (32 * heads, 1, 1),
                      outputShapes: [q.shape], outputDTypes: [q.dtype])[0]
    }
}

/// An MLX affine-quantized weight (group 64) replacing one BF16 projection or the token table.
struct QuantizedWeight {
    let wq: MLXArray, scales: MLXArray, biases: MLXArray?, bits: Int, groupSize: Int
    init(_ w: MLXArray, bits: Int, groupSize: Int = 64) {
        let q = MLX.quantized(w, groupSize: groupSize, bits: bits)
        wq = q.wq; scales = q.scales; biases = q.biases; self.bits = bits; self.groupSize = groupSize
        eval([wq, scales] + (biases.map { [$0] } ?? []))
    }
    func apply(_ x: MLXArray) -> MLXArray {
        quantizedMatmul(x, wq, scales: scales, biases: biases, transpose: true, groupSize: groupSize, bits: bits)
    }
    /// Table rows for `ids`, dequantized: [ids..., columns].
    func rows(_ ids: MLXArray) -> MLXArray {
        let flat = ids.flattened()
        let rows = dequantized(wq.take(flat, axis: 0), scales: scales.take(flat, axis: 0), biases: biases?.take(flat, axis: 0),
                               groupSize: groupSize, bits: bits)
        return rows.reshaped(ids.shape + [rows.dim(-1)])
    }
    var bytes: Int { wq.nbytes + scales.nbytes + (biases?.nbytes ?? 0) }
}
