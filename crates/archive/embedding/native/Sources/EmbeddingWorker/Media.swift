// Native preprocessing for the pinned official checkpoint. No input is truncated.
import Foundation
import ImageIO
import CoreGraphics
import MLX
import MLXFFT

enum WorkerError: Error, LocalizedError {
    case invalid(String)
    var errorDescription: String? { if case .invalid(let message) = self { return message }; return nil }
}

struct Media {
    let window: MLXArray
    let filters: MLXArray
    init() {
        window = MLXArray((0..<320).map { Float(0.5 - 0.5 * cos(2 * Double.pi * Double($0) / 320)) })
        func mel(_ f: Double) -> Double { 2595 * log10(1 + f / 700) }
        let edges = (0..<130).map { 700 * (pow(10, Double($0) * mel(8000) / 129 / 2595) - 1) }
        var values = [Float](repeating: 0, count: 257 * 128)
        for bin in 0..<257 {
            let hz = Double(bin) * 8000 / 256
            for m in 0..<128 {
                values[bin * 128 + m] = Float(max(0, min((hz - edges[m]) / (edges[m+1] - edges[m]),
                    (edges[m+2] - hz) / (edges[m+2] - edges[m+1]))))
            }
        }
        filters = MLXArray(values, [257, 128])
        eval(window, filters)
    }
    static func samples(_ path: String) throws -> [Float] {
        let attributes = try FileManager.default.attributesOfItem(atPath: path)
        guard let bytes = attributes[.size] as? NSNumber, bytes.intValue > 0,
              bytes.intValue % 4 == 0, bytes.intValue <= 16000 * 240 * 4 else {
            throw WorkerError.invalid("invalid or unbounded mono 16 kHz audio")
        }
        let data = try Data(contentsOf: URL(fileURLWithPath: path))
        // Little-endian f32 on arm64: one copy, no per-sample conversion.
        var values = [Float](repeating: 0, count: data.count / 4)
        values.withUnsafeMutableBytes { target in data.withUnsafeBytes { source in target.copyMemory(from: source) } }
        guard values.allSatisfy(\.isFinite) else { throw WorkerError.invalid("nonfinite audio") }
        return values
    }
    func audio(_ values: [Float]) -> MLXArray {
        let padded = concatenated([MLXArray.zeros([160]), MLXArray(values)])
        let count = max(0, (values.count + 160 - 321) / 160 + 1)
        guard count > 0 else { return MLXArray.zeros([0, 128]) }
        let frames = asStrided(padded, [count, 320], strides: [160, 1])
        let magnitude = abs(MLXFFT.rfft(frames * window, n: 512, axis: -1))
        return log(matmul(magnitude, filters) + 0.001)
    }
    static func targetSize(width: Int, height: Int, budget: Int) -> (Int, Int) {
        let scale = sqrt(Double(budget * 9 * 16 * 16) / Double(width * height))
        var h = Int(floor(scale * Double(height) / 48))
        var w = Int(floor(scale * Double(width) / 48))
        if h == 0 { h = 1; w = max(1, min(width / max(height, 1), budget)) }
        if w == 0 { w = 1; h = max(1, min(height / max(width, 1), budget)) }
        return (w * 48, h * 48)
    }
    // Pillow's separable uint8 bicubic: horizontal first, 22-bit coefficients,
    // uint8 rounding between the passes. Scaling into Float32 happens afterwards.
    private static func weights(input: Int, output: Int) -> [(Int, [Int64])] {
        let scale = Double(input) / Double(output), fs = max(scale, 1)
        func cubic(_ x: Double) -> Double {
            let x = abs(x)
            if x < 1 { return ((1.5 * x - 2.5) * x) * x + 1 }
            if x < 2 { return (((x - 5) * x + 8) * x - 4) * -0.5 }
            return 0
        }
        return (0..<output).map { i in
            let center = (Double(i) + 0.5) * scale
            let first = max(0, Int(center - 2 * fs + 0.5))
            let last = min(input, Int(center + 2 * fs + 0.5))
            let taps = (first..<last).map { cubic((Double($0) - center + 0.5) / fs) }
            let total = taps.reduce(0, +)
            return (first, taps.map { tap in
                let value = tap / total * Double(1 << 22)
                return Int64(value < 0 ? value - 0.5 : value + 0.5)
            })
        }
    }
    static func image(_ path: String, budget: Int) throws -> MLXArray {
        let (pixels, height, width) = try imagePixels(path, budget: budget)
        return MLXArray(pixels, [1, 3, height, width])
    }

    /// Decode + resample several images on parallel threads (CPU only; MLX arrays are
    /// created by the caller). Results keep the input order.
    static func imagePixels(_ requests: [(String, Int)]) throws -> [([Float], Int, Int)] {
        // One image: decode inline so its row-parallel resample keeps the whole pool.
        if requests.count <= 1 { return try requests.map { try imagePixels($0.0, budget: $0.1) } }
        var results = [Result<([Float], Int, Int), Error>?](repeating: nil, count: requests.count)
        results.withUnsafeMutableBufferPointer { slots in
            let base = slots.baseAddress!
            DispatchQueue.concurrentPerform(iterations: requests.count) { i in
                base[i] = Result { try imagePixels(requests[i].0, budget: requests[i].1) }
            }
        }
        return try results.map { try $0!.get() }
    }

    static func imagePixels(_ path: String, budget: Int) throws -> ([Float], Int, Int) {
        guard let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL, nil),
              let image = CGImageSourceCreateImageAtIndex(source, 0, nil),
              image.width > 0, image.height > 0, image.width <= 16384, image.height <= 16384,
              image.width * image.height <= 32_000_000 else { throw WorkerError.invalid("invalid or unbounded image") }
        let (w, h) = (image.width, image.height)
        var rgba = [UInt8](repeating: 0, count: w * h * 4)
        // PIL.convert("RGB") reads PNG's encoded RGB values without applying its
        // ICC profile or compositing its alpha. Drawing P3 PNG into sRGB changes
        // model inputs; copy the straight RGBA provider bytes instead.
        var drawn = false
        if image.bitsPerComponent == 8 && image.bitsPerPixel == 32,
           image.alphaInfo == .last || image.alphaInfo == .noneSkipLast,
           image.bitmapInfo.intersection(.byteOrderMask).isEmpty,
           let provider = image.dataProvider?.data {
            let bytes = provider as Data
            if bytes.count >= image.bytesPerRow * h {
                let rowBytes = image.bytesPerRow
                bytes.withUnsafeBytes { source in rgba.withUnsafeMutableBytes { target in
                    for y in 0..<h { memcpy(target.baseAddress! + y * w * 4, source.baseAddress! + y * rowBytes, w * 4) }
                } }
                drawn = true
            }
        }
        if !drawn { drawn = rgba.withUnsafeMutableBytes { bytes -> Bool in
            guard let context = CGContext(data: bytes.baseAddress, width: w, height: h, bitsPerComponent: 8,
                bytesPerRow: w * 4, space: image.colorSpace ?? CGColorSpace(name: CGColorSpace.sRGB)!,
                bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue | CGBitmapInfo.byteOrder32Big.rawValue) else { return false }
            context.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
            return true
        } }
        guard drawn else { throw WorkerError.invalid("image decode failed") }
        let (ow, oh) = targetSize(width: w, height: h, budget: budget)
        // Same integer arithmetic as before, over flat tap tables and raw buffers,
        // with rows split across cores (each output row is independent).
        func table(_ taps: [(Int, [Int64])]) -> (first: [Int], count: [Int], stride: Int, values: [Int64]) {
            let stride = taps.map { $0.1.count }.max() ?? 0
            var values = [Int64](repeating: 0, count: taps.count * stride)
            for (i, tap) in taps.enumerated() { for (j, v) in tap.1.enumerated() { values[i * stride + j] = v } }
            return (taps.map { $0.0 }, taps.map { $0.1.count }, stride, values)
        }
        let columns = table(weights(input: w, output: ow)), rows = table(weights(input: h, output: oh))
        var horizontal = [UInt8](repeating: 0, count: h * ow * 3)
        var pixels = [Float](repeating: 0, count: 3 * ow * oh)
        let lanes = max(1, min(8, ProcessInfo.processInfo.activeProcessorCount / 2))
        rgba.withUnsafeBufferPointer { source in
            horizontal.withUnsafeMutableBufferPointer { target in
                columns.values.withUnsafeBufferPointer { values in
                    let (src, dst, vals) = (source.baseAddress!, target.baseAddress!, values.baseAddress!)
                    DispatchQueue.concurrentPerform(iterations: lanes) { lane in
                        for y in stride(from: lane, to: h, by: lanes) { for x in 0..<ow {
                            let first = columns.first[x], count = columns.count[x], tap = vals + x * columns.stride
                            var r: Int64 = 1 << 21, g: Int64 = 1 << 21, b: Int64 = 1 << 21
                            var at = src + (y * w + first) * 4
                            for j in 0..<count {
                                let t = tap[j]
                                r += Int64(at[0]) * t; g += Int64(at[1]) * t; b += Int64(at[2]) * t
                                at += 4
                            }
                            let o = dst + (y * ow + x) * 3
                            o[0] = UInt8(clamping: r >> 22); o[1] = UInt8(clamping: g >> 22); o[2] = UInt8(clamping: b >> 22)
                        } }
                    }
                }
            }
        }
        let plane = ow * oh
        horizontal.withUnsafeBufferPointer { source in
            pixels.withUnsafeMutableBufferPointer { target in
                rows.values.withUnsafeBufferPointer { values in
                    let (src, dst, vals) = (source.baseAddress!, target.baseAddress!, values.baseAddress!)
                    DispatchQueue.concurrentPerform(iterations: lanes) { lane in
                        for y in stride(from: lane, to: oh, by: lanes) {
                            let first = rows.first[y], count = rows.count[y], tap = vals + y * rows.stride
                            for x in 0..<ow {
                                var r: Int64 = 1 << 21, g: Int64 = 1 << 21, b: Int64 = 1 << 21
                                var at = src + (first * ow + x) * 3
                                for j in 0..<count {
                                    let t = tap[j]
                                    r += Int64(at[0]) * t; g += Int64(at[1]) * t; b += Int64(at[2]) * t
                                    at += ow * 3
                                }
                                let o = y * ow + x
                                dst[o] = Float(UInt8(clamping: r >> 22)) * Float(1.0 / 255)
                                dst[plane + o] = Float(UInt8(clamping: g >> 22)) * Float(1.0 / 255)
                                dst[2 * plane + o] = Float(UInt8(clamping: b >> 22)) * Float(1.0 / 255)
                            }
                        }
                    }
                }
            }
        }
        return (pixels, oh, ow)
    }
}

extension Media {
    /// Self-test fixtures only: an sRGB RGBA8 buffer as PNG.
    static func writePNG(rgba: [UInt8], width: Int, height: Int, to url: URL) throws {
        guard let provider = CGDataProvider(data: Data(rgba) as CFData),
              let image = CGImage(width: width, height: height, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: width * 4,
                                  space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipLast.rawValue),
                                  provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent),
              let destination = CGImageDestinationCreateWithURL(url as CFURL, "public.png" as CFString, 1, nil)
        else { throw WorkerError.invalid("self-test image") }
        CGImageDestinationAddImage(destination, image, nil)
        guard CGImageDestinationFinalize(destination) else { throw WorkerError.invalid("self-test image") }
    }
}
