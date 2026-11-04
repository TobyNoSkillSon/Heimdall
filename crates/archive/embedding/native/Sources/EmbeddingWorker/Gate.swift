// Per-part self-test with a remembered verdict (Vella KernelGate pattern).
//
// On the first start for a (GPU, macOS build, helper binary, MLX) key the helper runs itself as a
// child with `--self-test`: every fast part is compiled and compared with stock MLX, one JSON line
// per part. A Metal compile error aborts only the child; the part it was testing fails and the
// child is restarted for the rest. The verdict is stored in HEIMDALL_EMBED_STATE and reused
// (~ms). A part that fails, or produces a non-finite vector at run time, drops to plain MLX.
// HEIMDALL_EMBED_SAFE=1 (Rust sets it after two crashes in a row) runs everything plain.
import Foundation
import Darwin
import MLX
import MLXNN

enum Gate {
    static let version = 1
    /// Parts tried when their self-test passes. Lab override: HEIMDALL_EMBED_FAST=plain|part,part.
    static let defaults: Fast = {
        var fast = Fast()
        for name in ["smallM", "tile", "audioKernel", "compiled", "compiledVision", "batch"] { fast[name] = true }
        return fast
    }()

    struct Verdict: Codable {
        var key: String
        var parts: [String: Bool]
        var metrics: [String: Double]
        var notes: [String: String]
        var complete = true
    }

    static var environment: [String: String] { ProcessInfo.processInfo.environment }
    static var stateFile: URL? {
        guard let path = environment["HEIMDALL_EMBED_STATE"], !path.isEmpty else { return nil }
        return URL(fileURLWithPath: path).appendingPathComponent("embedding-gate.json")
    }

    static func osBuild() -> String {
        var size = 0
        sysctlbyname("kern.osversion", nil, &size, nil, 0)
        var buffer = [CChar](repeating: 0, count: max(size, 1))
        sysctlbyname("kern.osversion", &buffer, &size, nil, 0)
        return String(cString: buffer)
    }

    static func key() -> String {
        let binary = URL(fileURLWithPath: CommandLine.arguments[0]).resolvingSymlinksInPath()
        let attributes = (try? FileManager.default.attributesOfItem(atPath: binary.path)) ?? [:]
        let size = (attributes[.size] as? NSNumber)?.intValue ?? 0
        let modified = (attributes[.modificationDate] as? Date)?.timeIntervalSince1970 ?? 0
        return ["v\(version)", GPU.deviceInfo().architecture, osBuild(), "\(size)", String(format: "%.0f", modified),
                SmallMGEMM.revision, environment["HEIMDALL_EMBED_FAST"] ?? ""].joined(separator: "|")
    }

    /// Parts wanted on this Mac before testing: defaults (or the lab override) minus what the hardware lacks.
    static func candidates() -> Fast {
        var fast = defaults
        if let override = environment["HEIMDALL_EMBED_FAST"] {
            fast = Fast()
            for name in override.split(separator: ",").map(String.init) where Fast.names.contains(name) { fast[name] = true }
        }
        if !SmallMGEMM.tensorOpsAvailable { fast.tile = false }
        return fast
    }

    /// The parts this helper process runs with.
    static func resolve(weights: URL) -> Fast {
        if environment["HEIMDALL_EMBED_SAFE"] == "1" {
            fputs("EmbeddingGemma 2: safe mode, every part plain MLX\n", stderr)
            return .plain
        }
        let wanted = candidates()
        if wanted.isPlain { return .plain }
        let key = key()
        if let file = stateFile, let data = try? Data(contentsOf: file),
           let verdict = try? JSONDecoder().decode(Verdict.self, from: data), verdict.key == key,
           wanted.enabled.allSatisfy({ verdict.parts[$0] != nil }) {
            return apply(verdict, to: wanted)
        }
        let verdict = runChild(weights: weights, parts: wanted.enabled, key: key)
        // A child that died before testing anything (low memory at start, unreadable weights) says
        // nothing about the parts: run plain now, keep no verdict, test again on the next start.
        guard verdict.complete else { return apply(verdict, to: wanted) }
        save(verdict)
        return apply(verdict, to: wanted)
    }

    static func apply(_ verdict: Verdict, to wanted: Fast) -> Fast {
        var fast = Fast()
        for name in wanted.enabled { fast[name] = verdict.parts[name] == true }
        return fast
    }

    static func save(_ verdict: Verdict) {
        guard let file = stateFile, let data = try? JSONEncoder().encode(verdict) else { return }
        try? FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true,
                                                 attributes: [.posixPermissions: 0o700])
        try? data.write(to: file, options: .atomic)
    }

    /// A part that misbehaved while serving stays off for this key.
    static func recordRuntimeFailure(_ parts: [String], reason: String) {
        guard let file = stateFile, let data = try? Data(contentsOf: file),
              var verdict = try? JSONDecoder().decode(Verdict.self, from: data), verdict.key == key() else { return }
        for part in parts { verdict.parts[part] = false; verdict.notes[part] = "runtime: \(reason)" }
        save(verdict)
        fputs("EmbeddingGemma 2: \(parts.joined(separator: ",")) off after runtime check: \(reason)\n", stderr)
    }

    /// Runs `--self-test` children until every part has a result. A child that dies fails the part it
    /// was on; at most `parts.count + 1` children.
    static func runChild(weights: URL, parts: [String], key: String) -> Verdict {
        var verdict = Verdict(key: key, parts: [:], metrics: [:], notes: [:])
        var remaining = parts
        let started = Date()
        for _ in 0...parts.count where !remaining.isEmpty {
            let process = Process()
            process.executableURL = URL(fileURLWithPath: CommandLine.arguments[0])
            process.arguments = ["--self-test", weights.path, remaining.joined(separator: ",")]
            var environment = Self.environment
            environment.removeValue(forKey: "HEIMDALL_EMBED_STATE")
            process.environment = environment
            let output = Pipe()
            process.standardOutput = output
            process.standardError = FileHandle.standardError
            var current: String? = nil
            do { try process.run() } catch {
                verdict.complete = false
                for part in remaining { verdict.parts[part] = false; verdict.notes[part] = "self-test did not start" }
                break
            }
            // Bounded: a hung child is killed after 180 s in total.
            let deadline = DispatchTime.now() + .seconds(max(1, 180 - Int(Date().timeIntervalSince(started))))
            DispatchQueue.global().asyncAfter(deadline: deadline) { if process.isRunning { process.terminate() } }
            let data = output.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            for line in data.split(separator: 10) {
                guard let object = try? JSONSerialization.jsonObject(with: Data(line)) as? [String: Any],
                      let part = object["part"] as? String else { continue }
                if object["start"] as? Bool == true { current = part; continue }
                verdict.parts[part] = object["ok"] as? Bool ?? false
                if let metric = object["metric"] as? Double { verdict.metrics[part] = metric }
                if let note = object["note"] as? String { verdict.notes[part] = note }
                remaining.removeAll { $0 == part }
                current = nil
            }
            if process.terminationStatus != 0 || process.terminationReason == .uncaughtSignal {
                guard let failed = current else {
                    // Died outside any part's test: no part to blame.
                    verdict.complete = false
                    for part in remaining { verdict.parts[part] = false; verdict.notes[part] = "self-test incomplete (status \(process.terminationStatus))" }
                    remaining = []
                    break
                }
                verdict.parts[failed] = false
                verdict.notes[failed] = "self-test child died (status \(process.terminationStatus))"
                remaining.removeAll { $0 == failed }
            } else if process.terminationStatus == 0 {
                for part in remaining { verdict.parts[part] = false; verdict.notes[part] = "no result" }
                remaining = []
            }
        }
        for part in remaining { verdict.parts[part] = false; verdict.notes[part] = "not tested" }
        let summary = parts.map { "\($0)=\(verdict.parts[$0] == true ? "on" : "off")" }.joined(separator: " ")
        fputs("EmbeddingGemma 2 self-test (\(String(format: "%.1f", Date().timeIntervalSince(started))) s): \(summary)\n", stderr)
        return verdict
    }

    // MARK: - Child side

    static func emit(_ object: [String: Any]) {
        guard var data = try? JSONSerialization.data(withJSONObject: object) else { return }
        data.append(10)
        FileHandle.standardOutput.write(data)
    }

    static func random(_ shape: [Int], seed: UInt64, scale: Float = 1) -> MLXArray {
        (MLXRandom.normal(shape, key: MLXRandom.key(seed)) * scale).asType(.bfloat16)
    }

    static func cosine(_ a: [Float], _ b: [Float]) -> Double {
        var dot = 0.0, na = 0.0, nb = 0.0
        for i in 0..<min(a.count, b.count) { dot += Double(a[i] * b[i]); na += Double(a[i] * a[i]); nb += Double(b[i] * b[i]) }
        return dot / max((na * nb).squareRoot(), 1e-12)
    }

    /// `--self-test <weights> <parts>`: one {"part","ok","metric"} line per part, in order.
    static func selfTest(runtime: Runtime, parts: [String]) throws {
        let scratch = FileManager.default.temporaryDirectory.appendingPathComponent("heimdall-embed-selftest-\(getpid())")
        try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: scratch) }
        let samples = try SelfTestInputs(directory: scratch)
        for part in parts {
            emit(["part": part, "start": true])
            var ok = false, metric = 0.0, note = ""
            switch part {
            case "smallM":
                let results = SmallMGEMM.selfTest(including: { $0 == "tile.bf16.dense.none" || $0 == "gemv.bf16.dense.none" })
                let failures = SmallMGEMM.selfTestFailures(results)
                metric = Double(results.values.max() ?? 0); ok = failures.isEmpty && !results.isEmpty
                note = failures.joined(separator: ",")
            case "tile":
                var worst: Float = 0
                for rows in [9, 40, 300, 768] {
                    let x = random([rows, 2048], seed: UInt64(rows)), w = random([512, 2048], seed: 7, scale: 0.05)
                    worst = max(worst, SmallMGEMM.relativeRMS(TileLinear.apply(x, w), matmul(x, w.T)))
                }
                metric = Double(worst); ok = worst <= 2e-2
            case "audioKernel":
                guard let encoder = runtime.audioEncoder else { note = "no native audio encoder"; break }
                let mels = [random([300, 128], seed: 11), random([137, 128], seed: 12)]
                metric = Double(SmallMGEMM.relativeRMS(encoder(mels, kernel: true), encoder(mels, kernel: false))); ok = metric <= 2e-2
            case "fused256", "flash512", "blocked":
                var fast = Fast(); fast[part] = true
                let dim = part == "fused256" ? 256 : 512, rows = part == "fused256" ? 300 : 1100
                let kvHeads = part == "fused256" ? 2 : 1
                var worst: Float = 0
                for masked in [false, true] {
                    let batch = masked ? 2 : 1
                    let q = random([batch, 4, rows, dim], seed: 21) * 0.06, k = random([batch, kvHeads, rows, dim], seed: 22) * 0.06
                    let v = random([batch, kvHeads, rows, dim], seed: 23)
                    let mask: MLXArray? = masked ? (MLXArray(0..<rows).reshaped(1, rows) .< MLXArray([Int32(rows), Int32(rows - 77)]).reshaped(2, 1)).reshaped(2, 1, 1, rows) : nil
                    let candidate = Attention.full(queries: q, keys: k, values: v, mask: mask, fast: fast)
                    let reference = Attention.full(queries: q, keys: k, values: v, mask: mask, fast: .plain)
                    worst = max(worst, SmallMGEMM.relativeRMS(candidate, reference))
                }
                metric = Double(worst); ok = worst <= (part == "blocked" ? 1e-3 : 2e-2)
            case "compiled", "compiledVision", "batch":
                var fast = Fast(); fast[part] = true
                runtime.fast = .plain
                let plain = try samples.items.map { try runtime.embed([$0], dimensions: 768).0 }
                runtime.fast = fast
                let candidate: [[Float]] = part == "batch"
                    ? try runtime.embedBatch(samples.items.map { [$0] }, dimensions: 768).0.map { try $0.get() }
                    : try samples.items.map { try runtime.embed([$0], dimensions: 768).0 }
                runtime.fast = .plain
                metric = zip(plain, candidate).map { cosine($0, $1) }.min() ?? 0
                ok = metric >= (part == "batch" ? 0.999 : 0.9999)
            default: note = "unknown part"
            }
            emit(["part": part, "ok": ok && metric.isFinite, "metric": metric.isFinite ? metric : -1, "note": note])
        }
    }
}

/// Synthetic inputs for the whole-model checks: two texts, two same-size images, one audio clip.
struct SelfTestInputs {
    let items: [Input]
    init(directory: URL) throws {
        func png(_ name: String, seed: UInt8) throws -> String {
            let width = 320, height = 200
            var rgba = [UInt8](repeating: 255, count: width * height * 4)
            for y in 0..<height { for x in 0..<width {
                let i = (y * width + x) * 4
                rgba[i] = UInt8((x * 3 + Int(seed) * 40) & 255); rgba[i + 1] = UInt8((y * 5) & 255); rgba[i + 2] = UInt8(((x ^ y) + Int(seed)) & 255)
            } }
            let url = directory.appendingPathComponent(name)
            try Media.writePNG(rgba: rgba, width: width, height: height, to: url)
            return url.path
        }
        var pcm = [Float](repeating: 0, count: 16000 * 3)
        for i in pcm.indices { pcm[i] = 0.2 * sin(Float(i) * 0.05) + 0.05 * sin(Float(i) * 0.31) }
        let audio = directory.appendingPathComponent("tone.f32")
        try pcm.withUnsafeBytes { Data($0) }.write(to: audio)
        items = [
            Input(parts: [Part(type: "text", text: "Heimdall self-test: the quick brown fox jumps over the lazy dog.", path: nil, frames: nil)], query: false, weight: 1),
            Input(parts: [Part(type: "text", text: "kernel parity check", path: nil, frames: nil)], query: true, weight: 1),
            Input(parts: [Part(type: "text", text: "\nTerminal", path: nil, frames: nil), Part(type: "image", text: nil, path: try png("a.png", seed: 1), frames: nil)], query: false, weight: 1),
            Input(parts: [Part(type: "text", text: "\nSafari", path: nil, frames: nil), Part(type: "image", text: nil, path: try png("b.png", seed: 2), frames: nil)], query: false, weight: 1),
            Input(parts: [Part(type: "audio", text: nil, path: audio.path, frames: nil)], query: false, weight: 1),
        ]
    }
}
