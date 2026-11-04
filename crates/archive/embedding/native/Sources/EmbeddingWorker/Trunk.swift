// Native EmbeddingGemma 2 text trunk over the loaded checkpoint parameters.
//
// With `options = []` it computes exactly the upstream mlx-swift-lm graph (bit-identical,
// checked by the lab parity script). Options:
//   .floatNorms  norm weights cast to FP32 once at load instead of per call (same values)
//   .smallM      SmallMGEMM tensor tile for every projection at 9...256 rows, B's 2048->512
//                tile up to 768 rows (FP32 accumulation order only)
//   .noMask      no attention-mask arrays when no row is padded and L fits the window
// Serving embeds one row (`valid: nil`). Multi-row input (lab batching probe) pads to the
// longest row, masks padding in attention and pooling, and fills soft tokens in
// batch-then-sequence order.
import Foundation
import MLX
import MLXNN

struct TrunkOptions: OptionSet {
    let rawValue: Int
    static let floatNorms = TrunkOptions(rawValue: 1)
    static let smallM = TrunkOptions(rawValue: 2)
    static let noMask = TrunkOptions(rawValue: 4)
    static let all: TrunkOptions = [.floatNorms, .smallM, .noMask]
}

final class Trunk {
    struct Norm {
        let weight: MLXArray, float: MLXArray?
        func callAsFunction(_ x: MLXArray, eps: Float) -> MLXArray {
            MLXFast.rmsNorm(x, weight: float ?? weight.asType(.float32), eps: eps).asType(x.dtype)
        }
    }
    struct Layer {
        let q: MLXArray, k: MLXArray, v: MLXArray, o: MLXArray
        let qNorm: Norm, kNorm: Norm
        let gate: MLXArray, up: MLXArray, down: MLXArray
        let pleGate: MLXArray, pleProjection: MLXArray, pleNorm: Norm
        let inputNorm: Norm, postAttention: Norm, preFeedforward: Norm, postFeedforward: Norm
        let scalar: MLXArray
        let heads: Int, kvHeads: Int, headDim: Int, ropeBase: Float, global: Bool
        let vWeight: MLXArray
    }

    let options: TrunkOptions
    let embedTokens: MLXArray, pleProjection: MLXArray, pleNorm: Norm, finalNorm: Norm, projection: MLXArray
    let layers: [Layer]
    let hidden: Int, perLayer: Int, window: Int, eps: Float
    let softTokenIDs: [Int32]
    let tile = SmallMGEMM.tensorOpsAvailable

    init(parameters: [String: MLXArray], config: [String: Any], options: TrunkOptions) throws {
        self.options = options
        guard let text = config["text_config"] as? [String: Any],
              let hidden = text["hidden_size"] as? Int, let count = text["num_hidden_layers"] as? Int,
              let heads = text["num_attention_heads"] as? Int, let kvHeads = text["num_key_value_heads"] as? Int,
              let headDim = text["head_dim"] as? Int, let perLayer = text["hidden_size_per_layer_input"] as? Int,
              let window = text["sliding_window"] as? Int, let eps = (text["rms_norm_eps"] as? NSNumber)?.floatValue,
              let types = text["layer_types"] as? [String], types.count == count,
              let ropes = text["rope_parameters"] as? [String: [String: Any]]
        else { throw WorkerError.invalid("unsupported text configuration") }
        let overrides = text["per_layer_config"] as? [String: [String: Int]] ?? [:]
        func p(_ key: String) throws -> MLXArray {
            guard let value = parameters["language_model." + key] else { throw WorkerError.invalid("missing trunk parameter \(key)") }
            return value
        }
        func norm(_ key: String) throws -> Norm {
            let weight = try p(key)
            return Norm(weight: weight, float: options.contains(.floatNorms) ? weight.asType(.float32) : nil)
        }
        self.hidden = hidden; self.perLayer = perLayer; self.window = window; self.eps = eps
        embedTokens = try p("embed_tokens.weight")
        pleProjection = try p("ple.per_layer_model_projection.weight")
        pleNorm = try norm("ple.per_layer_projection_norm.weight")
        finalNorm = try norm("norm.weight")
        projection = try p("embedding_projection.weight")
        softTokenIDs = ["image_token_id", "video_token_id", "audio_token_id"].compactMap { (config[$0] as? Int).map(Int32.init) }
        var built: [Layer] = []
        for i in 0..<count {
            let key = "layers.\(i)."
            let override = overrides.first { Int($0.key) == i }?.value
            guard let theta = (ropes[types[i]]?["rope_theta"] as? NSNumber)?.floatValue else {
                throw WorkerError.invalid("missing rope parameters")
            }
            let dim = override?["head_dim"] ?? headDim
            built.append(Layer(
                q: try p(key + "self_attn.q_proj.weight"), k: try p(key + "self_attn.k_proj.weight"),
                v: try p(key + "self_attn.v_proj.weight"), o: try p(key + "self_attn.o_proj.weight"),
                qNorm: try norm(key + "self_attn.q_norm.weight"), kNorm: try norm(key + "self_attn.k_norm.weight"),
                gate: try p(key + "mlp.gate_proj.weight"), up: try p(key + "mlp.up_proj.weight"), down: try p(key + "mlp.down_proj.weight"),
                pleGate: try p(key + "ple_block.per_layer_input_gate.weight"),
                pleProjection: try p(key + "ple_block.per_layer_projection.weight"),
                pleNorm: try norm(key + "ple_block.post_per_layer_input_norm.weight"),
                inputNorm: try norm(key + "input_layernorm.weight"), postAttention: try norm(key + "post_attention_layernorm.weight"),
                preFeedforward: try norm(key + "pre_feedforward_layernorm.weight"),
                postFeedforward: try norm(key + "post_feedforward_layernorm.weight"),
                scalar: try p(key + "layer_scalar"),
                heads: heads, kvHeads: override?["num_key_value_heads"] ?? kvHeads, headDim: dim, ropeBase: theta,
                global: types[i] == "full_attention", vWeight: MLXArray.ones([dim], dtype: .float32)))
        }
        layers = built
        eval(built.flatMap { [$0.qNorm.float, $0.kNorm.float, $0.pleNorm.float, $0.inputNorm.float, $0.postAttention.float,
                              $0.preFeedforward.float, $0.postFeedforward.float, $0.vWeight].compactMap { $0 } }
             + [pleNorm.float, finalNorm.float].compactMap { $0 })
    }

    /// Weights quantized by the mixed-precision spec, keyed "<layer>.<projection>" (e.g. "3.down") or "embed".
    var quantized: [String: QuantizedWeight] = [:]

    func linear(_ x: MLXArray, _ w: MLXArray, _ key: String? = nil, _ fast: Fast) -> MLXArray {
        if let key, let q = quantized[key] { return q.apply(x) }
        if fast.smallM && options.contains(.smallM) {
            let rows = x.size / x.dim(-1)
            if fast.tile, tile, w.dim(0) == 512, w.dim(1) == 2048, (9...768).contains(rows), x.dtype == w.dtype {
                return TileLinear.apply(x, w)
            }
            if SmallMGEMM.tileRows.contains(rows), let out = SmallMGEMM.matmul(x, .dense(w)) { return out }
        }
        return matmul(x, w.T)
    }

    /// Token embeddings; a quantized table dequantizes only the rows it looks up.
    func embed(_ ids: MLXArray) -> MLXArray {
        guard let q = quantized["embed"] else { return take(embedTokens, ids, axis: 0) }
        return q.rows(ids).asType(embedTokens.dtype)
    }

    /// `ids` [B, L]; `valid` [B, L] (nil: no padding); `soft` [Σ placeholders, hidden].
    /// Returns [B, 768] FP32 unit vectors.
    func callAsFunction(ids: MLXArray, valid: MLXArray?, soft: MLXArray?, fast: Fast = .plain) -> MLXArray {
        let (batch, length) = (ids.dim(0), ids.dim(1))
        let mask = valid ?? (options.contains(.noMask) ? nil : MLXArray.ones([batch, length], dtype: .int32))
        var x = embed(ids)
        x = x * MLXArray(Float(hidden).squareRoot()).asType(x.dtype)
        if let soft, let first = softTokenIDs.first {
            let isSoft = softTokenIDs.dropFirst().reduce(ids .== first) { $0 .|| (ids .== $1) }
            let index = maximum(cumsum(isSoft.flattened().asType(.int32), axis: 0) - 1, 0)
            let rows = soft.asType(x.dtype).reshaped(-1, hidden).take(index, axis: 0).reshaped(batch, length, hidden)
            x = MLX.where(isSoft.expandedDimensions(axis: -1), rows, x)
        }
        // Per-layer inputs.
        let projected = linear(x, pleProjection, "ple", fast) * (1 / Float(hidden).squareRoot())
        let perLayerInputs = pleNorm(projected.reshaped(batch, length, layers.count, perLayer), eps: eps)
        // Spans, as upstream.
        let global: EmbeddingGemma2Span = .full(Masks.bidirectional(batch: batch, seqLen: length, paddingMask: mask))
        let blocks = (length + window - 1) / window
        let local: EmbeddingGemma2Span =
            if 3 * blocks * window * window < length * length {
                .window(radius: window, validKeys: mask)
            } else if length - 1 > window {
                .full(Masks.combine(pattern: Masks.slidingWindowPattern(seqLen: length, radius: window),
                                    batch: batch, seqLen: length, paddingMask: mask))
            } else {
                global
            }
        for (index, layer) in layers.enumerated() {
            let span = layer.global ? global : local
            let key = "\(index)."
            let normed = layer.inputNorm(x, eps: eps)
            let q = MLXFast.RoPE(layer.qNorm(linear(normed, layer.q, key + "q", fast).reshaped(batch, length, layer.heads, layer.headDim), eps: eps)
                .transposed(0, 2, 1, 3), dimensions: layer.headDim, traditional: false, base: layer.ropeBase, scale: 1, offset: 0)
            let k = MLXFast.RoPE(layer.kNorm(linear(normed, layer.k, key + "k", fast).reshaped(batch, length, layer.kvHeads, layer.headDim), eps: eps)
                .transposed(0, 2, 1, 3), dimensions: layer.headDim, traditional: false, base: layer.ropeBase, scale: 1, offset: 0)
            let rawValues = linear(normed, layer.v, key + "v", fast).reshaped(batch, length, layer.kvHeads, layer.headDim)
            let normedValues = MLXFast.rmsNorm(rawValues, weight: layer.vWeight, eps: eps).asType(rawValues.dtype)
            let attended = span.attention(queries: q, keys: k, values: normedValues.transposed(0, 2, 1, 3), fast: fast)
                .transposed(0, 2, 1, 3).reshaped(batch, length, -1)
            let ff: MLXArray
            if fast.compiled {
                x = Compiled.residualNorm(x, linear(attended, layer.o, key + "o", fast), layer.postAttention.float ?? layer.postAttention.weight.asType(.float32))
                ff = layer.preFeedforward(x, eps: eps)
                let hidden = Compiled.geluGate(linear(ff, layer.gate, key + "gate", fast), linear(ff, layer.up, key + "up", fast))
                x = Compiled.residualNorm(x, linear(hidden, layer.down, key + "down", fast), layer.postFeedforward.float ?? layer.postFeedforward.weight.asType(.float32))
                let gated = Compiled.geluGate(linear(x, layer.pleGate, key + "pleGate", fast), perLayerInputs[0..., 0..., index, 0...])
                x = Compiled.residualNormScale(x, linear(gated, layer.pleProjection, key + "pleProjection", fast),
                                               layer.pleNorm.float ?? layer.pleNorm.weight.asType(.float32), layer.scalar)
            } else {
                x = x + layer.postAttention(linear(attended, layer.o, key + "o", fast), eps: eps)
                ff = layer.preFeedforward(x, eps: eps)
                x = x + layer.postFeedforward(linear(geluApproximate(linear(ff, layer.gate, key + "gate", fast)) * linear(ff, layer.up, key + "up", fast), layer.down, key + "down", fast), eps: eps)
                let gated = geluApproximate(linear(x, layer.pleGate, key + "pleGate", fast)) * perLayerInputs[0..., 0..., index, 0...]
                x = (x + layer.pleNorm(linear(gated, layer.pleProjection, key + "pleProjection", fast), eps: eps)) * layer.scalar
            }
        }
        WindowPlan.shared.clear()
        x = finalNorm(x, eps: eps)
        let pooled: MLXArray
        if let mask {
            let expanded = mask.expandedDimensions(axes: [2]).asType(.float32)
            pooled = (x.asType(.float32) * expanded).sum(axis: 1) / maximum(expanded.sum(axis: 1), MLXArray(1e-9))
        } else {
            // Same arithmetic as the all-ones mask: FP32 sum over L divided by L.
            pooled = x.asType(.float32).sum(axis: 1) / MLXArray(Float(length))
        }
        let vector = matmul(pooled.asType(x.dtype), projection.T).asType(.float32)
        let norm = maximum(MLX.sqrt((vector * vector).sum(axis: -1, keepDims: true)), MLXArray(1e-9))
        return vector / norm
    }
}

// Upstream span/mask helpers (mlx-swift-lm c53d7d48, EmbeddingGemma2.swift), copied
// because they are internal to that module.
enum EmbeddingGemma2Span {
    case full(MLXArray?)
    case window(radius: Int, validKeys: MLXArray?)

    func attention(queries: MLXArray, keys: MLXArray, values: MLXArray, fast: Fast = .plain) -> MLXArray {
        switch self {
        case .full(let mask):
            return Attention.full(queries: queries, keys: keys, values: values, mask: mask, fast: fast)
        case .window(let radius, let validKeys):
            let (batch, length) = (queries.dim(0), queries.dim(2))
            let blocks = (length + radius - 1) / radius
            let span = 3 * radius
            let tail = blocks * radius - length
            let context = IntOrPair((radius, radius + tail))
            // Identical index/mask arrays for every local layer of one forward: build once.
            let plan = WindowPlan.shared.get(batch: batch, length: length, radius: radius, validKeys: validKeys)
            let indices = plan.indices
            func blocked(_ x: MLXArray) -> MLXArray {
                MLX.padded(x, widths: [0, 0, context, 0])
                    .take(indices, axis: 2)
                    .reshaped(batch, x.dim(1), blocks, span, x.dim(3))
                    .transposed(0, 2, 1, 3, 4)
                    .reshaped(batch * blocks, x.dim(1), span, x.dim(3))
            }
            let blockQueries = MLX.padded(queries, widths: [0, 0, .init((0, tail)), 0])
                .reshaped(batch, queries.dim(1), blocks, radius, queries.dim(3))
                .transposed(0, 2, 1, 3, 4)
                .reshaped(batch * blocks, queries.dim(1), radius, queries.dim(3))
            let mask = plan.mask
            let output = Attention.full(queries: blockQueries, keys: blocked(keys), values: blocked(values), mask: mask, fast: fast)
            return output.reshaped(batch, blocks, queries.dim(1), radius, values.dim(3))
                .transposed(0, 2, 1, 3, 4)
                .reshaped(batch, queries.dim(1), blocks * radius, values.dim(3))[
                    0..., 0..., ..<length, 0...]
        }
    }
}

/// Per-forward cache of the window path's index and mask arrays (rebuilt when the
/// shape or validity array changes; cleared after each forward).
final class WindowPlan {
    nonisolated(unsafe) static let shared = WindowPlan()
    var key: (Int, Int, Int, ObjectIdentifier?)? = nil
    var indices = MLXArray(0), mask = MLXArray(0)
    func clear() { key = nil; indices = MLXArray(0); mask = MLXArray(0) }
    func get(batch: Int, length: Int, radius: Int, validKeys: MLXArray?) -> WindowPlan {
        let id = validKeys.map { ObjectIdentifier($0) }
        if let key, key.0 == batch, key.1 == length, key.2 == radius, key.3 == id { return self }
        let blocks = (length + radius - 1) / radius, span = 3 * radius, tail = blocks * radius - length
        indices = (MLXArray(0 ..< blocks).reshaped(blocks, 1) * radius + MLXArray(0 ..< span).reshaped(1, span)).flattened()
        let row = MLXArray(0 ..< radius).reshaped(radius, 1)
        let column = MLXArray(0 ..< span).reshaped(1, span)
        let valid = MLX.padded(
            (validKeys ?? MLXArray.ones([batch, length])).asType(.bool),
            widths: [0, IntOrPair((radius, radius + tail))], value: MLXArray(false)
        )
        .take(indices, axis: 1)
        .reshaped(batch * blocks, 1, 1, span)
        mask = ((column .>= row) .&& (column .<= row + 2 * radius) .&& valid) .|| (column .== row + radius)
        key = (batch, length, radius, id)
        return self
    }
}

enum Masks {
    static func bidirectional(batch: Int, seqLen: Int, paddingMask: MLXArray?) -> MLXArray? {
        guard let paddingMask else { return nil }
        let validKeys = paddingMask.asType(.bool).reshaped(batch, 1, 1, seqLen)
        return validKeys .|| logicalNot(validKeys.any(axis: -1, keepDims: true))
    }

    static func slidingWindowPattern(seqLen: Int, radius: Int) -> MLXArray {
        let rows = MLXArray(0 ..< seqLen).reshaped(seqLen, 1)
        let columns = MLXArray(0 ..< seqLen).reshaped(1, seqLen)
        return (abs(rows - columns) .<= MLXArray(Int32(radius))).reshaped(1, 1, seqLen, seqLen)
    }

    static func combine(pattern: MLXArray, batch: Int, seqLen: Int, paddingMask: MLXArray?) -> MLXArray {
        guard let paddingMask else { return pattern }
        let validKeys = paddingMask.asType(.bool).reshaped(batch, 1, 1, seqLen)
        let diagonal =
            (MLXArray(0 ..< seqLen).reshaped(seqLen, 1)
            .== MLXArray(0 ..< seqLen).reshaped(1, seqLen))
            .reshaped(1, 1, seqLen, seqLen)
        return (pattern .&& validKeys) .|| diagonal
    }
}
