import Foundation
import MLX
import MLXNN
import Tokenizers

/// `heimdall-embedding-worker --rerank <model dir>`: Qwen3-Reranker-0.6B scoring over JSON lines.
/// Method is the lab reference's (lab/reranker-2026-11-13/qwen_reranker.py): Qwen's yes/no prompt, the
/// system + instruction + query prefix computed once per request and its keys/values shared by every document batch,
/// documents as " " + text cut to `max_doc_tokens` then the suffix, length-sorted right-padded batches, score =
/// softmax over the final position's [no, yes] rows of the tied embedding, probability of yes.
/// Weights stay BF16 exactly as stored; activations BF16; only the 2-logit softmax is FP32.
enum Rerank {
    static let modelID = "Qwen/Qwen3-Reranker-0.6B"
    static let modelVersion = "e61197ed:bf16:rerank-v1"
    static let prefix = "<|im_start|>system\nJudge whether the Document meets the requirements based on the Query and the Instruct "
        + "provided. Note that the answer can only be \"yes\" or \"no\".<|im_end|>\n<|im_start|>user\n"
    static let suffix = "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
    static let instruct = "Given a search query about past conversations, retrieve the transcript lines that answer it"
    static let maxDocuments = 500
    /// Rows per document batch. Measured 13 Nov (Mac in use, informational): see README "Rerank mode".
    static let defaultBatch = 32
    static let defaultMaxDocTokens = 256

    struct Request: Codable {
        let id: UInt64
        var op: String? = nil
        var query: String? = nil
        var documents: [String]? = nil
        var max_doc_tokens: Int? = nil
        var instruction: String? = nil
        var batch: Int? = nil
    }
    struct Stats: Codable {
        let seconds: Double
        let load_seconds: Double
        let tokens: Int
        let peak_bytes: Int
        let active_bytes: Int
        let batch: Int
    }
    struct Reply: Codable {
        let id: UInt64?
        var error: String? = nil
        var model_id: String? = nil
        var model_version: String? = nil
        var scores: [Float]? = nil
        var stats: Stats? = nil
        var usage: Usage? = nil
        var mlx: [String: Int]? = nil
    }

    static func main(directory: URL) async {
        let model: Qwen3Reranker
        do {
            let start = ProcessInfo.processInfo.systemUptime
            model = try await Qwen3Reranker(directory: directory)
            fputs("rerank: loaded in \(String(format: "%.2f", ProcessInfo.processInfo.systemUptime - start)) s\n", stderr)
        } catch RerankError.paused(let message) {
            fputs("rerank paused: \(message)\n", stderr); exit(75)
        } catch {
            fputs("Qwen3-Reranker: \(error.localizedDescription)\n", stderr); exit(1)
        }
        do {
            while let data = try Worker.line(limit: 16 << 20) {
                autoreleasepool {
                    var reply = Reply(id: (try? JSONDecoder().decode(IDOnly.self, from: data))?.id)
                    do {
                        let request = try JSONDecoder().decode(Request.self, from: data)
                        switch request.op {
                        case "usage":
                            reply.usage = Usage.current()
                            reply.mlx = ["active": Memory.activeMemory, "cache": Memory.cacheMemory, "peak": Memory.peakMemory]
                        case nil:
                            try model.admit()
                            reply.scores = try model.score(request, stats: &reply.stats)
                            reply.model_id = modelID; reply.model_version = modelVersion
                        default: throw WorkerError.invalid("unsupported op")
                        }
                    } catch RerankError.paused(let message) {
                        reply.error = "rerank paused: \(message)"
                    } catch { reply.error = error.localizedDescription }
                    do {
                        var output = try JSONEncoder().encode(reply); output.append(10)
                        try FileHandle.standardOutput.write(contentsOf: output)
                    } catch { fputs("rerank reply failed\n", stderr) }
                }
                Memory.clearCache()
            }
        } catch { fputs("rerank: \(error.localizedDescription)\n", stderr); exit(1) }
    }
    struct IDOnly: Codable { let id: UInt64 }
}

enum RerankError: Error { case paused(String) }

final class Qwen3Reranker {
    struct Layer {
        let q, k, v, o, qNorm, kNorm, inputNorm, postNorm, gate, up, down: MLXArray
    }
    let tokenizer: any Tokenizers.Tokenizer
    let embed: MLXArray, norm: MLXArray, head: MLXArray
    let layers: [Layer]
    let heads: Int, kvHeads: Int, headDim: Int, eps: Float, theta: Float, scale: Float
    let suffixIDs: [Int]
    let loadSeconds: Double
    var swapSamples: [(time: Double, swap: UInt64)] = []
    static let swapWindow = 120.0
    static let minimumFree: UInt64 = 3 << 30

    init(directory: URL) async throws {
        // Admission first: nothing is loaded while memory is short.
        let memory = try MemoryState.current()
        guard memory.free >= Self.minimumFree else { throw RerankError.paused("low free memory") }
        swapSamples = [(ProcessInfo.processInfo.systemUptime, memory.swap)]
        // Uncapped, the lab run peaked near 5 GB and grew swap; weights are 1.2 GB.
        Memory.cacheLimit = 256 << 20
        Memory.memoryLimit = 3 << 30
        let configURL = directory.appendingPathComponent("config.json")
        let weightsURL = directory.appendingPathComponent("model.safetensors")
        guard FileManager.default.fileExists(atPath: configURL.path),
              FileManager.default.fileExists(atPath: weightsURL.path),
              FileManager.default.fileExists(atPath: directory.appendingPathComponent("tokenizer.json").path) else {
            throw WorkerError.invalid("reranker model directory incomplete: \(directory.path)")
        }
        guard let config = try JSONSerialization.jsonObject(with: Data(contentsOf: configURL)) as? [String: Any],
              config["model_type"] as? String == "qwen3",
              let hidden = config["hidden_size"] as? Int, let count = config["num_hidden_layers"] as? Int,
              let heads = config["num_attention_heads"] as? Int, let kvHeads = config["num_key_value_heads"] as? Int,
              let headDim = config["head_dim"] as? Int, let eps = (config["rms_norm_eps"] as? NSNumber)?.floatValue,
              config["tie_word_embeddings"] as? Bool == true
        else { throw WorkerError.invalid("unexpected reranker config") }
        guard config["rope_scaling"] == nil || config["rope_scaling"] is NSNull else {
            throw WorkerError.invalid("rope scaling unsupported")
        }
        self.heads = heads; self.kvHeads = kvHeads; self.headDim = headDim; self.eps = eps
        self.theta = (config["rope_theta"] as? NSNumber)?.floatValue ?? 1_000_000
        self.scale = Float(pow(Double(headDim), -0.5))

        let start = ProcessInfo.processInfo.systemUptime
        let weights = try loadArrays(url: weightsURL)
        func weight(_ name: String) throws -> MLXArray {
            guard let array = weights["model." + name] else { throw WorkerError.invalid("missing weight \(name)") }
            guard array.dtype == .bfloat16 else { throw WorkerError.invalid("weight \(name) is not bfloat16") }
            return array
        }
        embed = try weight("embed_tokens.weight")
        norm = try weight("norm.weight")
        guard embed.shape.count == 2, embed.dim(1) == hidden else { throw WorkerError.invalid("embedding shape") }
        layers = try (0..<count).map { i in
            let p = "layers.\(i)."
            return Layer(q: try weight(p + "self_attn.q_proj.weight"), k: try weight(p + "self_attn.k_proj.weight"),
                v: try weight(p + "self_attn.v_proj.weight"), o: try weight(p + "self_attn.o_proj.weight"),
                qNorm: try weight(p + "self_attn.q_norm.weight"), kNorm: try weight(p + "self_attn.k_norm.weight"),
                inputNorm: try weight(p + "input_layernorm.weight"), postNorm: try weight(p + "post_attention_layernorm.weight"),
                gate: try weight(p + "mlp.gate_proj.weight"), up: try weight(p + "mlp.up_proj.weight"),
                down: try weight(p + "mlp.down_proj.weight"))
        }
        tokenizer = try await AutoTokenizer.from(modelFolder: directory)
        guard let yes = tokenizer.convertTokenToId("yes"), let no = tokenizer.convertTokenToId("no") else {
            throw WorkerError.invalid("yes/no tokens missing")
        }
        head = embed[MLXArray([Int32(no), Int32(yes)])]  // (2, D): [no, yes]
        suffixIDs = tokenizer.encode(text: Rerank.suffix, addSpecialTokens: false)
        var all = [embed, norm, head]
        for l in layers { all += [l.q, l.k, l.v, l.o, l.qNorm, l.kNorm, l.inputNorm, l.postNorm, l.gate, l.up, l.down] }
        eval(all)
        loadSeconds = ProcessInfo.processInfo.systemUptime - start
        Memory.clearCache()
    }

    /// Before each request: refuse while free memory is short or swap grew within the window (embedder's rule).
    func admit() throws {
        let now = ProcessInfo.processInfo.systemUptime
        let memory = try MemoryState.current()
        swapSamples.removeAll { now - $0.time > Self.swapWindow }
        swapSamples.append((now, memory.swap))
        let lowest = swapSamples.map(\.swap).min() ?? memory.swap
        guard memory.free >= Self.minimumFree else { Memory.clearCache(); throw RerankError.paused("low free memory") }
        guard memory.swap <= lowest + (16 << 20) else { Memory.clearCache(); throw RerankError.paused("swap growing") }
    }

    func encode(_ text: String) -> [Int] { tokenizer.encode(text: text, addSpecialTokens: false) }

    /// mlx_lm's `swiglu` (shapeless compiled silu(gate) * up), so the fused rounding matches the reference.
    static let swiglu: @Sendable (MLXArray, MLXArray) -> MLXArray = compile(shapeless: true) { gate, up in silu(gate) * up }

    @inline(__always) func linear(_ x: MLXArray, _ w: MLXArray) -> MLXArray { matmul(x, w.T) }

    /// One decoder layer over `h` [B, L, D] at positions offset ..< offset + L, attending over `prefix` keys/values
    /// (shared, batch 1) followed by its own. Returns the new hidden state and this layer's own keys/values.
    func layer(_ l: Layer, _ h: MLXArray, offset: Int, prefix: (MLXArray, MLXArray)?, full: Bool = true)
        -> (MLXArray?, MLXArray, MLXArray)
    {
        let (B, L) = (h.dim(0), h.dim(1))
        let x = MLXFast.rmsNorm(h, weight: l.inputNorm, eps: eps)
        var q = MLXFast.rmsNorm(linear(x, l.q).reshaped(B, L, heads, -1), weight: l.qNorm, eps: eps).transposed(0, 2, 1, 3)
        var k = MLXFast.rmsNorm(linear(x, l.k).reshaped(B, L, kvHeads, -1), weight: l.kNorm, eps: eps).transposed(0, 2, 1, 3)
        let v = linear(x, l.v).reshaped(B, L, kvHeads, -1).transposed(0, 2, 1, 3)
        q = MLXFast.RoPE(q, dimensions: headDim, traditional: false, base: theta, scale: 1, offset: offset)
        k = MLXFast.RoPE(k, dimensions: headDim, traditional: false, base: theta, scale: 1, offset: offset)
        guard full else { return (nil, k, v) }
        var keys = k, values = v
        if let (pk, pv) = prefix {
            keys = concatenated([broadcast(pk, to: [B, pk.dim(1), pk.dim(2), pk.dim(3)]), k], axis: 2)
            values = concatenated([broadcast(pv, to: [B, pv.dim(1), pv.dim(2), pv.dim(3)]), v], axis: 2)
        }
        // Causal, aligned to the last key: row i sees the prefix and its own tokens up to i; right padding is never seen.
        let attended = MLXFast.scaledDotProductAttention(queries: q, keys: keys, values: values, scale: scale, mask: .causal)
        var out = h + linear(attended.transposed(0, 2, 1, 3).reshaped(B, L, -1), l.o)
        let y = MLXFast.rmsNorm(out, weight: l.postNorm, eps: eps)
        out = out + linear(Self.swiglu(linear(y, l.gate), linear(y, l.up)), l.down)
        return (out, k, v)
    }

    func score(_ request: Rerank.Request, stats: inout Rerank.Stats?) throws -> [Float] {
        let start = ProcessInfo.processInfo.systemUptime
        Memory.peakMemory = 0
        let documents = request.documents ?? []
        guard documents.count <= Rerank.maxDocuments else { throw WorkerError.invalid("too many documents (max \(Rerank.maxDocuments))") }
        let maxDoc = max(1, min(request.max_doc_tokens ?? Rerank.defaultMaxDocTokens, 2048))
        let batch = max(1, min(request.batch ?? Rerank.defaultBatch, 128))
        let query = request.query ?? ""
        let instruction = request.instruction ?? Rerank.instruct
        var scores = [Float](repeating: 0, count: documents.count)
        var tokens = 0
        if !documents.isEmpty {
            let prefixIDs = encode(Rerank.prefix + "<Instruct>: \(instruction)\n<Query>: \(query)\n<Document>:")
            // Prefix keys/values once per request; the last layer's attention and MLP are not needed for them.
            var cache: [(MLXArray, MLXArray)] = []
            var h = embed[MLXArray(prefixIDs.map(Int32.init)).reshaped(1, -1)]
            for (i, l) in layers.enumerated() {
                let (next, k, v) = layer(l, h, offset: 0, prefix: nil, full: i < layers.count - 1)
                cache.append((k, v))
                if let next { h = next }
            }
            eval(cache.flatMap { [$0.0, $0.1] })
            let P = prefixIDs.count
            let sequences = documents.map { Array(encode(" " + $0).prefix(maxDoc)) + suffixIDs }
            tokens = sequences.reduce(0) { $0 + $1.count }
            // Stable like Python's sorted(): ties keep input order, so batches match the reference.
            let order = sequences.indices.sorted { (sequences[$0].count, $0) < (sequences[$1].count, $1) }
            for s in stride(from: 0, to: order.count, by: batch) {
                let index = Array(order[s ..< min(s + batch, order.count)])
                let B = index.count
                let L = index.map { sequences[$0].count }.max()!
                var flat: [Int32] = []
                flat.reserveCapacity(B * L)
                for i in index { flat += sequences[i].map(Int32.init) + [Int32](repeating: 0, count: L - sequences[i].count) }
                var x = embed[MLXArray(flat).reshaped(B, L)]
                for (l, kv) in zip(layers, cache) { x = layer(l, x, offset: P, prefix: kv).0! }
                let last = MLXArray(index.map { Int32(sequences[$0].count - 1) })
                let rows = takeAlong(x, broadcast(last.reshaped(B, 1, 1), to: [B, 1, x.dim(2)]), axis: 1).reshaped(B, -1)
                let logits = matmul(MLXFast.rmsNorm(rows, weight: norm, eps: eps), head.T).asType(.float32)
                let p = softmax(logits, axis: -1)[0..., 1]
                eval(p)
                for (j, value) in zip(index, p.asArray(Float.self)) { scores[j] = value }
            }
        }
        guard scores.allSatisfy(\.isFinite) else { throw WorkerError.invalid("nonfinite rerank score") }
        stats = .init(seconds: ProcessInfo.processInfo.systemUptime - start, load_seconds: loadSeconds, tokens: tokens,
            peak_bytes: Memory.peakMemory, active_bytes: Memory.activeMemory, batch: batch)
        return scores
    }
}
