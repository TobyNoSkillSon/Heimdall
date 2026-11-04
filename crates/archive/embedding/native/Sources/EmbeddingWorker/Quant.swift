// Mixed precision: chosen units quantized to MLX affine int8/int4 (group 64), the rest BF16.
// Units: "embed" (token table), "text.<i>" or "text.<i>.<projection>" (trunk layer), "text.ple",
// "audio.<i>" (conformer layer), "vision.<i>" (vision encoder layer). Weights are only ever
// quantized down from the loaded BF16 checkpoint; nothing quantized is converted back up.
import Foundation
import MLX
import MLXNN

/// Shared by the audio encoder's projections: key -> quantized weight.
final class QuantStore { var weights: [String: QuantizedWeight] = [:] }

extension Runtime {
    /// Parses "unit=bits,unit=bits" (bits 4 or 8; 16 = BF16).
    static func parseQuant(_ text: String) throws -> [String: Int] {
        var spec: [String: Int] = [:]
        for item in text.split(separator: ",") where !item.isEmpty {
            let pair = item.split(separator: "=")
            guard pair.count == 2, let bits = Int(pair[1]), [4, 8, 16].contains(bits) else { throw WorkerError.invalid("invalid quant spec \(item)") }
            spec[String(pair[0])] = bits
        }
        return spec
    }

    /// Resets every unit to BF16, then quantizes the units in `spec`. Returns the quantized weight bytes.
    @discardableResult
    func applyQuant(_ spec: [String: Int]) throws -> Int {
        // Text trunk and token table.
        if let trunk {
            trunk.quantized = [:]
            for (unit, bits) in spec where bits != 16 {
                let parts = unit.split(separator: ".").map(String.init)
                switch parts.first {
                case "embed": trunk.quantized["embed"] = QuantizedWeight(trunk.embedTokens, bits: bits)
                case "text":
                    if parts.count == 2, parts[1] == "ple" { trunk.quantized["ple"] = QuantizedWeight(trunk.pleProjection, bits: bits); continue }
                    guard parts.count >= 2, let index = Int(parts[1]), trunk.layers.indices.contains(index) else { throw WorkerError.invalid("unknown unit \(unit)") }
                    let layer = trunk.layers[index]
                    let all: [(String, MLXArray)] = [("q", layer.q), ("k", layer.k), ("v", layer.v), ("o", layer.o), ("gate", layer.gate),
                                                     ("up", layer.up), ("down", layer.down), ("pleGate", layer.pleGate), ("pleProjection", layer.pleProjection)]
                    for (name, weight) in all where parts.count == 2 || parts[2] == name {
                        trunk.quantized["\(index).\(name)"] = QuantizedWeight(weight, bits: bits)
                    }
                default: break
                }
            }
        }
        // Audio conformer layers.
        if let audioEncoder {
            audioEncoder.store.weights = [:]
            for (unit, bits) in spec where bits != 16 && unit.hasPrefix("audio.") {
                guard let index = Int(unit.dropFirst("audio.".count)), audioEncoder.layers.indices.contains(index) else { throw WorkerError.invalid("unknown unit \(unit)") }
                for clipped in audioEncoder.layers[index].projections {
                    audioEncoder.store.weights[clipped.key] = QuantizedWeight(clipped.weight, bits: bits)
                }
            }
        }
        // Vision encoder layers: swap Linear <-> QuantizedLinear modules.
        let vision = spec.filter { $0.key.hasPrefix("vision.") && $0.value != 16 }
        if !vision.isEmpty || !visionOriginals.isEmpty {
            if visionOriginals.isEmpty {
                for (path, module) in model.leafModules().flattened() where path.hasPrefix("vision_tower.encoder.layers.") {
                    if let linear = module as? Linear, !(module is QuantizedLinear) { visionOriginals[path] = linear }
                }
            }
            var updates: [(String, Module)] = []
            for (path, linear) in visionOriginals {
                let index = path.split(separator: ".")[3]
                if let bits = vision["vision.\(index)"] {
                    updates.append((path, QuantizedLinear(linear, groupSize: 64, bits: bits)))
                } else {
                    updates.append((path, linear))
                }
            }
            model.update(modules: ModuleChildren.unflattened(updates))
            for (path, module) in model.leafModules().flattened() where path.hasPrefix("vision_tower.encoder.layers.") {
                if let q = module as? QuantizedLinear { eval(q.weight, q.scales) }
            }
        }
        quantSpec = spec
        return (trunk?.quantized.values.reduce(0) { $0 + $1.bytes } ?? 0)
            + (audioEncoder?.store.weights.values.reduce(0) { $0 + $1.bytes } ?? 0)
    }
}
