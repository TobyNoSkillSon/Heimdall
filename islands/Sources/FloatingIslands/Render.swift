// Lab renders: islands built offscreen (no window is ever ordered on screen), their layer trees drawn into PNGs at
// the display's scale on a dark desktop-like backdrop; animations as contact sheets of presentation frames.
//   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --render <out dir>
// With FLOATING_ISLANDS_RENDER_EXISTING=1 the lab folder's own islands are drawn as they are (one PNG per island,
// named by id), nothing is seeded or deleted: the check for what a writer (the plan refresher) produced.
import AppKit
import IslandModel
import ImageIO
import UniformTypeIdentifiers

final class Render {
    let app: AppController
    let out: URL
    let scale: CGFloat = 2
    var log: [String] = []
    init(_ app: AppController, out: String) {
        self.app = app
        self.out = URL(fileURLWithPath: out, isDirectory: true)
    }

    var screen: CGRect { NSScreen.main!.frame }

    func run() {
        guard ProcessInfo.processInfo.environment["FLOATING_ISLANDS_DIR"] != nil else { print("render: needs FLOATING_ISLANDS_DIR"); exit(2) }
        try? FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
        if ProcessInfo.processInfo.environment["FLOATING_ISLANDS_RENDER_EXISTING"] == "1" { return existing() }
        for d in [Paths.islands, Paths.responses] {
            for f in (try? FileManager.default.contentsOfDirectory(at: d, includingPropertiesForKeys: nil)) ?? [] { try? FileManager.default.removeItem(at: f) }
        }
        put("todo", ["preset": "todo", "list": "todo", "expanded": true, "items": [
            ["id": "arsenal", "title": "Arsenal: push branch", "group": "running"],
            ["id": "qwen", "title": "Qwen-Image into Arsenal", "group": "running", "detail": "Converting weights"],
            ["id": "caches", "title": "Delete old caches", "group": "waiting",
             "proposal": ["text": "~/.cache/huggingface  12.4 GB\n~/Library/Caches/pip  3.1 GB\nlab/drama3 renders  0.9 GB"]],
            ["id": "ipad", "title": "iPad sale: answer the buyer", "group": "waiting"],
            ["id": "echo", "title": "Echo: Heimdall hears itself", "group": "discussed"],
            ["id": "site", "title": "Arsenal website rebuild", "group": "discussed", "done": true]]])
        put("scheduled", ["preset": "todo", "list": "scheduled", "expanded": true, "items": [
            // Due times count down from the render's clock: within the hour (full white), later today, tomorrow, days.
            ["id": "menu", "title": "Heimdall: menu revamp", "due": Date().timeIntervalSince1970 + 25 * 60],
            ["id": "phonon", "title": "Phonon voices as presets", "due": Date().timeIntervalSince1970 + 3 * 3600 + 60],
            ["id": "review", "title": "Review night pass", "due": Self.tomorrow(10)],
            ["id": "tap", "title": "Arsenal: Homebrew tap", "due": Date().timeIntervalSince1970 + 3 * 86400],
            ["id": "old", "title": "Vella: research notes", "when": "Sat"]]])
        put("settled", ["preset": "todo", "list": "settled", "items": [
            ["id": "s1", "title": "Night pass stage 1: bug fixes"], ["id": "s2", "title": "Islands bundled in Heimdall"]]])
        put("job-q", ["title": "iPhone build", "state": "waiting", "stage": "Your answer", "attention": "question", "icon": "iphone"])
        put("job-bell", ["title": "Decider data", "state": "waiting", "stage": "Your test", "attention": "waiting"])
        put("job-run", ["title": "Arsenal site", "state": "running", "stage": "Building", "icon": "globe"])

        var t = 1.6
        func at(_ dt: Double, _ f: @escaping () -> Void) { t += dt; DispatchQueue.main.asyncAfter(deadline: .now() + t, execute: f) }

        at(0) { [self] in
            shot("layout", rect: screen)
            shot("todo-open", ids: ["todo"])
            shot("scheduled-open", ids: ["scheduled"])
            shot("settled-pill", ids: ["settled"])
            shot("jobs-attention-rest", ids: ["job-q", "job-bell", "job-run"])
        }
        // Attention pulse: frames of one pulse on the two attention pills.
        at(0.2) { [self] in
            app.controllers["job-q"]?.pulseAttention()
            app.controllers["job-bell"]?.pulseAttention()
            sheet("anim-attention", ids: ["job-q", "job-bell"], times: [0, 0.1, 0.2, 0.29, 0.4, 0.6, 0.9, 1.3])
        }
        // Approval: open the proposal (animated), then the choice.
        at(2.0) { [self] in
            app.controllers["todo"]?.labPerform(.disclose(key: "caches"))
            sheet("anim-approval-open", ids: ["todo"], times: [0, 0.05, 0.1, 0.16, 0.24, 0.36, 0.6, 1.0])
        }
        at(1.6) { [self] in
            shot("approval-open", ids: ["todo"])
            app.controllers["todo"]?.labPerform(.approve(key: "caches", action: "yes"))
        }
        at(0.4) { [self] in shot("approval-chosen", ids: ["todo"]) }
        at(1.6) { [self] in
            shot("approval-closed-with-choice", ids: ["todo"])
            // Add: the agent adds an item.
            patchItems("todo") { $0 + [["id": "voice", "title": "Heimdall: AirPods only while speaking", "group": "running"]] }
            sheet("anim-add", ids: ["todo"], times: [0, 0.05, 0.1, 0.16, 0.24, 0.4, 0.8, 1.5])
        }
        at(2.0) { [self] in
            // Tick: the user ticks the first running item; it fades to done and sinks to the bottom of Running.
            app.controllers["todo"]?.labPerform(.tick(key: "arsenal", title: ""))
            sheet("anim-tick", ids: ["todo"], times: [0, 0.05, 0.1, 0.16, 0.24, 0.36, 0.6, 1.0])
        }
        at(1.6) { [self] in
            shot("todo-ticked", ids: ["todo"])
            // Move: Echo moves from Discussed to Running.
            patchItems("todo") { $0.map { var i = $0; if i["id"] as? String == "echo" { i["group"] = "running" }; return i } }
            sheet("anim-move", ids: ["todo"], times: [0, 0.05, 0.1, 0.16, 0.24, 0.36, 0.6, 1.4])
        }
        at(2.0) { [self] in
            // Remove: the iPad item goes; the rest close up and the shape shrinks on the same spring.
            patchItems("todo") { $0.filter { $0["id"] as? String != "ipad" } }
            sheet("anim-remove", ids: ["todo"], times: [0, 0.05, 0.1, 0.16, 0.24, 0.36, 0.6, 1.0])
        }
        at(1.6) { [self] in
            // Between lists: Phonon leaves Scheduled for Settled (open both).
            _ = IslandFile.patch("settled") { $0["expanded"] = true }
        }
        at(1.0) { [self] in
            shot("settled-open", ids: ["settled"])
            var moved: JSONObject?
            patchItems("scheduled") { items in moved = items.first { $0["id"] as? String == "phonon" }; return items.filter { $0["id"] as? String != "phonon" } }
            patchItems("settled") { $0 + [moved ?? [:]] }
            sheet("anim-between-lists", ids: ["scheduled", "settled"], times: [0, 0.05, 0.1, 0.16, 0.24, 0.36, 0.6, 1.5])
        }
        at(2.0) { [self] in
            // Notch origin: a new job grows out of the notch into its slot.
            put("job-new", ["title": "Port voices", "state": "running", "stage": "Starting", "icon": "waveform", "origin": ["from": "notch"]])
            let n = IslandController.notchRect(NSScreen.main)
            let r = CGRect(x: n.minX - 40, y: screen.maxY - 300, width: screen.maxX - n.minX + 40, height: 300)
            sheet("anim-notch", rect: r, times: [0.02, 0.06, 0.1, 0.15, 0.2, 0.28, 0.4, 0.8], cols: 2)
        }
        at(2.2) { [self] in
            shot("layout-after", rect: screen)
            let busy = app.controllers.values.flatMap { c in c.animatedLayers().filter { !(c.attentionShown != nil && $0.contains("attn")) }.map { "\(c.id): \($0)" } }
            print("render: at rest \(busy.isEmpty ? "nothing animating" : busy.joined(separator: ", "))")
            for l in log { print(l) }
            for d in [Paths.islands, Paths.responses] {
                for f in (try? FileManager.default.contentsOfDirectory(at: d, includingPropertiesForKeys: nil)) ?? [] { try? FileManager.default.removeItem(at: f) }
            }
            exit(0)
        }
    }

    /// The folder's islands as they are, each alone, once their arrival has settled; then the whole layout.
    private func existing() {
        // An open To do list shows its first waiting proposal opened, buttons and all.
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) { [self] in
            for c in app.controllers.values where c.island.preset == "todo" && c.island.expanded {
                // FLOATING_ISLANDS_RENDER_OPEN="key,key" opens those rows instead ("none": every row closed).
                if let keys = ProcessInfo.processInfo.environment["FLOATING_ISLANDS_RENDER_OPEN"]?.split(separator: ",").map(String.init) {
                    for it in c.island.planItems where keys.contains(it.key) { c.labPerform(.disclose(key: it.key)) }
                } else if let it = c.island.planItems.first(where: { $0.proposal != nil && !$0.done && $0.group == "waiting" }) { c.labPerform(.disclose(key: it.key)) }
            }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.0) { [self] in
            for id in app.controllers.keys.sorted() { shot(id, ids: [id]) }
            shot("layout", rect: screen)
            // The running islands' sign of work: frames across one pulse (the first comes 4 s after they were built),
            // then still frames after it.
            let running = app.controllers.keys.sorted().filter { app.controllers[$0]?.island.look == "task" }
            for id in running { sheet("work-\(id)", ids: [id], times: [2.0, 2.25, 2.45, 2.65, 2.85, 3.3, 4.2]) }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 6.8) { [self] in
            for l in log { print(l) }
            exit(0)
        }
    }

    private func put(_ id: String, _ o: JSONObject) { try? IslandFile.writeAtomic(o.merging(["id": id]) { a, _ in a }, to: Paths.file(id)) }
    private func patchItems(_ id: String, _ f: ([JSONObject]) -> [JSONObject]) {
        _ = IslandFile.patch(id) { o in o["items"] = f(o["items"] as? [JSONObject] ?? []) }
    }

    /// The rect around some islands (where their shapes are on screen now), with room around them.
    private func around(_ ids: [String], pad: CGFloat = 20) -> CGRect {
        ids.compactMap { app.controllers[$0] }.map { $0.panel.frame.union($0.visibleShapeRect()) }.reduce(CGRect.null) { $0.union($1) }
            .insetBy(dx: -pad, dy: -pad).integral
    }

    /// Draw the islands' current presentation into an image of `rect` (screen points, y up).
    private func image(_ rect: CGRect) -> CGImage? {
        let w = Int(rect.width * scale), h = Int(rect.height * scale)
        guard let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.scaleBy(x: scale, y: scale)
        // A dark desktop: a soft vertical gradient, so black islands and their hairlines read as they do on screen.
        let g = CGGradient(colorsSpace: nil, colors: [Look.rgb(0x2B2E35).cgColor, Look.rgb(0x1A1C21).cgColor] as CFArray, locations: [0, 1])!
        ctx.drawLinearGradient(g, start: CGPoint(x: 0, y: rect.height), end: .zero, options: [])
        for c in app.controllers.values.sorted(by: { $0.panel.level.rawValue < $1.panel.level.rawValue }) where !c.removing || c.shape.presentation() != nil {
            let f = c.panel.frame
            guard f.intersects(rect) else { continue }
            ctx.saveGState()
            // The root layer is flipped (y down) inside a window whose origin is its bottom-left corner.
            ctx.translateBy(x: f.minX - rect.minX, y: f.maxY - rect.minY)
            ctx.scaleBy(x: 1, y: -1)
            let root = c.view.layer!
            (root.presentation() ?? root).render(in: ctx)
            ctx.restoreGState()
        }
        return ctx.makeImage()
    }

    static func tomorrow(_ hour: Int) -> Double {
        let c = Calendar.current
        let d = c.date(byAdding: .day, value: 1, to: Date())!
        return c.date(bySettingHour: hour, minute: 0, second: 0, of: d)!.timeIntervalSince1970
    }

    private func write(_ img: CGImage, _ name: String) {
        let url = out.appendingPathComponent(name + ".png")
        guard let d = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil) else { return }
        CGImageDestinationAddImage(d, img, [kCGImagePropertyDPIWidth: 144, kCGImagePropertyDPIHeight: 144] as CFDictionary)
        CGImageDestinationFinalize(d)
        log.append("render: \(url.path) \(img.width)×\(img.height)")
    }

    func shot(_ name: String, ids: [String]? = nil, rect: CGRect? = nil) {
        let r = rect ?? around(ids ?? Array(app.controllers.keys))
        if let img = image(r) { write(img, name) }
    }

    /// Frames of an animation at the given seconds after now, laid out left to right (or in `cols` columns), each
    /// labelled with its time. The crop is fixed at the union of where the islands are at the start and at the end.
    func sheet(_ name: String, ids: [String] = [], rect: CGRect? = nil, times: [Double], cols: Int? = nil) {
        var frames: [(Double, CGImage)] = []
        var rects: [CGRect] = []
        let t0 = CACurrentMediaTime()
        // Capture the full screen per frame (cheap enough at this size), crop once the end place is known.
        for t in times {
            DispatchQueue.main.asyncAfter(deadline: .now() + t) { [self] in
                if let img = image(screen) { frames.append((CACurrentMediaTime() - t0, img)) }
                if rect == nil { rects.append(around(ids)) }
                guard frames.count == times.count else { return }
                let crop = (rect ?? rects.reduce(CGRect.null) { $0.union($1) }).intersection(screen).integral
                compose(name, frames: frames, crop: crop, cols: cols ?? times.count)
            }
        }
    }

    private func compose(_ name: String, frames: [(Double, CGImage)], crop: CGRect, cols: Int) {
        let cw = crop.width * scale, ch = crop.height * scale, gap: CGFloat = 12 * scale, label: CGFloat = 22 * scale
        let rows = Int(ceil(Double(frames.count) / Double(cols)))
        let W = Int(CGFloat(cols) * cw + CGFloat(cols + 1) * gap), H = Int(CGFloat(rows) * (ch + label) + CGFloat(rows + 1) * gap)
        guard let ctx = CGContext(data: nil, width: W, height: H, bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return }
        ctx.setFillColor(Look.rgb(0x111214).cgColor)
        ctx.fill(CGRect(x: 0, y: 0, width: W, height: H))
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: ctx, flipped: false)
        for (k, (t, img)) in frames.enumerated() {
            let col = k % cols, row = k / cols
            let x = gap + CGFloat(col) * (cw + gap)
            let yTop = CGFloat(H) - gap - CGFloat(row) * (ch + label + gap)
            // Crop in the full-screen frame's pixel space (y down in CGImage cropping).
            let px = CGRect(x: (crop.minX - screen.minX) * scale, y: (screen.maxY - crop.maxY) * scale, width: cw, height: ch)
            if let part = img.cropping(to: px) { ctx.draw(part, in: CGRect(x: x, y: yTop - label - ch, width: cw, height: ch)) }
            let s = String(format: "%.2f s", t) as NSString
            s.draw(at: CGPoint(x: x + 2 * scale, y: yTop - label + 4 * scale),
                   withAttributes: [.font: NSFont.monospacedDigitSystemFont(ofSize: 12 * scale, weight: .medium), .foregroundColor: Look.white(0.75)])
        }
        NSGraphicsContext.restoreGraphicsState()
        if let img = ctx.makeImage() { write(img, name) }
    }
}
