// The viewer's audio player: play/pause, a waveform drawn once from the file (AVAudioFile, off the main thread),
// click or drag on it to scrub, elapsed and total time. Only playback moves the playhead; paused, nothing runs.
import AppKit
import AVFoundation

final class AudioPlayerView: NSView {
    static let height: CGFloat = 72
    private let url: URL
    private let player: AVPlayer
    private var timeObserver: Any?
    private var endObserver: NSObjectProtocol?
    private let button = CALayer()
    private let glyph = CALayer()
    private let dim = CALayer()
    private let bright = CALayer()
    private let brightMask = CALayer()
    private let elapsed = NSTextField(labelWithString: "0:00")
    private let total = NSTextField(labelWithString: "–:––")
    private(set) var peaks: [Float]?
    private var duration: Double = 0
    private var scrubbing = false
    private var playing: Bool { player.rate != 0 }

    init(url: URL) {
        self.url = url
        player = AVPlayer(url: url)
        player.actionAtItemEnd = .pause
        super.init(frame: CGRect(x: 0, y: 0, width: 400, height: Self.height))
        wantsLayer = true
        layer?.noImplicitAnimations()
        for l in [button, glyph, dim, bright, brightMask] { l.noImplicitAnimations() }
        button.backgroundColor = NSColor.white.cgColor
        button.cornerCurve = .continuous
        layer?.addSublayer(button)
        button.addSublayer(glyph)
        dim.contentsGravity = .resize
        bright.contentsGravity = .resize
        brightMask.backgroundColor = NSColor.black.cgColor
        brightMask.anchorPoint = .zero
        bright.mask = brightMask
        layer?.addSublayer(dim)
        layer?.addSublayer(bright)
        for f in [elapsed, total] {
            f.font = Look.smallMono
            f.textColor = Look.ink2
            addSubview(f)
        }
        total.alignment = .right
        setGlyph()
        timeObserver = player.addPeriodicTimeObserver(forInterval: CMTime(value: 1, timescale: 30), queue: .main) { [weak self] _ in
            self?.tick()
        }
        endObserver = NotificationCenter.default.addObserver(forName: .AVPlayerItemDidPlayToEndTime, object: player.currentItem, queue: .main) { [weak self] _ in
            self?.player.seek(to: .zero)
            self?.setGlyph()
            self?.tick()
        }
        let u = url
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let (p, d) = Self.readPeaks(u, bins: 160)
            DispatchQueue.main.async {
                guard let self else { return }
                self.peaks = p
                self.duration = d
                self.total.stringValue = Self.clock(d)
                self.drawWave()
            }
        }
    }
    required init?(coder: NSCoder) { fatalError() }
    override var isFlipped: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    private var scale: CGFloat { window?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 2 }
    private var waveRect: CGRect { CGRect(x: 58, y: 4, width: max(10, bounds.width - 58), height: 44) }

    override func layout() {
        super.layout()
        let d: CGFloat = 44
        button.frame = CGRect(x: 2, y: 4, width: d, height: d)
        button.cornerRadius = d / 2
        glyph.frame = button.bounds
        dim.frame = waveRect
        bright.frame = waveRect
        elapsed.frame = CGRect(x: waveRect.minX, y: 54, width: 100, height: 18)
        total.frame = CGRect(x: bounds.width - 100, y: 54, width: 100, height: 18)
        drawWave()
        tick()
    }

    private func setGlyph() {
        let cfg = NSImage.SymbolConfiguration(pointSize: 17, weight: .bold).applying(.init(paletteColors: [.black]))
        let img = NSImage(systemSymbolName: playing ? "pause.fill" : "play.fill", accessibilityDescription: nil)?.withSymbolConfiguration(cfg)
        glyph.contents = img?.layerContents(forContentsScale: scale)
        glyph.contentsScale = scale
        glyph.contentsGravity = .center
        // The play triangle sits optically centred a point to the right.
        glyph.frame = button.bounds.offsetBy(dx: playing ? 0 : 1.5, dy: 0)
    }

    private func drawWave() {
        let r = waveRect
        guard r.width > 10 else { return }
        dim.contents = Self.waveImage(peaks, size: r.size, color: Look.white(0.28), scale: scale)
        bright.contents = Self.waveImage(peaks, size: r.size, color: .white, scale: scale)
    }

    private func tick() {
        let t = player.currentTime().seconds
        let d = duration > 0 ? duration : (player.currentItem?.duration.seconds ?? 0)
        let f = d > 0 && t.isFinite ? CGFloat(min(1, max(0, t / d))) : 0
        brightMask.frame = CGRect(x: 0, y: 0, width: waveRect.width * f, height: waveRect.height)
        elapsed.stringValue = Self.clock(t.isFinite ? t : 0)
    }

    func toggle() {
        if playing { player.pause() } else { player.play() }
        setGlyph()
    }

    // Self-test hooks.
    var isPlaying: Bool { playing }
    var seconds: Double { player.currentTime().seconds }
    func mute() { player.isMuted = true }
    var buttonCentre: CGPoint { CGPoint(x: button.frame.midX, y: button.frame.midY) }
    func wavePoint(_ fraction: CGFloat) -> CGPoint { CGPoint(x: waveRect.minX + waveRect.width * fraction, y: waveRect.midY) }

    func pause() {
        player.pause()
        setGlyph()
    }

    func teardown() {
        player.pause()
        if let o = timeObserver { player.removeTimeObserver(o); timeObserver = nil }
        if let o = endObserver { NotificationCenter.default.removeObserver(o); endObserver = nil }
        player.replaceCurrentItem(with: nil)
    }

    /// Seeks to a fraction (0…1) of the file; the playhead follows at once.
    func seek(fraction: CGFloat) {
        let d = duration > 0 ? duration : (player.currentItem?.duration.seconds ?? 0)
        guard d.isFinite, d > 0 else { return }
        let t = CMTime(seconds: Double(min(1, max(0, fraction))) * d, preferredTimescale: 600)
        player.seek(to: t, toleranceBefore: .zero, toleranceAfter: .zero) { [weak self] _ in self?.tick() }
        brightMask.frame.size.width = waveRect.width * min(1, max(0, fraction))
        elapsed.stringValue = Self.clock(t.seconds)
    }

    override func mouseDown(with e: NSEvent) {
        let p = convert(e.locationInWindow, from: nil)
        if button.frame.insetBy(dx: -6, dy: -6).contains(p) { toggle(); return }
        if waveRect.insetBy(dx: 0, dy: -10).contains(p) {
            scrubbing = true
            seek(fraction: (p.x - waveRect.minX) / waveRect.width)
        }
    }

    override func mouseDragged(with e: NSEvent) {
        guard scrubbing else { return }
        let p = convert(e.locationInWindow, from: nil)
        seek(fraction: (p.x - waveRect.minX) / waveRect.width)
    }

    override func mouseUp(with e: NSEvent) { scrubbing = false }

    static func clock(_ s: Double) -> String {
        guard s.isFinite, s >= 0 else { return "0:00" }
        let i = Int(s.rounded(.down))
        return i >= 3600 ? String(format: "%d:%02d:%02d", i / 3600, i / 60 % 60, i % 60) : String(format: "%d:%02d", i / 60, i % 60)
    }

    /// RMS loudness per bin, 0…1, read from evenly spaced windows (bounded cost on long files), and the duration.
    static func readPeaks(_ url: URL, bins: Int) -> ([Float]?, Double) {
        guard let f = try? AVAudioFile(forReading: url) else { return (nil, 0) }
        let frames = f.length
        let sr = f.processingFormat.sampleRate
        let duration = sr > 0 ? Double(frames) / sr : 0
        guard frames > 0, let buf = AVAudioPCMBuffer(pcmFormat: f.processingFormat, frameCapacity: 16_384) else { return (nil, duration) }
        let binLen = max(1, frames / AVAudioFramePosition(bins))
        var out = [Float](repeating: 0, count: bins)
        for b in 0..<bins {
            let start = AVAudioFramePosition(b) * binLen
            guard start < frames else { break }
            f.framePosition = start
            let want = AVAudioFrameCount(min(Int64(16_384), binLen, frames - start))
            guard (try? f.read(into: buf, frameCount: want)) != nil, let ch = buf.floatChannelData else { continue }
            let n = Int(buf.frameLength)
            guard n > 0 else { continue }
            var sum: Float = 0
            for c in 0..<Int(buf.format.channelCount) {
                let p = ch[c]
                for i in 0..<n { sum += p[i] * p[i] }
            }
            out[b] = (sum / Float(n * Int(buf.format.channelCount))).squareRoot()
        }
        let top = max(out.max() ?? 0, 0.0001)
        return (out.map { min(1, $0 / top) }, duration)
    }

    /// Rounded bars (2.5 pt wide, 2 pt apart), centred, at least a dot tall; a flat line while the peaks load.
    static func waveImage(_ peaks: [Float]?, size: CGSize, color: NSColor, scale: CGFloat) -> CGImage? {
        let w = Int(size.width * scale), h = Int(size.height * scale)
        guard w > 0, h > 0, let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                                                space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.scaleBy(x: scale, y: scale)
        ctx.setFillColor(color.cgColor)
        let barW: CGFloat = 2.5, gap: CGFloat = 2
        let count = Int((size.width + gap) / (barW + gap))
        for i in 0..<max(0, count) {
            var v: CGFloat = 0
            if let peaks, !peaks.isEmpty {
                // Each bar takes the loudest bin it covers.
                let a = i * peaks.count / count, b = max(a + 1, (i + 1) * peaks.count / count)
                v = CGFloat(peaks[a..<min(b, peaks.count)].max() ?? 0)
            }
            let bh = max(barW, size.height * (0.06 + 0.94 * v))
            let r = CGRect(x: CGFloat(i) * (barW + gap), y: (size.height - bh) / 2, width: barW, height: bh)
            ctx.addPath(CGPath(roundedRect: r, cornerWidth: barW / 2, cornerHeight: barW / 2, transform: nil))
        }
        ctx.fillPath()
        return ctx.makeImage()
    }
}
