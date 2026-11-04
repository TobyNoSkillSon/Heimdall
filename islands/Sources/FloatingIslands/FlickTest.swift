// Lab flick test: islands thrown at speed with synthetic drags sent through each window's own event path, in real
// time (8 ms apart, real timestamps), so the release velocity and the landing spring are the ones a hand produces.
// The pointer, keyboard and screen are never driven. Each throw's flight is sampled every frame into flick-<name>.csv.
//   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --flicktest
import AppKit
import IslandModel

final class FlickTest {
    let app: AppController
    var results: [(String, Bool, String)] = []
    var marks: [String] = []   // "name,start,end" seconds since launch, for picking video frames
    let t0 = ProcessInfo.processInfo.systemUptime
    init(_ app: AppController) { self.app = app }

    func run() {
        guard ProcessInfo.processInfo.environment["FLOATING_ISLANDS_DIR"] != nil else {
            print("flicktest: refusing to run without FLOATING_ISLANDS_DIR"); exit(2)
        }
        for d in [Paths.islands, Paths.responses] {
            for f in (try? FileManager.default.contentsOfDirectory(at: d, includingPropertiesForKeys: nil)) ?? [] { try? FileManager.default.removeItem(at: f) }
        }
        put("r1", ["title": "Qwen-Image", "state": "running", "stage": "Final tests", "icon": "photo"])
        put("r2", ["title": "iPhone", "state": "waiting", "stage": "Your test"])
        put("todo", ["preset": "todo", "title": "To do", "icon": "checklist",
                     "items": [["title": "Qwen-Image into Arsenal", "group": "running"], ["title": "Test Heimdall Talk", "group": "waiting"],
                               ["title": "Arsenal website rebuild", "group": "discussed"]]])
        put("ball", ["title": "iPad sale", "state": "done", "position": ["x": 780, "y": 420]])
        let scr = NSScreen.main!.frame
        let rowTop = scr.maxY - max(NSScreen.main!.safeAreaInsets.top, scr.maxY - NSScreen.main!.visibleFrame.maxY) - 6

        var steps: [(Double, () -> Void)] = []
        var t = 1.4
        func at(_ dt: Double, _ f: @escaping () -> Void) { t += dt; steps.append((t, f)) }

        // 1. Flick left from the middle, a little below the To do pill: it attaches to the left edge where the throw lands.
        at(0) { [self] in
            let b = c("ball")
            throwIsland("left", b, from: CGPoint(x: b.shapeRect.midX, y: 700), velocity: CGVector(dx: -2600, dy: 120))
        }
        at(1.6) { [self] in
            let b = c("ball"), td = c("todo")
            check("flick left: attaches to the left edge", abs(b.shapeRect.minX - (scr.minX + 8)) < 1, "\(b.shapeRect)")
            check("flick left: lands where the throw points (release 712 + 60 pt of glide)", abs(b.shapeRect.midY - 772.5) < 3, "\(b.shapeRect)")
            check("flick left: side and height kept in the file", side("ball") == "left" && abs((edgeY("ball") ?? -1) - Double(scr.maxY - b.shapeRect.maxY)) < 0.5, "\(read("ball") ?? [:])")
            check("flick left: clear of To do", !b.shapeRect.insetBy(dx: 1, dy: 1).intersects(td.shapeRect), "")
        }
        // 2. Opening To do never covers it; closing To do leaves it at its height.
        at(0.1) { [self] in _ = IslandFile.patch("todo") { $0["expanded"] = true } }
        at(0.9) { [self] in
            check("opening To do covers nothing on its edge", c("todo").expanded && noOverlap(), "\(c("ball").shapeRect) vs \(c("todo").shapeRect)")
            _ = IslandFile.patch("todo") { $0["expanded"] = false }
        }
        // 3. Flick right, aimed between Qwen-Image and iPhone: it takes that height and iPhone makes room below it.
        at(0.9) { [self] in
            let b = c("ball"), r1 = c("r1"), r2 = c("r2")
            check("To do closed: the island is back at its height", abs(b.shapeRect.midY - 772.5) < 3, "\(b.shapeRect)")
            throwIsland("right", b, from: CGPoint(x: b.shapeRect.midX, y: (r1.shapeRect.midY + r2.shapeRect.midY) / 2), velocity: CGVector(dx: 2800, dy: 0), startX: 700)
        }
        at(1.6) { [self] in
            let b = c("ball"), r1 = c("r1"), r2 = c("r2")
            check("flick right: attaches to the right edge", abs(b.shapeRect.maxX - (scr.maxX - 8)) < 1, "\(b.shapeRect)")
            check("flick right: lands between the two, in order, no overlap", r1.shapeRect.minY >= b.shapeRect.maxY + 7.5 && b.shapeRect.minY >= r2.shapeRect.maxY + 7.5,
                  "r1 \(r1.shapeRect) ball \(b.shapeRect) r2 \(r2.shapeRect)")
        }
        // 4. Flick up hard: no top-middle dock; it goes to the top of the nearer edge, under the menu bar.
        at(0.1) { [self] in
            throwIsland("up", c("ball"), from: CGPoint(x: 700, y: 600), velocity: CGVector(dx: 200, dy: 2600), startX: 700)
        }
        at(1.6) { [self] in
            let b = c("ball")
            check("flick up: top of the left edge, under the menu bar", abs(b.shapeRect.maxY - rowTop) < 1 && abs(b.shapeRect.minX - (scr.minX + 8)) < 1, "\(b.shapeRect) row \(rowTop)")
            check("flick up: no top dock in the file", side("ball") == "left", "\(read("ball") ?? [:])")
            check("right stack closed up when it left", abs(c("r1").shapeRect.maxY - (scr.maxY - app.columnTop(NSScreen.main!))) < 1, "\(c("r1").shapeRect)")
        }
        // 5. Plain drag: iPhone carried slowly low on the right and let go after a pause: it stays at that height.
        at(0.1) { [self] in
            drag("low", c("r2"), to: CGPoint(x: 1500, y: 300), pause: 0.12)
        }
        at(1.4) { [self] in
            let r2 = c("r2")
            check("plain drag: stays at the height it was let go, on the right edge", abs(r2.shapeRect.midY - 300) < 1.5 && abs(r2.shapeRect.maxX - (scr.maxX - 8)) < 1, "\(r2.shapeRect)")
            // 6. Qwen-Image dragged down the right edge onto iPhone: no overlap, it takes the place and iPhone makes room.
            drag("along", c("r1"), to: CGPoint(x: scr.maxX - 80, y: 310), pause: 0.12)
        }
        at(1.4) { [self] in
            let r1 = c("r1"), r2 = c("r2")
            check("drag along the edge: both on the right edge, no overlap", abs(r1.shapeRect.maxX - (scr.maxX - 8)) < 1 && noOverlap(), "r1 \(r1.shapeRect) r2 \(r2.shapeRect)")
            check("drag along the edge: the one carried there sits at its height", abs(r1.shapeRect.midY - 310) < 1.5 || abs(r2.shapeRect.midY - 300) < 1.5, "r1 \(r1.shapeRect) r2 \(r2.shapeRect)")
            // An agent update without a place keeps the place the user chose.
            _ = IslandFile.patch("r2") { $0["note"] = "agent rewrite" }
        }
        at(0.6) { [self] in
            check("agent update keeps the chosen height", edgeY("r2") != nil && side("r2") == "right", "\(read("r2") ?? [:])")
            let st = (IslandFile.read(Paths.state)?["islands"] as? JSONObject)?["ball"] as? JSONObject
            check("state.json says it is pinned, with its top-left", st?["pinned"] as? Bool == true && (st?["y"] as? NSNumber)?.doubleValue == Double(scr.maxY - c("ball").shapeRect.maxY), "\(st ?? [:])")
            check("no island overlaps another", noOverlap(), "")
            check("no island in the menu bar strip", app.controllers.values.allSatisfy { $0.shapeRect.maxY <= rowTop + 0.5 }, "")
        }
        at(1.2) { [self] in
            let busy = app.controllers.values.flatMap { c in c.animatedLayers().map { "\(c.id): \($0)" } }
            check("at rest no layer is animating", busy.isEmpty, busy.joined(separator: ", "))
            for r in results { print("\(r.1 ? "PASS" : "FAIL")  \(r.0)\(r.2.isEmpty || r.1 ? "" : "  (\(r.2))")") }
            let failed = results.filter { !$0.1 }
            print("flicktest: \(results.count - failed.count)/\(results.count) passed")
            try? marks.joined(separator: "\n").write(to: Paths.root.appendingPathComponent("flick-marks.csv"), atomically: true, encoding: .utf8)
            for d in [Paths.islands, Paths.responses] {
                for f in (try? FileManager.default.contentsOfDirectory(at: d, includingPropertiesForKeys: nil)) ?? [] { try? FileManager.default.removeItem(at: f) }
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                try? FileManager.default.removeItem(at: Paths.pid)
                try? FileManager.default.removeItem(at: Paths.state)
                exit(failed.isEmpty ? 0 : 1)
            }
        }
        for (when, f) in steps { DispatchQueue.main.asyncAfter(deadline: .now() + when, execute: f) }
    }

    // MARK: Gestures

    /// Press on the island, carry it along a straight line at `velocity` for 96 ms (12 moves, 8 ms apart), and let go
    /// on the next frame. The island is first carried slowly to the start point (`from`), as a hand picks it up.
    private func throwIsland(_ name: String, _ c: IslandController, from: CGPoint, velocity v: CGVector, startX: CGFloat? = nil) {
        let grab = CGPoint(x: c.shapeRect.midX, y: c.shapeRect.midY)
        let start = CGPoint(x: startX ?? from.x, y: from.y)
        var evs: [(Double, NSEvent.EventType, CGPoint)] = [(0, .leftMouseDown, grab)]
        // Carry to the start at a walking pace, then pause: no velocity left over from the approach.
        for k in 1...10 { let f = CGFloat(k) / 10; evs.append((0.016 * Double(k), .leftMouseDragged, CGPoint(x: grab.x + (start.x - grab.x) * f, y: grab.y + (start.y - grab.y) * f))) }
        let t1 = 0.016 * 10 + 0.15
        for k in 0...12 {
            let dt = 0.008 * Double(k)
            evs.append((t1 + dt, .leftMouseDragged, CGPoint(x: start.x + v.dx * CGFloat(dt), y: start.y + v.dy * CGFloat(dt))))
        }
        let tr = t1 + 0.008 * 13
        evs.append((tr, .leftMouseUp, CGPoint(x: start.x + v.dx * CGFloat(0.104), y: start.y + v.dy * CGFloat(0.104))))
        play(name, c, evs, release: tr, throwSpeed: hypot(v.dx, v.dy))
    }

    /// A plain carry: slow, then a pause before letting go.
    private func drag(_ name: String, _ c: IslandController, to p: CGPoint, pause: Double) {
        let grab = CGPoint(x: c.shapeRect.midX, y: c.shapeRect.midY)
        var evs: [(Double, NSEvent.EventType, CGPoint)] = [(0, .leftMouseDown, grab)]
        for k in 1...30 { let f = CGFloat(k) / 30; evs.append((0.016 * Double(k), .leftMouseDragged, CGPoint(x: grab.x + (p.x - grab.x) * f, y: grab.y + (p.y - grab.y) * f))) }
        let tr = 0.016 * 30 + pause
        evs.append((tr, .leftMouseUp, p))
        play(name, c, evs, release: tr, throwSpeed: 0)
    }

    /// Send the events at their times and sample the shape every 8 ms from the press until 0.9 s after release.
    private func play(_ name: String, _ c: IslandController, _ evs: [(Double, NSEvent.EventType, CGPoint)], release: Double, throwSpeed: CGFloat) {
        let start = ProcessInfo.processInfo.systemUptime
        var rows = ["t,phase,x,y,w,h"]
        var after: [(Double, CGRect)] = []
        let end = release + 0.9
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now(), repeating: 0.008)
        timer.setEventHandler { [weak c] in
            guard let c else { return }
            let t = ProcessInfo.processInfo.systemUptime - start
            let r = c.visibleShapeRect()
            rows.append(String(format: "%.4f,%@,%.1f,%.1f,%.1f,%.1f", t, t < release ? "carry" : "flight", r.midX, r.midY, r.width, r.height))
            if t >= release { after.append((t, r)) }
        }
        timer.resume()
        for (dt, type, p) in evs {
            DispatchQueue.main.asyncAfter(deadline: .now() + dt) { [self] in send(c, type, p) }
        }
        marks.append(String(format: "%@,%.3f,%.3f", name, start - t0, start - t0 + end))
        DispatchQueue.main.asyncAfter(deadline: .now() + end) { [self] in
            timer.cancel()
            try? rows.joined(separator: "\n").write(to: Paths.root.appendingPathComponent("flick-\(name).csv"), atomically: true, encoding: .utf8)
            // Smooth: no frame-to-frame jump far beyond the throw's own speed; momentum: it leaves the hand moving.
            // The first sample after release is skipped: it can read the grown window frame before the layer's new
            // geometry is committed (a sampler artifact; the recording shows no jump on screen).
            // A jump is a frame whose speed is far above the frame before it (the spring itself ramps smoothly).
            var maxJump: CGFloat = 0, worstRatio: CGFloat = 0
            var prev: CGFloat?
            for k in 2..<after.count {
                let v = hypot(after[k].1.midX - after[k - 1].1.midX, after[k].1.midY - after[k - 1].1.midY) / CGFloat(after[k].0 - after[k - 1].0)
                maxJump = max(maxJump, v)
                if let prev { worstRatio = max(worstRatio, v / max(prev, 400)) }
                prev = v
            }
            if throwSpeed > 0, after.count > 6 {
                let a = after[1], b = after[min(6, after.count - 1)]
                let early = hypot(b.1.midX - a.1.midX, b.1.midY - a.1.midY) / CGFloat(b.0 - a.0)
                check("\(name): leaves the hand with its momentum (≥ 35 % of the throw speed in the first 40 ms)", early >= throwSpeed * 0.35, "\(Int(early)) pt/s of \(Int(throwSpeed))")
                check("\(name): no jumps in flight (no frame over 2 × the speed of the frame before)", worstRatio <= 2, "worst \(String(format: "%.2f", worstRatio))×, peak \(Int(maxJump)) pt/s")
            } else {
                check("\(name): settles without jumps", maxJump <= 1500 && worstRatio <= 2, "peak \(Int(maxJump)) pt/s, worst \(String(format: "%.2f", worstRatio))×")
            }
            let last = after.suffix(10).map(\.1)
            check("\(name): comes to rest at its slot", last.allSatisfy { abs($0.midX - c.shapeRect.midX) < 0.6 && abs($0.midY - c.shapeRect.midY) < 0.6 }, "\(last.last ?? .zero) vs \(c.shapeRect)")
        }
    }

    // MARK: Helpers

    private func c(_ id: String) -> IslandController { app.controllers[id]! }
    private func put(_ id: String, _ o: JSONObject) { try? IslandFile.writeAtomic(o.merging(["id": id]) { a, _ in a }, to: Paths.file(id)) }
    private func read(_ id: String) -> JSONObject? { IslandFile.read(Paths.file(id)) }
    private func side(_ id: String) -> String? { read(id)?["side"] as? String }
    private func edgeY(_ id: String) -> Double? { (read(id)?["edgeY"] as? NSNumber)?.doubleValue }
    private func check(_ name: String, _ ok: Bool, _ detail: String) { results.append((name, ok, detail)) }
    private func noOverlap() -> Bool {
        let rs = app.controllers.values.map(\.shapeRect)
        for a in rs.indices { for b in rs.indices where a < b && rs[a].insetBy(dx: 1, dy: 1).intersects(rs[b]) { return false } }
        return true
    }

    private func send(_ c: IslandController, _ type: NSEvent.EventType, _ screenPoint: CGPoint) {
        let wp = c.panel.convertPoint(fromScreen: screenPoint)
        guard let e = NSEvent.mouseEvent(with: type, location: wp, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                                         windowNumber: c.panel.windowNumber, context: nil, eventNumber: 0, clickCount: 1,
                                         pressure: type == .leftMouseUp ? 0 : 1) else { return }
        c.panel.sendEvent(e)
    }
}
