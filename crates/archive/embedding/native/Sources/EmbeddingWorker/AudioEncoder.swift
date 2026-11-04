// Batched BF16 Gemma 4 audio encoder over the loaded checkpoint parameters.
//
// Same operations as the pinned mlx-vlm reference worker (fast RMSNorm, weak
// scalar residual weights, FP32 attention scores, BF16 context into the post
// projection), with every audio part of one input encoded in one padded call.
// The attention is causal (context right = 0) and both subsampling convolutions
// see zeroed padding, so a part's valid outputs do not depend on its batch peers.
// Upstream mlx-swift-lm multiplied the residual by a strong FP32 scalar array,
// which promoted all later audio layers and their weights to FP32.
// The chunked local attention (scaled q.k + relative bias, soft cap, causal window,
// softmax, values; ~25 MLX dispatches) runs as one Metal kernel per layer.
import Foundation
import MLX
import MLXNN

struct AudioConfig: Decodable {
    let hidden_size: Int
    let num_hidden_layers: Int
    let num_attention_heads: Int
    let rms_norm_eps: Float
    let attention_chunk_size: Int
    let attention_context_left: Int
    let attention_context_right: Int
    let attention_logit_cap: Float
    let attention_invalid_logits_value: Float
    let residual_weight: Float
    let gradient_clipping: Float
    let conv_kernel_size: Int
    let use_clipped_linears: Bool
}

final class AudioEncoder {
    struct Clipped {
        let weight: MLXArray, bounds: (MLXArray, MLXArray, MLXArray, MLXArray)?
        let key: String, store: QuantStore
        func product(_ x: MLXArray) -> MLXArray { store.weights[key]?.apply(x) ?? matmul(x, weight.T) }
        func callAsFunction(_ x: MLXArray) -> MLXArray {
            guard let (inMin, inMax, outMin, outMax) = bounds else { return product(x) }
            return clip(product(clip(x, min: inMin, max: inMax)), min: outMin, max: outMax)
        }
    }
    struct FeedForward { let pre: MLXArray, expand: Clipped, contract: Clipped, post: MLXArray }
    struct Layer {
        let ff1: FeedForward, ff2: FeedForward
        let q: Clipped, k: Clipped, v: Clipped, post: Clipped
        let queryScale: MLXArray        // [headDim] FP32: q scale * softplus(per-dim scale)
        let relativeKeys: MLXArray      // [heads, headDim, distances] FP32, constant per layer
        let relativeRows: MLXArray      // [heads, distances, headDim] FP32, same values (fused kernel)
        let preAttention: MLXArray, postAttention: MLXArray, out: MLXArray
        let convPre: MLXArray, convStart: Clipped, convWeight: MLXArray, convNorm: MLXArray, convEnd: Clipped
        var projections: [Clipped] { [ff1.expand, ff1.contract, ff2.expand, ff2.contract, q, k, v, post, convStart, convEnd] }
    }
    let store: QuantStore

    let config: AudioConfig
    let heads: Int, headDim: Int, chunk: Int, past: Int, future: Int, context: Int
    let conv0: MLXArray, norm0: MLXArray, conv1: MLXArray, norm1: MLXArray, inputProjection: MLXArray
    let layers: [Layer]
    let outputWeight: MLXArray, outputBias: MLXArray, embedWeight: MLXArray
    let causal: MLXArray   // [chunk, context] reference window rule
    var fusedAttention: MLXFast.MLXFastKernel? = nil
    static func relative0Width(_ c: AudioConfig) -> Int { max(0, c.attention_context_left - 1) + c.attention_context_right + 1 }

    init(parameters: [String: MLXArray], config: AudioConfig) throws {
        let store = QuantStore()
        self.store = store
        func p(_ key: String) throws -> MLXArray {
            guard let value = parameters[key] else { throw WorkerError.invalid("missing audio parameter \(key)") }
            return value
        }
        func clipped(_ key: String) throws -> Clipped {
            let weight = try p(key + ".linear.weight")
            guard config.use_clipped_linears else { return Clipped(weight: weight, bounds: nil, key: key, store: store) }
            return Clipped(weight: weight, bounds: (try p(key + ".input_min"), try p(key + ".input_max"),
                                                    try p(key + ".output_min"), try p(key + ".output_max")), key: key, store: store)
        }
        func feedForward(_ key: String) throws -> FeedForward {
            FeedForward(pre: try p(key + ".pre_layer_norm.weight"), expand: try clipped(key + ".ffw_layer_1"),
                        contract: try clipped(key + ".ffw_layer_2"), post: try p(key + ".post_layer_norm.weight"))
        }
        self.config = config
        heads = config.num_attention_heads
        headDim = config.hidden_size / heads
        chunk = config.attention_chunk_size
        past = max(0, config.attention_context_left - 1)
        future = config.attention_context_right
        context = chunk + past + future
        guard future == 0 else { throw WorkerError.invalid("batched audio requires a causal encoder") }
        let root = "audio_tower."
        conv0 = try p(root + "subsample_conv_projection.layer0.conv.weight")
        norm0 = try p(root + "subsample_conv_projection.layer0.norm.weight")
        conv1 = try p(root + "subsample_conv_projection.layer1.conv.weight")
        norm1 = try p(root + "subsample_conv_projection.layer1.norm.weight")
        inputProjection = try p(root + "subsample_conv_projection.input_proj_linear.weight")
        outputWeight = try p(root + "output_proj.weight")
        outputBias = try p(root + "output_proj.bias")
        embedWeight = try p("embed_audio.embedding_projection.weight")
        // Reference relative positions: distances `past` down to `-future`, [sin | cos],
        // FP32 angles cast to the activation dtype before the relative key projection.
        let half = config.hidden_size / 2
        let increment = log(10_000.0) / Double(max(half - 1, 1))
        let inverse = MLXArray((0..<half).map { Float(exp(Double($0) * -increment)) })
        let distances = MLXArray((-future...past).reversed().map { Float($0) })
        let angles = distances.expandedDimensions(axis: -1) * inverse
        let signal = concatenated([sin(angles), cos(angles)], axis: -1).asType(conv0.dtype)
        let keyScale = Float(log(1 + M_E) / log(2.0))
        let queryScale = Float(pow(Double(config.hidden_size / config.num_attention_heads), -0.5) / log(2.0))
        var built: [Layer] = []
        for i in 0..<config.num_hidden_layers {
            let key = root + "layers.\(i)."
            let relative = matmul(signal, try p(key + "self_attn.relative_k_proj.weight").T)
                .reshaped(-1, heads, headDim).asType(.float32).transposed(1, 2, 0)
            let perDim = try p(key + "self_attn.per_dim_scale").asType(.float32)
            let softplus = MLX.where(perDim .> 20, perDim, log(1 + exp(perDim)))
            built.append(Layer(
                ff1: try feedForward(key + "feed_forward1"), ff2: try feedForward(key + "feed_forward2"),
                q: try clipped(key + "self_attn.q_proj"), k: try clipped(key + "self_attn.k_proj"),
                v: try clipped(key + "self_attn.v_proj"), post: try clipped(key + "self_attn.post"),
                queryScale: queryScale * softplus, relativeKeys: relative,
                relativeRows: relative.transposed(0, 2, 1).contiguous(),
                preAttention: try p(key + "norm_pre_attn.weight"), postAttention: try p(key + "norm_post_attn.weight"),
                out: try p(key + "norm_out.weight"),
                convPre: try p(key + "lconv1d.pre_layer_norm.weight"), convStart: try clipped(key + "lconv1d.linear_start"),
                convWeight: try p(key + "lconv1d.depthwise_conv1d.weight"), convNorm: try p(key + "lconv1d.conv_norm.weight"),
                convEnd: try clipped(key + "lconv1d.linear_end")))
        }
        layers = built
        let row = MLXArray(0..<chunk).reshaped(chunk, 1)
        let column = MLXArray(0..<context).reshaped(1, context)
        let distance = row + past - column
        causal = (distance .>= 0) .&& (distance .< past)
        eval(layers.flatMap { [$0.relativeKeys, $0.relativeRows, $0.queryScale] } + [causal])
        let fusedOK = headDim == 128 && past + 1 == AudioEncoder.relative0Width(config) && past <= 16
        fusedAttention = fusedOK ? MLXFast.metalKernel(
            name: "heimdall_eg2_audio_attention", inputNames: ["q", "k", "v", "rel", "qscale", "lengths"], outputNames: ["out"],
            source: """
                const uint lane = thread_index_in_simdgroup, h = simdgroup_index_in_threadgroup;
                const int t = thread_position_in_grid.y, b = thread_position_in_grid.z;
                const int T = q_shape[1], HD = q_shape[2], len = lengths[b];
                const size_t row = (size_t)b * T, col = h * 128 + lane * 4;
                device const T_* qp = q + (row + t) * HD + col;
                const float4 qv = float4(float(qp[0]), float(qp[1]), float(qp[2]), float(qp[3]))
                    * float4(qscale[lane * 4], qscale[lane * 4 + 1], qscale[lane * 4 + 2], qscale[lane * 4 + 3]);
                float s[\(past)];
                float m = -INFINITY;
                for (int d = 0; d < \(past); ++d) {
                    const int kp = t - d;
                    if (kp < 0 || kp >= len) { s[d] = -INFINITY; continue; }
                    device const T_* kq = k + (row + kp) * HD + col;
                    const float4 kv = float4(float(kq[0]), float(kq[1]), float(kq[2]), float(kq[3])) * \(keyScale)f;
                    device const float* rp = rel + ((size_t)h * \(past + 1) + (\(past) - d)) * 128 + lane * 4;
                    const float4 rv = float4(rp[0], rp[1], rp[2], rp[3]);
                    float score = simd_sum(dot(qv, kv)) + simd_sum(dot(qv, rv));
                    score = metal::precise::tanh(score / \(config.attention_logit_cap)f) * \(config.attention_logit_cap)f;
                    s[d] = score; m = max(m, score);
                }
                float total = 0.0f;
                float4 acc = float4(0.0f);
                for (int d = 0; d < \(past); ++d) {
                    if (s[d] == -INFINITY) continue;
                    const float e = metal::precise::exp(s[d] - m);
                    total += e;
                    device const T_* vq = v + (row + t - d) * HD + col;
                    acc += e * float4(float(vq[0]), float(vq[1]), float(vq[2]), float(vq[3]));
                }
                const float4 o = total > 0.0f ? acc / total : float4(0.0f);
                device T_* op = out + (row + t) * HD + col;
                op[0] = T_(o[0]); op[1] = T_(o[1]); op[2] = T_(o[2]); op[3] = T_(o[3]);
                """) : nil
    }

    /// Mel frames per part → soft-token count, `ceil(ceil(frames / 2) / 2)`.
    static func tokens(frames: Int) -> Int { ((frames + 1) / 2 + 1) / 2 }

    /// `parts`: [frames_i, 128] BF16 log-mel, frames_i > 0. Returns [Σ tokens_i, text hidden].
    func callAsFunction(_ parts: [MLXArray], kernel: Bool = true) -> MLXArray {
        let lengths = parts.map { $0.dim(0) }
        let batch = parts.count, longest = lengths.max()!
        let eps = config.rms_norm_eps, limit = config.gradient_clipping
        // Zero padding equals the reference's masked zeroing of invalid input frames.
        var x = batch == 1 ? parts[0].expandedDimensions(axis: 0) : stacked(parts.map {
            $0.dim(0) == longest ? $0 : padded($0, widths: [.init((0, longest - $0.dim(0))), 0])
        })
        func valid(_ counts: [Int], _ length: Int) -> MLXArray {
            MLXArray(0..<length).reshaped(1, length) .< MLXArray(counts.map(Int32.init)).reshaped(batch, 1)
        }
        // Subsampling: two stride-2 3x3 convolutions, channel LayerNorm, ReLU.
        x = relu(MLXFast.layerNorm(conv2d(x.expandedDimensions(axis: -1).asType(conv0.dtype), conv0, stride: 2, padding: 1),
                                   weight: norm0, bias: nil, eps: eps))
        let half = lengths.map { ($0 + 1) / 2 }
        if batch > 1 { x = MLX.where(valid(half, x.dim(1)).reshaped(batch, -1, 1, 1), x, MLXArray.zeros([1], dtype: x.dtype)) }
        x = relu(MLXFast.layerNorm(conv2d(x, conv1, stride: 2, padding: 1), weight: norm1, bias: nil, eps: eps))
        let counts = half.map { ($0 + 1) / 2 }
        let length = x.dim(1)
        x = matmul(x.reshaped(batch, length, -1), inputProjection.T)
        let blocks = (length + chunk - 1) / chunk
        // Key validity per sequence: inside [0, length_b) in the strided context windows.
        let keyIndex = (MLXArray(0..<blocks).reshaped(blocks, 1) * chunk - past + MLXArray(0..<context).reshaped(1, context))
        let keyValid = (keyIndex .>= 0).reshaped(1, blocks, context)
            .&& (keyIndex.reshaped(1, blocks, context) .< MLXArray(counts.map(Int32.init)).reshaped(batch, 1, 1))
        let mask = keyValid.reshaped(batch, 1, blocks, 1, context) .&& causal.reshaped(1, 1, 1, chunk, context)
        let lengthArray = MLXArray(counts.map(Int32.init))
        let validFrames = batch > 1 ? valid(counts, length).reshaped(batch, length, 1).asType(x.dtype) : nil
        let indices = (MLXArray(Array(stride(from: 0, to: blocks * chunk, by: chunk))).reshaped(blocks, 1)
                       + MLXArray(0..<context).reshaped(1, context)).flattened().asType(.int32)
        func feedForward(_ f: FeedForward, _ x: MLXArray) -> MLXArray {
            var h = MLXFast.rmsNorm(clip(x, min: -limit, max: limit), weight: f.pre, eps: eps)
            h = f.contract(silu(f.expand(h)))
            return x + MLXFast.rmsNorm(clip(h, min: -limit, max: limit), weight: f.post, eps: eps) * config.residual_weight
        }
        func contextBlocks(_ x: MLXArray) -> MLXArray {
            padded(x, widths: [0, .init((past, future + chunk - 1)), 0, 0]).take(indices, axis: 1)
                .reshaped(batch, blocks, context, heads, headDim).transposed(0, 3, 1, 2, 4)
        }
        for layer in layers {
            x = feedForward(layer.ff1, x)
            let residual = x
            let normed = MLXFast.rmsNorm(clip(x, min: -limit, max: limit), weight: layer.preAttention, eps: eps)
            var h: MLXArray
            if kernel, let fusedAttention {
                // One kernel: scaled q.k + relative bias, soft cap, causal window, softmax, values.
                let context = fusedAttention([layer.q(normed), layer.k(normed), layer.v(normed), layer.relativeRows,
                                              layer.queryScale, lengthArray],
                                             template: [("T_", x.dtype)], grid: (32 * heads, length, batch),
                                             threadGroup: (32 * heads, 1, 1), outputShapes: [[batch, length, heads * headDim]],
                                             outputDTypes: [x.dtype])[0]
                h = layer.post(context)
            } else {
            let q = (layer.q(normed).asType(.float32).reshaped(batch, length, heads, headDim) * layer.queryScale)
            let queries = padded(q, widths: [0, .init((0, blocks * chunk - length)), 0, 0])
                .reshaped(batch, blocks, chunk, heads, headDim).transposed(0, 3, 1, 2, 4)
            let keys = contextBlocks(layer.k(normed).asType(.float32).reshaped(batch, length, heads, headDim) * Float(log(1 + M_E) / log(2.0)))
            let values = contextBlocks(layer.v(normed).asType(.float32).reshaped(batch, length, heads, headDim))
            var scores = matmul(queries, keys.transposed(0, 1, 2, 4, 3))
            let relative = matmul(queries.reshaped(batch, heads, blocks * chunk, headDim), layer.relativeKeys)
            // Reference blocked relative shift.
            let width = relative.dim(-1)
            let shifted = padded(relative.reshaped(batch, heads, blocks, chunk, width),
                                 widths: [0, 0, 0, 0, .init((0, context + 1 - width))])
                .reshaped(batch, heads, blocks, chunk * (context + 1))[0..., 0..., 0..., 0..<(chunk * context)]
                .reshaped(batch, heads, blocks, chunk, context)
            scores = scores + shifted
            scores = tanh(scores / config.attention_logit_cap) * config.attention_logit_cap
            scores = MLX.where(mask, scores, MLXArray(config.attention_invalid_logits_value))
            let attended = matmul(softmax(scores, axis: -1), values).transposed(0, 2, 3, 1, 4)
                .reshaped(batch, blocks * chunk, -1)[0..., 0..<length, 0...]
            h = layer.post(attended.asType(x.dtype))
            }
            x = residual + MLXFast.rmsNorm(clip(h, min: -limit, max: limit), weight: layer.postAttention, eps: eps)
            if let validFrames { x = x * validFrames }
            // Light causal depthwise convolution.
            h = layer.convStart(MLXFast.rmsNorm(x, weight: layer.convPre, eps: eps))
            let channels = h.dim(-1) / 2
            h = h[0..., 0..., 0..<channels] * sigmoid(h[0..., 0..., channels...])
            h = conv1d(padded(h, widths: [0, .init((config.conv_kernel_size - 1, 0)), 0]), layer.convWeight, groups: channels)
            h = silu(MLXFast.rmsNorm(clip(h, min: -limit, max: limit), weight: layer.convNorm, eps: eps))
            x = layer.convEnd(h) + x
            x = feedForward(layer.ff2, x)
            x = MLXFast.rmsNorm(clip(x, min: -limit, max: limit), weight: layer.out, eps: eps)
        }
        x = addMM(outputBias, x, outputWeight.T)
        x = matmul(MLXFast.rmsNorm(x, weight: MLXArray.mlxNone, eps: eps), embedWeight.T)
        if batch == 1 { return x[0] }
        return concatenated(counts.enumerated().map { x[$0.offset, 0..<$0.element] }, axis: 0)
    }
}
