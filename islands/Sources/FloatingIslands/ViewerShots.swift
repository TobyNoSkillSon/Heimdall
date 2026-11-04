// Offscreen renders of the viewer, one PNG per file kind, at real size (2×), on a dark backdrop with the island's
// chrome (black continuous-corner shape, hairline, symbol, title, close button). No window is shown: the views live
// in a window that is never ordered in; WebKit content comes from its own snapshot, video from its first frame.
//   FloatingIslands --viewer-shots <out-dir> <fixtures-dir>
import AppKit
import IslandModel
import WebKit

enum ViewerShots {
    struct Case { let name, title, file: String; var actions: [ViewerAction] = []; var click: Int? = nil }

    static func run(out: String, fixtures: String) -> Never {
        let app = NSApplication.shared
        app.setActivationPolicy(.prohibited)
        let outURL = URL(fileURLWithPath: out)
        try? FileManager.default.createDirectory(at: outURL, withIntermediateDirectories: true)
        let fx = URL(fileURLWithPath: fixtures)
        let cases: [Case] = [
            Case(name: "markdown", title: "Cleanup plan", file: "plan.md"),
            Case(name: "code", title: "sizes.rs", file: "sizes.rs"),
            Case(name: "image", title: "Dusk", file: "dusk.png"),
            Case(name: "pdf", title: "Report", file: "report.pdf"),
            Case(name: "audio", title: "Melody", file: "melody.m4a"),
            Case(name: "video", title: "Clip", file: "clip.mp4"),
            Case(name: "html", title: "Latency", file: "page.html"),
            Case(name: "quicklook", title: "Letter", file: "letter.docx"),
            Case(name: "missing", title: "Old notes", file: "gone.md"),
            Case(name: "actions", title: "Cleanup plan", file: "plan.md",
                 actions: [ViewerAction(label: "Do all of it", action: "do-all"), ViewerAction(label: "Only caches", action: "caches"),
                           ViewerAction(label: "Not now", action: "later")]),
            Case(name: "actions-chosen", title: "Cleanup plan", file: "plan.md",
                 actions: [ViewerAction(label: "Do all of it", action: "do-all"), ViewerAction(label: "Only caches", action: "caches"),
                           ViewerAction(label: "Not now", action: "later")], click: 1),
        ]
        let screen = CGSize(width: 1728, height: 1117)
        let window = NSWindow(contentRect: CGRect(x: -30000, y: -30000, width: 1200, height: 1000), styleMask: [.borderless], backing: .buffered, defer: false)
        window.appearance = NSAppearance(named: .darkAqua)
        let host = FlippedView(frame: CGRect(x: 0, y: 0, width: 1200, height: 1000))
        window.contentView = host
        var report: [String] = []
        var i = 0
        func next() {
            guard i < cases.count else {
                try? report.joined(separator: "\n").appending("\n").write(to: outURL.appendingPathComponent("report.txt"), atomically: true, encoding: .utf8)
                print(report.joined(separator: "\n"))
                exit(0)
            }
            let c = cases[i]
            i += 1
            let v = FileViewer(path: fx.appendingPathComponent(c.file).path, actions: c.actions)
            var clicked: String?
            v.onAction = { clicked = $0.action }
            var resized = false
            v.onResize = { resized = true }
            let size = v.islandSize(screen: screen)
            let body = FileViewer.bodyRect(size)
            host.subviews.forEach { $0.removeFromSuperview() }
            let r = v.makeView(size: body.size)
            r.frame = CGRect(origin: CGPoint(x: 20, y: 20), size: body.size)
            host.addSubview(r)
            if let k = c.click, let row = r.subviews.first(where: { $0 is ActionRow }) as? ActionRow { row.click(k); row.confirm(true) }
            // Let async loads land (text, PDF, waveform, poster, page load).
            let wait: Double = [.html, .other, .video, .pdf].contains(v.kind) ? 2.5 : 0.8
            DispatchQueue.main.asyncAfter(deadline: .now() + wait) {
                let size = resized ? v.islandSize(screen: screen) : size
                let body = FileViewer.bodyRect(size)
                if resized { r.frame = CGRect(origin: CGPoint(x: 20, y: 20), size: body.size); v.layout(body.size) }
                finish(v, c, size, body) { img in
                    let png = outURL.appendingPathComponent("viewer-\(c.name).png")
                    if let img, let data = NSBitmapImageRep(cgImage: img).representation(using: .png, properties: [:]) { try? data.write(to: png) }
                    report.append("\(c.name): kind=\(v.kind.name) island=\(Int(size.width))x\(Int(size.height)) actions=\(c.actions.count)"
                                  + (clicked.map { " clicked=\($0)" } ?? "") + (img == nil ? " FAILED" : ""))
                    v.teardown()
                    next()
                }
            }
        }
        DispatchQueue.main.async { next() }
        app.run()
        exit(0)
    }

    private static func finish(_ v: FileViewer, _ c: Case, _ size: CGSize, _ body: CGRect, _ done: @escaping (CGImage?) -> Void) {
        guard let rep = v.snapshot(size: body.size), let content = rep.cgImage else { done(nil); return }
        if let web = v.webView {
            let cfg = WKSnapshotConfiguration()
            cfg.rect = web.bounds
            web.takeSnapshot(with: cfg) { img, _ in
                done(compose(c.title, v.kind, size, body, content, overlay: img.flatMap { ($0.cgImage(forProposedRect: nil, context: nil, hints: nil), web.frame) }))
            }
            return
        }
        done(compose(c.title, v.kind, size, body, content, overlay: nil))
    }

    /// The island as it looks open: backdrop, black shape, hairline, header, then the body bitmap.
    private static func compose(_ title: String, _ kind: ViewerKind, _ size: CGSize, _ body: CGRect, _ content: CGImage,
                                overlay: (CGImage?, CGRect)?) -> CGImage? {
        let margin: CGFloat = 28, s: CGFloat = 2
        let W = size.width + 2 * margin, H = size.height + 2 * margin
        guard let ctx = CGContext(data: nil, width: Int(W * s), height: Int(H * s), bitsPerComponent: 8, bytesPerRow: 0,
                                  space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.scaleBy(x: s, y: s)
        // Flip to y-down so everything below reads like the island's own coordinates.
        ctx.translateBy(x: 0, y: H)
        ctx.scaleBy(x: 1, y: -1)
        ctx.setFillColor(NSColor(srgbRed: 0.17, green: 0.18, blue: 0.21, alpha: 1).cgColor)
        ctx.fill(CGRect(x: 0, y: 0, width: W, height: H))
        let shape = CGRect(x: margin, y: margin, width: size.width, height: size.height)
        let path = CGPath(roundedRect: shape, cornerWidth: Look.radius(size.height), cornerHeight: Look.radius(size.height), transform: nil)
        ctx.addPath(path)
        ctx.setFillColor(NSColor.black.cgColor)
        ctx.fillPath()
        let ns = NSGraphicsContext(cgContext: ctx, flipped: true)
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = ns
        // Header: symbol, title, close.
        let cfg = NSImage.SymbolConfiguration(pointSize: Look.iconSize, weight: .semibold).applying(.init(paletteColors: [.white]))
        var titleX = shape.minX + Look.pad + 28
        if let sym = NSImage(systemSymbolName: FileViewer.symbol(kind), accessibilityDescription: nil)?.withSymbolConfiguration(cfg) {
            titleX = shape.minX + Look.pad + max(22, sym.size.width) + Look.headerGap
            let r = CGRect(x: shape.minX + Look.pad, y: shape.minY + (Look.pillH - sym.size.height) / 2 + 4, width: sym.size.width, height: sym.size.height)
            sym.draw(in: r, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
        }
        (title as NSString).draw(at: CGPoint(x: titleX, y: shape.minY + 12),
                                 withAttributes: [.font: Look.title, .foregroundColor: Look.ink])
        let xc = NSImage.SymbolConfiguration(pointSize: 18, weight: .semibold).applying(.init(paletteColors: [Look.white(0.9), Look.white(0.18)]))
        if let x = NSImage(systemSymbolName: "xmark.circle.fill", accessibilityDescription: nil)?.withSymbolConfiguration(xc) {
            let r = CGRect(x: shape.maxX - Look.pad - 4 - x.size.width / 2, y: shape.minY + Look.pillH / 2 + 4 - x.size.height / 2, width: x.size.width, height: x.size.height)
            x.draw(in: r, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
        }
        NSGraphicsContext.restoreGraphicsState()
        // Body (bitmaps are y-up: draw them flipped back).
        func drawImage(_ img: CGImage, in r: CGRect) {
            ctx.saveGState()
            ctx.translateBy(x: r.minX, y: r.maxY)
            ctx.scaleBy(x: 1, y: -1)
            ctx.draw(img, in: CGRect(origin: .zero, size: r.size))
            ctx.restoreGState()
        }
        ctx.saveGState()
        ctx.addPath(path)
        ctx.clip()
        let b = body.offsetBy(dx: margin, dy: margin)
        drawImage(content, in: b)
        if let (img, frame) = overlay, let img { drawImage(img, in: frame.offsetBy(dx: b.minX, dy: b.minY)) }
        ctx.restoreGState()
        ctx.addPath(path)
        ctx.setStrokeColor(Look.hairline)
        ctx.setLineWidth(Look.hairlineWidth)
        ctx.strokePath()
        return ctx.makeImage()
    }
}
