import Foundation
import Darwin
import MLX
import MLXNN
import MLXLMCommon
import Tokenizers

let modelID = "google/embeddinggemma-2"
let modelVersion = "914f7f89142e33e77833254d9c9b90c3cef7303b:mlx-swift-c53d7d48:bf16:native-v2:input-v1"

@_silgen_name("proc_pid_rusage")
private func processUsage(_ pid: Int32, _ flavor: Int32, _ info: UnsafeMutableRawPointer) -> Int32

struct Usage: Codable {
    let cpu_s: Double
    let footprint_bytes: UInt64
    let peak_footprint_bytes: UInt64
    let energy_j: Double?
    static func current() -> Self {
        var raw = [UInt64](repeating: 0, count: 128)
        let energy = raw.withUnsafeMutableBytes { processUsage(getpid(), 6, $0.baseAddress!) == 0 }
        if !energy { _ = raw.withUnsafeMutableBytes { processUsage(getpid(), 4, $0.baseAddress!) } }
        var timebase = mach_timebase_info_data_t()
        mach_timebase_info(&timebase)
        return .init(cpu_s: Double(raw[2] + raw[3]) * Double(timebase.numer) / Double(timebase.denom) / 1e9,
            footprint_bytes: raw[9], peak_footprint_bytes: raw[30], energy_j: energy ? Double(raw[42]) / 1e9 : nil)
    }
}
struct MemoryState {
    /// Memory the system can hand out without swapping: free (with speculative) and purgeable pages, plus the
    /// inactive pages that are file-backed (at most min(inactive, file-backed); inactive anonymous memory would have
    /// to be compressed or swapped, so it is not counted). Free pages alone stay low on a busy Mac with tens of GB
    /// of reclaimable cache (18 Oct: admission refused at 86 GB available).
    let free: UInt64
    let swap: UInt64
    static func current() throws -> Self {
        var vm = vm_statistics64_data_t()
        var count = mach_msg_type_number_t(MemoryLayout<vm_statistics64_data_t>.size / MemoryLayout<integer_t>.size)
        let result = withUnsafeMutablePointer(to: &vm) { pointer in
            pointer.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                host_statistics64(mach_host_self(), HOST_VM_INFO64, $0, &count)
            }
        }
        var swap = xsw_usage(), size = MemoryLayout<xsw_usage>.size
        guard result == KERN_SUCCESS, sysctlbyname("vm.swapusage", &swap, &size, nil, 0) == 0 else {
            throw WorkerError.invalid("memory counters unavailable")
        }
        // host_statistics64 free_count already includes speculative pages.
        let fileInactive = min(UInt64(vm.inactive_count), UInt64(vm.external_page_count))
        let pages = UInt64(vm.free_count) + UInt64(vm.purgeable_count) + fileInactive
        return .init(free: pages * UInt64(vm_kernel_page_size), swap: swap.xsu_used)
    }
}
struct Part: Codable {
    let type: String
    var text: String?
    let path: String?
    var frames: [String]?
}
struct Input: Codable {
    var parts: [Part]
    let query: Bool
    let weight: Float
}
struct Request: Codable {
    let id: UInt64
    var inputs: [Input]? = nil
    var items: [[Input]]? = nil
    let dimensions: Int?
    let op: String?
    /// Lab only (HEIMDALL_EMBED_LAB=1): mixed-precision spec / fast parts for this process.
    var quant: String? = nil
    var fast: String? = nil
}
struct Stats: Codable { let seconds: Double; let tokens: Int; let windows: Int; var groups: Int? = nil }
struct Reply: Codable {
    let id: UInt64?
    var vector: [Float]? = nil
    var vectors: [[Float]?]? = nil
    var errors: [String?]? = nil
    var fast: [String]? = nil
    var quant_bytes: Int? = nil
    /// MLX allocator: active, cache and peak bytes (peak since the last lab reset).
    var mlx: [String: Int]? = nil
    var stats: Stats? = nil
    var model_id: String? = nil
    var model_version: String? = nil
    var usage: Usage? = nil
    var error: String? = nil
}

// Lab-only phase attribution (HEIMDALL_EMBED_PROFILE=1): forces an eval at each
// phase boundary, so its totals are attribution, never serving timings.
final class Profile {
    static let enabled = ProcessInfo.processInfo.environment["HEIMDALL_EMBED_PROFILE"] == "1"
    static var phases: [String: Double] = [:]
    static var last = 0.0
    static func start() { phases = [:]; last = ProcessInfo.processInfo.systemUptime }
    static func mark(_ name: String, _ arrays: MLXArray...) {
        guard enabled else { return }
        if !arrays.isEmpty { eval(arrays) }
        let now = ProcessInfo.processInfo.systemUptime
        phases[name, default: 0] += (now - last) * 1000; last = now
    }
    static func flush() {
        guard enabled, let data = try? JSONEncoder().encode(phases) else { return }
        FileHandle.standardError.write(data + Data([10]))
    }
}

/// Prompt token IDs without re-tokenizing thousands of media placeholders: text
/// runs are encoded exactly as the tokenizer splits them at added tokens, and
/// placeholder IDs are spliced between them. HEIMDALL_EMBED_TOKENIZE=check also
/// encodes the full prompt string and fails on any difference (lab gate).
struct Prompt {
    static let check = ProcessInfo.processInfo.environment["HEIMDALL_EMBED_TOKENIZE"] == "check"
    let runtime: Runtime
    var pending: String
    var full: String
    var body: [Int] = []
    init(runtime: Runtime, prefix: String) { self.runtime = runtime; pending = prefix; full = Self.check ? prefix : "" }
    mutating func text(_ value: String) { pending += value; if Self.check { full += value } }
    mutating func flush() {
        if !pending.isEmpty { body += runtime.tokenizer.encode(text: pending, addSpecialTokens: false) }
        pending = ""
    }
    mutating func media(open: String, token: String, count: Int, close: String) {
        flush()
        body.append(runtime.specialID(open))
        body += [Int](repeating: runtime.specialID(token), count: count)
        body.append(runtime.specialID(close))
        if Self.check { full += open + String(repeating: token, count: count) + close }
    }
    mutating func ids() throws -> [Int] {
        flush()
        let ids = runtime.bos + body + runtime.eos
        if Self.check && ids != runtime.tokenizer.encode(text: full, addSpecialTokens: true) {
            throw WorkerError.invalid("spliced prompt IDs differ from full-prompt tokenization")
        }
        return ids
    }
}

/// MLX's buffer cache. MLX keeps freed GPU buffers for reuse up to `Memory.cacheLimit`, so between requests the helper
/// held the last request's dead activations (up to the old 2 GiB cap) as GPU memory on top of the 1.5 GB of weights.
/// Serving caps the cache at `capped` and returns it to the system after every request, so an idle helper holds the
/// weights. HEIMDALL_EMBED_CACHE_MB (lab A/B only): a number of MB (still cleared after each request), or `legacy`
/// for the old policy (min(2 GiB, max(256 MB, free/16)), never cleared).
enum CachePolicy: Equatable {
    case capped(Int)
    case legacy
    /// 256 MB. Measured 30 Oct (lab/memory-cache/README.md, helpers interleaved, M5 Max in use): short inputs cost
    /// nothing; a 2,853-token text took 66 ms against the old policy's 56 (64 MB: 92 ms, 1 GiB: 65 ms, no better);
    /// vectors bit-identical. The clear is what frees the memory; a larger cap only refills after it.
    static let defaultBytes = 256 << 20
    static var current: CachePolicy { parse(ProcessInfo.processInfo.environment["HEIMDALL_EMBED_CACHE_MB"]) }
    static func parse(_ value: String?) -> CachePolicy {
        guard let value, !value.isEmpty else { return .capped(defaultBytes) }
        if value == "legacy" { return .legacy }
        return Int(value).map { .capped(max(0, $0) << 20) } ?? .capped(defaultBytes)
    }
    func limit(free: UInt64) -> Int {
        switch self {
        case .capped(let bytes): return bytes
        case .legacy: return Int(min(2 << 30, max(256 << 20, free / 16)))
        }
    }
    /// After a request: the cache goes back to the system (not under `legacy`).
    func release() { if self != .legacy { Memory.clearCache() } }
}

final class Runtime {
    let model: MLXLMCommon.EmbeddingGemma2
    let tokenizer: any Tokenizers.Tokenizer
    let media: Media
    let audioEncoder: AudioEncoder?
    let trunk: Trunk?
    let bos: [Int], eos: [Int]
    var specials: [String: Int] = [:]
    func specialID(_ token: String) -> Int { specials[token]! }
    /// Swap samples of the last `swapWindow` seconds: growth above the window's lowest sample pauses embedding. A
    /// steady creep (8 MB a minute or more) trips it; swap another process added once stops the index for at most the
    /// window, not the rest of the session.
    var swapSamples: [(time: Double, swap: UInt64)]
    static let swapWindow = 120.0
    let footprintLimit: UInt64
    var lastAdmission = 0.0
    /// Fast parts in use (Gate.resolve); `.plain` is stock MLX everywhere.
    var fast: Fast = .plain
    var quantSpec: [String: Int] = [:]
    var visionOriginals: [String: Linear] = [:]
    var compiledVision: (@Sendable (MLXArray) -> MLXArray)? = nil
    /// The first multi-row group of this process is compared with its B=1 vector.
    var batchChecked = false
    init(directory: URL, tokenizer: any Tokenizers.Tokenizer) throws {
        let memory = try MemoryState.current()
        guard memory.free >= 8 << 30 else { throw WorkerError.invalid("embedding admission needs 8 GiB free") }
        swapSamples = [(ProcessInfo.processInfo.systemUptime, memory.swap)]
        footprintLimit = min(8 << 30, memory.free / 4)
        Memory.memoryLimit = min(6 << 30, Int(memory.free / 4))
        Memory.cacheLimit = CachePolicy.current.limit(free: memory.free)
        let configuration = try JSONDecoder().decode(EmbeddingGemma2Configuration.self,
            from: Data(contentsOf: directory.appendingPathComponent("config.json")))
        model = MLXLMCommon.EmbeddingGemma2(configuration)
        // Eager components avoid async GPU state crossing request threads.
        try loadWeights(modelDirectory: directory, model: model, componentLoading: .immediate)
        model.update(parameters: model.parameters().mapValues { $0.dtype.isFloatingPoint ? $0.asType(.bfloat16) : $0 })
        eval(model)
        Memory.clearCache()
        self.tokenizer = tokenizer
        self.media = Media()
        // Added-token IDs and the post-processor's single-sequence wrapping.
        let empty = tokenizer.encode(text: "", addSpecialTokens: true)
        guard empty.count == 2 else { throw WorkerError.invalid("unexpected tokenizer post-processing") }
        bos = [empty[0]]; eos = [empty[1]]
        // Lab A/B controls only: HEIMDALL_EMBED_TRUNK=upstream (mlx-swift-lm graph) or
        // exact (native trunk without the tile kernels), HEIMDALL_EMBED_AUDIO=upstream.
        let environment = ProcessInfo.processInfo.environment
        let rootConfig = try JSONSerialization.jsonObject(with: Data(contentsOf: directory.appendingPathComponent("config.json"))) as? [String: Any] ?? [:]
        let parameters = Dictionary(uniqueKeysWithValues: model.parameters().flattened())
        trunk = environment["HEIMDALL_EMBED_TRUNK"] == "upstream" ? nil
            : try Trunk(parameters: parameters, config: rootConfig, options: environment["HEIMDALL_EMBED_TRUNK"] == "exact" ? [] : .all)
        if let trunk { Compiled.eps = trunk.eps }
        if environment["HEIMDALL_EMBED_AUDIO"] == "upstream" {
            audioEncoder = nil
        } else {
            let audioConfig = try JSONDecoder().decode(AudioConfig.self, from: JSONSerialization.data(withJSONObject: rootConfig["audio_config"] ?? [:]))
            audioEncoder = try AudioEncoder(parameters: parameters, config: audioConfig)
        }
        for token in ["<|image>", "<|image|>", "<image|>", "<|audio>", "<|audio|>", "<audio|>", "<|video|>"] {
            let ids = tokenizer.encode(text: token, addSpecialTokens: false)
            guard ids.count == 1 else { throw WorkerError.invalid("media token is not one added token") }
            specials[token] = ids[0]
        }
        try admit(force: true)
    }
    func admit(force: Bool = false) throws {
        let now = ProcessInfo.processInfo.systemUptime
        if !force && now - lastAdmission < 0.5 { return }
        let memory = try MemoryState.current()
        swapSamples.removeAll { now - $0.time > Self.swapWindow }
        swapSamples.append((now, memory.swap))
        let lowest = swapSamples.map(\.swap).min() ?? memory.swap
        let swapGrowing = memory.swap > lowest + (16 << 20)
        guard memory.free >= 4 << 30, !swapGrowing,
              Usage.current().footprint_bytes <= footprintLimit else {
            // Keep the weights resident; give the scratch buffers back while paused.
            Memory.clearCache()
            throw WorkerError.invalid("embedding paused: low free memory, swap growth or bounded footprint exceeded")
        }
        // Only a passed check is remembered: a refused one is checked again on the next call.
        lastAdmission = now
    }
    func textTokens(_ text: String) -> [Int] { tokenizer.encode(text: text, addSpecialTokens: false) }
    func windows(_ item: Input) throws -> [Input] {
        var parts: [(Part, Int)] = []
        for part in item.parts {
            switch part.type {
            case "text":
                guard let text = part.text else { throw WorkerError.invalid("missing text") }
                guard !["<|image|>", "<|audio|>", "<|video|>"].contains(where: text.contains) else {
                    throw WorkerError.invalid("literal model media token in text")
                }
                let n = textTokens(text).count
                if n <= 1024 { parts.append((part, n)); continue }
                // Split original Unicode text, never decode/reword tokens or drop a tail.
                let boundaries = Array(text.indices) + [text.endIndex]
                var start = 0
                while start < boundaries.count - 1 {
                    var low = start + 1, high = boundaries.count - 1
                    while low < high {
                        let middle = (low + high + 1) / 2
                        if textTokens(String(text[boundaries[start]..<boundaries[middle]])).count <= 1024 { low = middle } else { high = middle - 1 }
                    }
                    var chunk = part
                    chunk.text = String(text[boundaries[start]..<boundaries[low]])
                    parts.append((chunk, textTokens(chunk.text!).count)); start = low
                }
            case "audio":
                guard let path = part.path else { throw WorkerError.invalid("missing audio path") }
                let bytes = (try FileManager.default.attributesOfItem(atPath: path)[.size] as? NSNumber)?.intValue ?? 0
                parts.append((part, (bytes / 4 + 639) / 640 + 2))
            case "image": parts.append((part, 282))
            case "video":
                guard let frames = part.frames, !frames.isEmpty, frames.count <= 256 else { throw WorkerError.invalid("invalid video frames") }
                for i in stride(from: 0, to: frames.count, by: 20) {
                    var chunk = part; chunk.frames = Array(frames[i..<min(i + 20, frames.count)])
                    parts.append((chunk, chunk.frames!.count * 142))
                }
            default: throw WorkerError.invalid("unsupported input part")
            }
        }
        var output: [Input] = [], current: [Part] = [], count = 0
        for (part, cost) in parts {
            guard cost <= 6000 else { throw WorkerError.invalid("single media part exceeds bounded context") }
            if !current.isEmpty && count + cost > 6000 { output.append(Input(parts: current, query: item.query, weight: item.weight)); current = []; count = 0 }
            current.append(part); count += cost
        }
        if !current.isEmpty { output.append(Input(parts: current, query: item.query, weight: item.weight)) }
        return output
    }
    struct Prepared { let ids: [Int]; let soft: [MLXArray] }

    /// Audio features per mel part; all parts of one input share one padded encoder call.
    func encodeAudio(_ mels: [MLXArray]) throws -> [MLXArray] {
        guard !mels.isEmpty else { return [] }
        guard let audioEncoder else {
            return try mels.map { mel in
                guard let encoded = try model.audioFeatures(mel) else { throw WorkerError.invalid("audio encoder unavailable") }
                guard encoded.dim(1) == AudioEncoder.tokens(frames: mel.dim(0)) else { throw WorkerError.invalid("audio token geometry") }
                return encoded.reshaped(-1, 512)
            }
        }
        let encoded = audioEncoder(mels, kernel: fast.audioKernel)
        var offset = 0
        return mels.map { mel in
            let count = AudioEncoder.tokens(frames: mel.dim(0))
            defer { offset += count }
            return encoded[offset..<(offset + count)]
        }
    }

    /// Media encoders and prompt IDs for each input; audio of all inputs is encoded together.
    func prepare(_ inputs: [Input]) throws -> [Prepared] {
        var mels: [MLXArray] = [], audioTokens: [[Int]] = []
        for input in inputs {
            var tokens: [Int] = []
            for part in input.parts where part.type == "audio" {
                try admit()
                guard let path = part.path else { throw WorkerError.invalid("missing audio path") }
                let samples = try Media.samples(path)
                Profile.mark("audio_read")
                let frames = media.audio(samples).asType(.bfloat16)
                Profile.mark("audio_mel", frames)
                tokens.append(frames.dim(0) == 0 ? 0 : AudioEncoder.tokens(frames: frames.dim(0)))
                if frames.dim(0) > 0 { mels.append(frames) }
            }
            audioTokens.append(tokens)
        }
        let audio = try encodeAudio(mels)
        if !audio.isEmpty { Profile.mark("audio_encoder", audio.last!) }
        // Every image and video frame of the batch is decoded/resampled concurrently.
        var imageRequests: [(String, Int)] = []
        for input in inputs { for part in input.parts {
            if part.type == "image", let path = part.path { imageRequests.append((path, 280)) }
            if part.type == "video", let frames = part.frames { imageRequests += frames.map { ($0, 140) } }
        } }
        let decoded = try Media.imagePixels(imageRequests)
        var cursor = 0
        func nextImage() throws -> MLXArray {
            guard cursor < decoded.count else { throw WorkerError.invalid("missing image path") }
            let (pixels, height, width) = decoded[cursor]; cursor += 1
            return MLXArray(pixels, [1, 3, height, width])
        }
        // Batching: still images of one geometry go through the vision tower together (<= 4 a call).
        var stillFeatures: [Int: MLXArray] = [:]
        if fast.batch {
            var stills: [Int] = [], index = 0
            for input in inputs { for part in input.parts {
                if part.type == "image" && part.path != nil { stills.append(index); index += 1 }
                if part.type == "video" { index += part.frames?.count ?? 0 }
            } }
            let groups = Dictionary(grouping: stills) { "\(decoded[$0].1)x\(decoded[$0].2)" }
            for (_, members) in groups where members.count > 1 {
                for start in stride(from: 0, to: members.count, by: 4) {
                    let slice = Array(members[start..<min(start + 4, members.count)])
                    let pixels = concatenated(slice.map { MLXArray(decoded[$0].0, [1, 3, decoded[$0].1, decoded[$0].2]) }, axis: 0)
                    let encoded = try visionFeatures(pixels)
                    for (offset, member) in slice.enumerated() { stillFeatures[member] = encoded[offset] }
                }
            }
        }
        var audioIndex = 0
        return try inputs.enumerated().map { inputIndex, input in
            var prompt = Prompt(runtime: self, prefix: input.query ? "task: search result | query: " : "title: none | text: ")
            var features: [MLXArray] = []
            var partIndex = 0
            for part in input.parts {
                try admit()
                switch part.type {
                case "text": prompt.text(part.text ?? "")
                case "image":
                    guard part.path != nil else { throw WorkerError.invalid("missing image path") }
                    let slot = cursor
                    let encoded: MLXArray
                    if let batched = stillFeatures[slot] {
                        cursor += 1
                        encoded = batched.expandedDimensions(axis: 0)
                    } else {
                        let pixels = try nextImage()
                        Profile.mark("image_prep", pixels)
                        encoded = try visionFeatures(pixels)
                    }
                    features.append(encoded.reshaped(-1, 512))
                    Profile.mark("vision_encoder", encoded)
                    prompt.media(open: "<|image>", token: "<|image|>", count: encoded.dim(1), close: "<image|>")
                case "audio":
                    let count = audioTokens[inputIndex][partIndex]; partIndex += 1
                    if count == 0 { prompt.media(open: "<|audio>", token: "<|audio|>", count: 0, close: "<audio|>"); continue }
                    features.append(audio[audioIndex]); audioIndex += 1
                    prompt.media(open: "<|audio>", token: "<|audio|>", count: count, close: "<audio|>")
                case "video":
                    guard let paths = part.frames, !paths.isEmpty else { throw WorkerError.invalid("empty video") }
                    // Recorder frames are already sampled: process all, in original order.
                    let pixels = try paths.map { _ in try nextImage() }
                    Profile.mark("video_prep", pixels.first!)
                    guard pixels.allSatisfy({ $0.shape == pixels[0].shape }) else { throw WorkerError.invalid("video frames must share geometry") }
                    let encoded = try visionFeatures(concatenated(pixels, axis: 0))
                    features.append(encoded.reshaped(-1, 512))
                    Profile.mark("video_encoder", encoded)
                    for _ in paths { prompt.media(open: "<|image>", token: "<|video|>", count: encoded.dim(1), close: "<image|>") }
                default: throw WorkerError.invalid("unsupported input part")
                }
            }
            Profile.mark("host")
            let ids = try prompt.ids()
            Profile.mark("tokenize")
            guard !ids.isEmpty, ids.count <= 8192 else { throw WorkerError.invalid("context overflow") }
            return Prepared(ids: ids, soft: features)
        }
    }

    func trunkVector(_ prepared: Prepared) throws -> [Float] {
        let inputIDs = MLXArray(prepared.ids.map(Int32.init), [1, prepared.ids.count])
        let softTokens = prepared.soft.isEmpty ? nil : concatenated(prepared.soft, axis: 0)
        let vector = trunk.map { $0(ids: inputIDs, valid: nil, soft: softTokens, fast: fast) }
            ?? model.embed(inputIds: inputIDs, attentionMask: MLXArray.ones([1, prepared.ids.count], dtype: .int32), softTokens: softTokens)
        eval(vector)
        Profile.mark("trunk")
        let values = vector.asArray(Float.self)
        guard values.count == 768, values.allSatisfy(\.isFinite), values.contains(where: { $0 != 0 }) else { throw WorkerError.invalid("nonfinite or zero embedding") }
        return values
    }

    /// Vision tower features [N, tokens, 512]; shape-compiled when that part is on.
    func visionFeatures(_ pixels: MLXArray) throws -> MLXArray {
        if fast.compiledVision {
            if compiledVision == nil {
                let model = self.model
                compiledVision = compile { pixels in (try? model.imageFeatures(pixels)) ?? MLXArray(Float.nan) }
            }
            let encoded = compiledVision!(pixels)
            guard encoded.ndim == 3 else { throw WorkerError.invalid("vision encoder unavailable") }
            return encoded
        }
        guard let encoded = try model.imageFeatures(pixels) else { throw WorkerError.invalid("vision encoder unavailable") }
        return encoded
    }

    /// Fast parts that produced a non-finite or zero vector: redo it plain; if plain works, the fast
    /// parts stay off for this process and this Mac's verdict.
    func guarded<T>(_ work: () throws -> T) throws -> T {
        do { return try work() }
        catch WorkerError.invalid(let message) where message == "nonfinite or zero embedding" && !fast.isPlain {
            let suspects = fast
            fast = .plain
            let value = try work()
            Gate.recordRuntimeFailure(suspects.enabled, reason: "non-finite vector, plain MLX was finite")
            return value
        }
    }

    /// One vector per item (each item = the inputs of one chunk). Items that fit one context share
    /// trunk calls in length groups; anything else (windows, errors) runs alone through `embed`.
    func embedBatch(_ items: [[Input]], dimensions: Int) throws -> ([Result<[Float], Error>], Stats) {
        guard [128,256,512,768].contains(dimensions), (1...8).contains(items.count) else { throw WorkerError.invalid("invalid embedding geometry") }
        try admit()
        let start = ProcessInfo.processInfo.systemUptime
        var results = [Result<[Float], Error>?](repeating: nil, count: items.count)
        var tokens = 0, windows = 0, groups = 0
        func alone(_ index: Int) {
            // Through `guarded`: a fast part that yields a non-finite vector is redone plain here too.
            results[index] = Result { try guarded { try embed(items[index], dimensions: dimensions) } }.map { vector, stats in
                tokens += stats.tokens; windows += stats.windows; groups += 1; return vector
            }
        }
        // Single-input items without media tokens in text, finite positive weight, audio far below the window bound.
        var candidates: [Int] = []
        for (index, item) in items.enumerated() {
            let simple = fast.batch && item.count == 1 && item[0].weight.isFinite && item[0].weight > 0
                && item[0].parts.allSatisfy { part in
                    part.type != "text" || !["<|image|>", "<|audio|>", "<|video|>"].contains(where: (part.text ?? "").contains)
                }
                && estimatedTokens(item[0]) <= 6000
            if simple { candidates.append(index) } else { alone(index) }
        }
        var prepared: [Int: Prepared] = [:]
        if candidates.count > 1 {
            do {
                for (index, value) in zip(candidates, try prepare(candidates.map { items[$0][0] })) { prepared[index] = value }
            } catch {
                prepared = [:]   // One bad item: every candidate runs alone and reports its own error.
            }
        }
        for index in candidates where prepared[index] == nil { alone(index) }
        // Length groups: sorted, at most 8 rows, longest <= 1.25 x shortest + 16, <= 8192 padded tokens.
        let ordered = prepared.keys.sorted { prepared[$0]!.ids.count < prepared[$1]!.ids.count }
        var group: [Int] = []
        func flush() {
            defer { group = [] }
            guard !group.isEmpty else { return }
            do {
                let vectors = try trunkVectors(group.map { prepared[$0]! })
                for (index, vector) in zip(group, vectors) {
                    results[index] = Result { try finish(vector, weight: items[index][0].weight, dimensions: dimensions) }
                    tokens += prepared[index]!.ids.count; windows += 1
                }
                groups += 1
            } catch {
                for index in group { alone(index) }
            }
        }
        for index in ordered {
            let length = prepared[index]!.ids.count
            if let first = group.first {
                let shortest = prepared[first]!.ids.count
                if group.count == 8 || Double(length) > Double(shortest) * 1.25 + 16 || (group.count + 1) * length > 8192 { flush() }
            }
            group.append(index)
        }
        flush()
        try admit(force: true)
        return (results.map { $0 ?? .failure(WorkerError.invalid("missing result")) },
                Stats(seconds: ProcessInfo.processInfo.systemUptime - start, tokens: tokens, windows: windows, groups: groups))
    }

    /// Upper estimate of an input's prompt length without encoding it (text ~1 token per 2 bytes,
    /// audio 25 tokens/s, images 282, video frames 142); longer inputs run alone.
    func estimatedTokens(_ input: Input) -> Int {
        input.parts.reduce(2) { total, part in
            switch part.type {
            case "text": return total + (part.text?.utf8.count ?? 0) / 2 + 1
            case "audio":
                let bytes = part.path.flatMap { (try? FileManager.default.attributesOfItem(atPath: $0)[.size] as? NSNumber)?.intValue } ?? 0
                return total + (bytes / 4 + 639) / 640 + 2
            case "image": return total + 282
            case "video": return total + (part.frames?.count ?? 0) * 142
            default: return total + 8192
            }
        }
    }

    /// Unit vector of one pooled trunk output, as `embed` builds it for a one-window input.
    func finish(_ value: [Float], weight: Float, dimensions: Int) throws -> [Float] {
        var vector = Array(value.prefix(dimensions)).map { $0 * weight }
        let norm = sqrt(vector.reduce(Float(0)) { $0 + $1 * $1 })
        guard vector.allSatisfy(\.isFinite), norm.isFinite, norm > 0 else { throw WorkerError.invalid("nonfinite or zero embedding") }
        vector = vector.map { $0 / norm }
        return vector
    }

    /// Several prepared rows in one padded trunk call (pad id 0, masked in attention and pooling).
    func trunkVectors(_ rows: [Prepared]) throws -> [[Float]] {
        if rows.count == 1 { return [try trunkVector(rows[0])] }
        guard let trunk else { return try rows.map { try trunkVector($0) } }
        let longest = rows.map(\.ids.count).max()!
        var ids = [Int32](repeating: 0, count: rows.count * longest), valid = [Int32](repeating: 0, count: rows.count * longest)
        for (row, prepared) in rows.enumerated() {
            for (column, id) in prepared.ids.enumerated() { ids[row * longest + column] = Int32(id); valid[row * longest + column] = 1 }
        }
        let soft = rows.flatMap(\.soft)
        let output = trunk(ids: MLXArray(ids, [rows.count, longest]), valid: MLXArray(valid, [rows.count, longest]),
                           soft: soft.isEmpty ? nil : concatenated(soft, axis: 0), fast: fast)
        eval(output)
        let flat = output.asArray(Float.self)
        let vectors = (0..<rows.count).map { Array(flat[($0 * 768)..<(($0 + 1) * 768)]) }
        for values in vectors {
            guard values.allSatisfy(\.isFinite), values.contains(where: { $0 != 0 }) else { throw WorkerError.invalid("nonfinite or zero embedding") }
        }
        // Runtime parity, once per process: the first row again at B=1.
        if !batchChecked {
            batchChecked = true
            let single = try trunkVector(rows[0])
            let similarity = Gate.cosine(single, vectors[0])
            if similarity < 0.999 {
                fast.batch = false
                Gate.recordRuntimeFailure(["batch"], reason: String(format: "B=1 cosine %.5f", similarity))
                return try rows.map { try trunkVector($0) }
            }
        }
        return vectors
    }

    func forward(_ input: Input) throws -> ([Float], Int) {
        let prepared = try prepare([input])[0]
        return (try trunkVector(prepared), prepared.ids.count)
    }

    func embed(_ inputs: [Input], dimensions: Int) throws -> ([Float], Stats) {
        guard [128,256,512,768].contains(dimensions), !inputs.isEmpty, inputs.count <= 256 else { throw WorkerError.invalid("invalid embedding geometry") }
        try admit()
        Profile.start()
        let start = ProcessInfo.processInfo.systemUptime
        var vector = [Float](repeating: 0, count: 768), tokens = 0, count = 0
        for item in inputs {
            guard item.weight.isFinite else { throw WorkerError.invalid("nonfinite input weight") }
            for part in item.parts where part.type == "text" {
                guard let text = part.text, !["<|image|>", "<|audio|>", "<|video|>"].contains(where: text.contains) else {
                    throw WorkerError.invalid("literal model media token in text")
                }
            }
            var values: [([Float], Int)] = []
            // Preserve the ordinary fused path; use windows only for actual overflow.
            // Rust flushes PCM after a callback crosses 120 seconds, so a valid
            // part can slightly exceed that soft threshold. Long legacy inputs
            // must window BEFORE running every audio encoder twice.
            var audioLowerBound = 0
            if item.parts.count > 3 {
                for part in item.parts where part.type == "audio" {
                    guard let path = part.path else { throw WorkerError.invalid("missing audio path") }
                    let bytes = (try FileManager.default.attributesOfItem(atPath: path)[.size] as? NSNumber)?.intValue ?? 0
                    let frames = max(0, (bytes / 4 + 160 - 321) / 160 + 1)
                    audioLowerBound += (frames + 3) / 4 + 2
                }
            }
            do {
                if audioLowerBound > 8192 { throw WorkerError.invalid("context overflow") }
                values = [try forward(item)]
            }
            catch WorkerError.invalid(let message) where message == "context overflow" {
                for group in try windows(item) { values.append(try forward(group)) }
            }
            let total = values.reduce(0) { $0 + $1.1 }
            guard total > 0 else { throw WorkerError.invalid("empty input") }
            for (value, length) in values {
                let weight = item.weight * Float(length) / Float(total)
                for i in 0..<768 { vector[i] += value[i] * weight }
                tokens += length; count += 1
            }
        }
        vector = Array(vector.prefix(dimensions))
        let norm = sqrt(vector.reduce(Float(0)) { $0 + $1 * $1 })
        guard vector.allSatisfy(\.isFinite), norm.isFinite, norm > 0 else { throw WorkerError.invalid("nonfinite or zero embedding") }
        vector = vector.map { $0 / norm }
        try admit(force: true)
        Profile.mark("finish"); Profile.flush()
        return (vector, Stats(seconds: ProcessInfo.processInfo.systemUptime - start, tokens: tokens, windows: count))
    }
}

@main struct Worker {
    static func line() throws -> Data? {
        var data = Data()
        while true {
            let byte = fgetc(stdin)
            if byte == EOF { return data.isEmpty ? nil : data }
            if byte == 10 { return data }
            guard data.count < 1024 * 1024 else { throw WorkerError.invalid("request exceeds protocol bound") }
            data.append(UInt8(byte))
        }
    }
    static func main() async {
        do {
            // No weights, no GPU work: the cache policy as MLX reports it back (the default test checks it).
            if CommandLine.arguments.count == 2 && CommandLine.arguments[1] == "--cache-policy" {
                let free = try MemoryState.current().free
                Memory.cacheLimit = CachePolicy.current.limit(free: free)
                print("{\"cache_limit_bytes\":\(Memory.cacheLimit),\"clears\":\(CachePolicy.current != .legacy)}")
                return
            }
            if CommandLine.arguments.count == 3 && CommandLine.arguments[1] == "--tokenize" {
                let tokenizer = try await AutoTokenizer.from(modelFolder: URL(fileURLWithPath: CommandLine.arguments[2]))
                while let data = try line() {
                    let text = try JSONDecoder().decode(String.self, from: data)
                    let ids = tokenizer.encode(text: text, addSpecialTokens: true)
                    var output = try JSONEncoder().encode(ids); output.append(10)
                    try FileHandle.standardOutput.write(contentsOf: output)
                }
                return
            }
            // Lab-only tensor capture, to verify preprocessing against the frozen
            // official processor independently of model arithmetic.
            if CommandLine.arguments.count == 5 && CommandLine.arguments[1] == "--dump-image" {
                let tensor = try Media.image(CommandLine.arguments[2], budget: Int(CommandLine.arguments[3]) ?? 280)
                eval(tensor)
                let values = tensor.asArray(Float.self)
                let bytes = values.withUnsafeBytes { Data($0) }
                try bytes.write(to: URL(fileURLWithPath: CommandLine.arguments[4]))
                print(tensor.shape.map(String.init).joined(separator: ",")); return
            }
            if CommandLine.arguments.count == 4 && CommandLine.arguments[1] == "--self-test" {
                let directory = URL(fileURLWithPath: CommandLine.arguments[2])
                let tokenizer = try await AutoTokenizer.from(modelFolder: directory)
                let runtime = try Runtime(directory: directory, tokenizer: tokenizer)
                try Gate.selfTest(runtime: runtime, parts: CommandLine.arguments[3].split(separator: ",").map(String.init))
                return
            }
            guard CommandLine.arguments.count == 2 else { throw WorkerError.invalid("weights directory required") }
            let directory = URL(fileURLWithPath: CommandLine.arguments[1])
            // Self-test first, before this process loads its own copy of the weights.
            let fast = Gate.resolve(weights: directory)
            let tokenizer = try await AutoTokenizer.from(modelFolder: directory)
            let runtime = try Runtime(directory: directory, tokenizer: tokenizer)
            runtime.fast = fast
            let lab = ProcessInfo.processInfo.environment["HEIMDALL_EMBED_LAB"] == "1"
            // Mixed precision was measured and rejected (3 Nov): serving stays BF16. The spec is lab-only.
            if lab, let spec = ProcessInfo.processInfo.environment["HEIMDALL_EMBED_QUANT"], !spec.isEmpty {
                try runtime.applyQuant(Runtime.parseQuant(spec))
            }
            while let data = try line() {
                autoreleasepool {
                    var reply = Reply(id: nil)
                    do {
                        let request = try JSONDecoder().decode(Request.self, from: data)
                        reply = Reply(id: request.id)
                        switch request.op {
                        case "usage":
                            reply.usage = Usage.current(); reply.fast = runtime.fast.enabled
                            reply.mlx = ["active": Memory.activeMemory, "cache": Memory.cacheMemory, "peak": Memory.peakMemory]
                        case "trim":
                            // Idle: weights stay resident, scratch buffers and window plans go.
                            WindowPlan.shared.clear()
                            runtime.compiledVision = nil
                            Memory.clearCache()
                            reply.usage = Usage.current()
                        case "batch":
                            guard let items = request.items else { throw WorkerError.invalid("missing items") }
                            let (results, stats) = try runtime.guarded { try runtime.embedBatch(items, dimensions: request.dimensions ?? 768) }
                            reply.vectors = results.map { try? $0.get() }
                            reply.errors = results.map { result in
                                if case .failure(let error) = result { return error.localizedDescription }
                                return nil
                            }
                            reply.stats = stats
                            reply.model_id = modelID; reply.model_version = modelVersion
                            reply.usage = Usage.current()
                        case "lab" where lab:
                            if let spec = request.quant { reply.quant_bytes = try runtime.applyQuant(Runtime.parseQuant(spec)); runtime.compiledVision = nil }
                            if let parts = request.fast {
                                var fast = Fast()
                                for name in parts.split(separator: ",").map(String.init) where Fast.names.contains(name) { fast[name] = true }
                                runtime.fast = fast
                            }
                            Memory.clearCache()
                            Memory.peakMemory = 0
                            reply.fast = runtime.fast.enabled; reply.usage = Usage.current()
                        case nil:
                            let (vector, stats) = try runtime.guarded { try runtime.embed(request.inputs ?? [], dimensions: request.dimensions ?? 768) }
                            reply.vector = vector; reply.stats = stats
                            reply.model_id = modelID; reply.model_version = modelVersion
                            reply.usage = Usage.current()
                        default: throw WorkerError.invalid("unsupported op")
                        }
                    } catch { reply.error = error.localizedDescription }
                    do {
                        var output = try JSONEncoder().encode(reply); output.append(10)
                        try FileHandle.standardOutput.write(contentsOf: output)
                    } catch { fputs("embedding reply failed\n", stderr) }
                }
                // After the reply, and after the pool has released the request's arrays (cleared inside the
                // pool, their buffers landed back in the cache afterwards).
                CachePolicy.current.release()
            }
        } catch { fputs("EmbeddingGemma 2: \(error.localizedDescription)\n", stderr); exit(1) }
    }
}
