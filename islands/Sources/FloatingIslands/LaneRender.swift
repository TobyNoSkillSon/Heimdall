// Lab: the three island fixes of 22 Oct afternoon, driven through the real event path offscreen and rendered.
//   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --render-lane <out dir>   (lab/lane_islands.sh runs it)
// 1. A 40-item Scheduled list is capped under the display's height and scrolls (trackpad gesture with momentum,
//    rubber band at the end, a wheel step). 2. Answers ask "Sure?" first: a proposal, a button, a viewer action;
//    confirm by a second click, cancel, or let it lapse. 3. A new island with no `origin` drops out of the notch; one
//    already there at launch does not. Writes PNGs, frame folders for films, and checks.txt.
import AppKit
import ImageIO
import IslandModel

private let longDetail = "Three caches hold 16.4 GB of weights no project has loaded since September; every one of them can be downloaded again, so deleting them loses nothing."

final class LaneRender {
    let app: AppController
    let render: Render
    var trace: [CGFloat] = []
    var checks: [(String, Bool, String)] = []

    init(_ app: AppController, out: String) {
        self.app = app
        render = Render(app, out: out)
    }

    var screen: CGRect { NSScreen.main!.frame }

    func run() {
        try? FileManager.default.createDirectory(at: render.out, withIntermediateDirectories: true)
        // 3. Notch: `launch` was in the folder before the app started (the script wrote it).
        at(0.8) { [self] in
            let l = app.controllers["launch"]
            check("an island there at launch appears in place (no notch flight)", l != nil && l?.bornFrom == nil, "\(String(describing: l?.bornFrom))")
            put("job-new", ["title": "Port voices", "state": "running", "stage": "Starting", "icon": "waveform", "side": "right"])
            let n = IslandController.notchRect(NSScreen.main)
            let r = CGRect(x: n.minX - 40, y: screen.maxY - 260, width: screen.maxX - n.minX + 40, height: 260)
            render.sheet("notch-flight", rect: r, times: [0.02, 0.05, 0.08, 0.12, 0.16, 0.22, 0.3, 0.45, 0.8], cols: 3)
            film("film-notch", rect: r, duration: 0.9)
        }
        at(1.6) { [self] in
            let c = app.controllers["job-new"]
            check("a new island with no origin key drops out of the notch", c?.bornFrom != nil && c?.bornFrom == IslandController.notchRect(NSScreen.main), "\(String(describing: c?.bornFrom))")
            check("it lands in the right column", (c?.shapeRect.maxX ?? 0) > screen.maxX - 20, "\(c?.shapeRect ?? .zero)")
            check("the file never gains an origin key", read("job-new")?["origin"] == nil, "")
            render.shot("notch-landed", rect: CGRect(x: screen.midX - 200, y: screen.maxY - 260, width: screen.width / 2 + 200, height: 260))
        }

        // 1. A list taller than the display.
        at(0.6) { [self] in
            let whens = ["Tonight", "Tomorrow", "Fri 10:00", "Sat", "Next week"]
            let titles = ["Arsenal: check the site", "Heimdall: menu revamp", "Phonon voices as presets", "Review the night pass", "iPad sale: photos", "Vella: TestFlight build"]
            let items: [JSONObject] = (1...40).map { k in ["id": "s\(k)", "title": "\(k). " + titles[k % 6], "when": whens[k % 5], "detail": "One quiet line about what happens then."] }
            put("scheduled", ["preset": "todo", "list": "scheduled", "expanded": true, "items": items])
        }
        at(1.2) { [self] in
            guard let c = app.controllers["scheduled"], let s = NSScreen.main else { check("scheduled shown", false, "\(app.controllers.keys.sorted()) \(String(describing: read("scheduled")?.keys.sorted()))"); return }
            let room = s.visibleFrame.height
            check("a 40-item list is capped under the visible screen height", c.shapeRect.height < room - 20 && c.shapeRect.height > room - 120,
                  "island \(Int(c.shapeRect.height)) pt, visible screen \(Int(room)) pt, display \(Int(s.frame.height)) pt")
            check("it stays on the display", s.visibleFrame.insetBy(dx: -1, dy: -1).contains(c.shapeRect), "\(c.shapeRect) in \(s.visibleFrame)")
            check("it scrolls", c.scrolls, "")
            check("all 40 rows are there (none dropped)", c.rowOrder().count == 40, "\(c.rowOrder().count) rows")
            render.shot("scroll-layout-fullscreen", rect: screen)
            render.shot("scroll-top", ids: ["scheduled"])
            // Longer still for the scrolling itself (the agent rewrites the open list: it stays open and capped).
            let more: [JSONObject] = (41...60).map { k in ["id": "s\(k)", "title": "\(k). Later item", "when": "Next week"] }
            _ = IslandFile.patch("scheduled") { $0["items"] = ($0["items"] as? [JSONObject] ?? []) + more }
        }
        at(0.6) { [self] in
            guard let c = app.controllers["scheduled"] else { return }
            check("rewritten to 60 items it keeps its height", c.shapeRect.height == (NSScreen.main.map { app.heightCap($0) } ?? 0) && c.rowOrder().count == 60, "\(c.shapeRect.height) \(c.rowOrder().count)")
            // A trackpad flick down: fingers for 10 frames, then the system's momentum (decaying), as macOS sends it.
            var k = 0.0
            func ev(_ dy: Double, _ phase: Int, _ momentum: Int) {
                let at = k; k += 1.0 / 60
                DispatchQueue.main.asyncAfter(deadline: .now() + at) { [self] in scroll(c, dy, phase: phase, momentum: momentum) }
            }
            ev(0, 1, 0)                                         // began
            for _ in 0..<10 { ev(-34, 2, 0) }                   // changed
            ev(0, 4, 0)                                         // ended
            var v = -40.0
            ev(v, 0, 1)                                         // momentum began
            for _ in 0..<26 { v *= 0.9; ev(v, 0, 2) }
            ev(0, 0, 3)                                         // momentum ended
            render.sheet("scroll-flick", ids: ["scheduled"], times: [0, 0.1, 0.2, 0.3, 0.45, 0.6, 0.8], cols: 7)
            film("film-scroll", rect: around(["scheduled"]), duration: 1.0)
        }
        at(1.4) { [self] in
            guard let c = app.controllers["scheduled"] else { return }
            check("the flick scrolled with momentum past the fingers' travel", c.scrollY > 340 + 100, "scrollY \(Int(c.scrollY))")
            check("the indicator has faded at rest", c.animatedLayers().isEmpty, "\(c.animatedLayers())")
            render.shot("scroll-middle", ids: ["scheduled"])
            // Rubber band: from near the bottom, the fingers pull past the end, let go, it springs back.
            c.setScroll(maxScroll(c) - 30)
            var k = 0.0
            func ev(_ dy: Double, _ phase: Int) {
                let at = k; k += 1.0 / 60
                DispatchQueue.main.asyncAfter(deadline: .now() + at) { [self] in scroll(c, dy, phase: phase, momentum: 0) }
            }
            ev(0, 1)
            for _ in 0..<12 { ev(-26, 2) }
            ev(0, 4)
            render.sheet("scroll-rubber-band", ids: ["scheduled"], times: [0.1, 0.2, 0.26, 0.32, 0.4, 0.55, 0.9], cols: 7)
        }
        at(0.25) { [self] in
            guard let c = app.controllers["scheduled"] else { return }
            check("pulled past the end it gives way", c.scrollY > maxScroll(c) + 5, "scrollY \(Int(c.scrollY)) max \(Int(maxScroll(c)))")
        }
        at(1.3) { [self] in
            guard let c = app.controllers["scheduled"] else { return }
            check("let go, it springs back to the end", abs(c.scrollY - maxScroll(c)) < 0.5 && c.animatedLayers().isEmpty, "scrollY \(Int(c.scrollY)) max \(Int(maxScroll(c))) \(c.animatedLayers())")
            render.shot("scroll-bottom", ids: ["scheduled"])
            // A hard flick from the top into the end: momentum stops there once, no shaking at the edge.
            c.setScroll(0)
            trace = []
            var k = 0.0
            func ev(_ dy: Double, _ phase: Int, _ momentum: Int) {
                let at = k; k += 1.0 / 60
                DispatchQueue.main.asyncAfter(deadline: .now() + at) { [self] in scroll(c, dy, phase: phase, momentum: momentum); trace.append(c.scrollY) }
            }
            ev(0, 1, 0)
            for _ in 0..<10 { ev(-60, 2, 0) }
            ev(0, 4, 0)
            var v = -90.0
            ev(v, 0, 1)
            for _ in 0..<40 { v *= 0.95; ev(v, 0, 2) }
            ev(0, 0, 3)
            render.sheet("scroll-flick-into-end", ids: ["scheduled"], times: [0.3, 0.45, 0.55, 0.65, 0.8, 1.0, 1.3], cols: 7)
        }
        at(1.6) { [self] in
            guard let c = app.controllers["scheduled"] else { return }
            let m = maxScroll(c)
            let flips = zip(trace, trace.dropFirst()).filter { ($0.0 > m + 0.5) != ($0.1 > m + 0.5) }.count
            check("a flick into the end overshoots once and settles (no shaking)", flips <= 2 && abs(c.scrollY - m) < 0.5 && c.animatedLayers().isEmpty, "\(flips) edge crossings, scrollY \(Int(c.scrollY)) of \(Int(m))")
            // A mouse wheel: three notches up, eased steps.
            for i in 0..<3 { DispatchQueue.main.asyncAfter(deadline: .now() + Double(i) * 0.08) { [self] in wheel(c, 3) } }
        }
        at(0.8) { [self] in
            guard let c = app.controllers["scheduled"] else { return }
            check("a wheel scrolls up in eased steps", c.scrollY < maxScroll(c) - 100 && c.scrollY > 0 && c.animatedLayers().isEmpty, "scrollY \(Int(c.scrollY)) of \(Int(maxScroll(c)))")
            // Closed and reopened: back at the top, short island lists do not scroll.
            _ = IslandFile.patch("scheduled") { $0["expanded"] = false }
        }
        at(0.8) { [self] in
            _ = IslandFile.patch("scheduled") { $0["expanded"] = true }
        }
        at(0.9) { [self] in
            guard let c = app.controllers["scheduled"] else { return }
            check("reopened, it starts at the top", c.scrollY == 0, "scrollY \(c.scrollY)")
            try? FileManager.default.removeItem(at: Paths.file("scheduled"))
        }

        // 2. Confirm before an answer goes.
        at(0.8) { [self] in
            put("todo", ["preset": "todo", "list": "todo", "expanded": true, "side": "left", "items": [
                ["id": "arsenal", "title": "Arsenal: push branch", "group": "running"],
                ["id": "caches", "title": "Delete old caches", "group": "waiting", "detail": longDetail,
                 "proposal": ["text": "~/.cache/huggingface  12.4 GB\n~/Library/Caches/pip  3.1 GB"]],
                ["id": "echo", "title": "Echo: Heimdall hears itself", "group": "discussed"]]])
        }
        at(0.8) { [self] in
            guard let td = app.controllers["todo"] else { check("todo shown", false, ""); return }
            // Closed, a row is its title only; opened, the whole explanation follows (titles-only rows, 23 Oct).
            let t = td.bodyTexts()
            let missing = Plan.symbols.map(\.symbol).filter { NSImage(systemSymbolName: $0, accessibilityDescription: nil) == nil }
            check("every plan row symbol is a real SF Symbol", missing.isEmpty, "\(missing)")
            check("a closed row shows its title only", t.contains("Delete old caches") && !t.contains { $0.contains("Three caches") }, "\(t)")
            click(td) { if case let .disclose(key) = $0 { return key == "caches" } else { return false } }
        }
        at(0.9) { [self] in
            guard let td = app.controllers["todo"] else { return }
            check("an opened row shows its whole explanation", td.bodyTexts().contains(longDetail), "\(td.bodyTexts())")
            render.shot("confirm-proposal-before", ids: ["todo"])
            let yes = td.controlCentre { if case let .approve(key, action) = $0 { return key == "caches" && action == "yes" } else { return false } }
            click(td) { if case let .approve(key, action) = $0 { return key == "caches" && action == "yes" } else { return false } }
            check("a proposal's Yes asks first: nothing sent", td.isConfirming && responses().isEmpty, "\(responses())")
            render.sheet("confirm-proposal-in", ids: ["todo"], times: [0, 0.04, 0.08, 0.12, 0.2], cols: 5)
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { [self] in
                render.shot("confirm-proposal-sure", ids: ["todo"])
                // A second click on the same spot within the window sends it.
                if let yes { send(td, .leftMouseDown, yes); send(td, .leftMouseUp, yes) }
                let r = responses().first { $0["kind"] as? String == "approval" }
                check("a second click on the same button sends it", !td.isConfirming && r?["action"] as? String == "yes" && r?["value"] as? String == "caches", "\(r ?? [:])")
            }
        }
        at(1.0) { [self] in
            render.shot("confirm-proposal-sent", ids: ["todo"])
            clearResponses()
            try? FileManager.default.removeItem(at: Paths.file("todo"))
            put("ask", ["preset": "waiting", "title": "Website concept", "state": "waiting", "expanded": true, "summary": "Which demo goes on the Arsenal site?",
                        "parts": [["type": "button", "label": "Photo to 3D", "action": "concept-3d"], ["type": "button", "label": "Conversation", "action": "concept-talk"],
                                  ["type": "button", "label": "Later", "action": "later"]]])
        }
        at(0.9) { [self] in
            guard let a = app.controllers["ask"] else { check("ask shown", false, ""); return }
            render.shot("confirm-button-before", ids: ["ask"])
            click(a) { if case let .button(id, _) = $0 { return id == "concept-3d" } else { return false } }
            check("a button asks first: nothing sent", a.isConfirming && responses().isEmpty, "")
            // A double-click (the second click 0 s later, on what is now Confirm) is not a decision.
            click(a) { if case let .button(id, _) = $0 { return id == "concept-3d" } else { return false } }
            check("a double-click does not confirm", a.isConfirming && responses().isEmpty, "")
        }
        at(0.4) { [self] in
            guard let a = app.controllers["ask"], let p = a.labConfirmCentre(cancel: true) else { check("cancel found", false, ""); return }
            render.shot("confirm-button-sure", ids: ["ask"])
            send(a, .leftMouseDown, p); send(a, .leftMouseUp, p)
            check("Cancel puts the buttons back and sends nothing", !a.isConfirming && responses().isEmpty, "")
        }
        at(0.4) { [self] in
            guard let a = app.controllers["ask"] else { return }
            render.shot("confirm-button-cancelled", ids: ["ask"])
            click(a) { if case let .button(id, _) = $0 { return id == "concept-talk" } else { return false } }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [self] in render.shot("confirm-button-middle", ids: ["ask"]) }
        }
        at(Confirm.window + 0.4) { [self] in
            guard let a = app.controllers["ask"] else { return }
            check("left alone, it lapses after \(Int(Confirm.window)) s and sends nothing", !a.isConfirming && responses().isEmpty, "")
            click(a) { if case let .button(id, _) = $0 { return id == "later" } else { return false } }
            let r = responses().first { $0["kind"] as? String == "button" }
            check("Later goes in one click", !a.isConfirming && r?["action"] as? String == "later", "\(r ?? [:])")
            clearResponses()
            try? FileManager.default.removeItem(at: Paths.file("ask"))
            // A viewer with proposed actions (the 16:17 concepts).
            let md = Paths.root.appendingPathComponent("concepts.md")
            try? "# Arsenal concepts\n\n1. **Photo to 3D**: one photo becomes a model you can turn.\n2. **Conversation**: a real recording, speakers and words in sync.\n3. **Receipt**: a photo of a receipt becomes a table.\n".write(to: md, atomically: true, encoding: .utf8)
            put("view-concepts", ["preset": "viewer", "title": "Arsenal concepts", "path": md.path, "expanded": true,
                                  "actions": [["label": "Photo to 3D", "action": "concept-3d"], ["label": "Conversation", "action": "concept-talk"], ["label": "Receipt", "action": "concept-receipt"]]])
        }
        at(1.6) { [self] in
            guard let v = app.controllers["view-concepts"], let row = v.panel.contentView?.subviews.first(where: { $0 is ViewerRootView && !$0.isHidden })?.subviews.first(where: { $0 is ActionRow }) as? ActionRow
            else { check("viewer actions shown", false, ""); return }
            viewerShot("confirm-viewer-before", v)
            let p = row.convert(CGPoint(x: row.bounds.midX, y: row.bounds.midY), to: nil)
            send(v, .leftMouseDown, p); send(v, .leftMouseUp, p)
            check("a viewer action asks first: nothing sent", row.isConfirming && responses().isEmpty, "\(responses())")
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [self] in
                viewerShot("confirm-viewer-sure", v)
                send(v, .leftMouseDown, p); send(v, .leftMouseUp, p)
                let r = responses().first { $0["kind"] as? String == "button" }
                check("a second click on it sends it", !row.isConfirming && r?["action"] as? String == "concept-talk", "\(r ?? [:])")
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [self] in viewerShot("confirm-viewer-sent", v) }
            }
        }
        at(1.6) { [self] in
            let lines = checks.map { "\($0.1 ? "ok  " : "FAIL") \($0.0)\($0.2.isEmpty ? "" : "  (\($0.2))")" }
            try? (lines.joined(separator: "\n") + "\n").write(to: render.out.appendingPathComponent("checks.txt"), atomically: true, encoding: .utf8)
            print(lines.joined(separator: "\n"))
            for l in render.log { print(l) }
            for d in [Paths.islands, Paths.responses] {
                for f in (try? FileManager.default.contentsOfDirectory(at: d, includingPropertiesForKeys: nil)) ?? [] { try? FileManager.default.removeItem(at: f) }
            }
            exit(checks.allSatisfy(\.1) ? 0 : 1)
        }
    }

    // MARK: Helpers

    /// Steps run in order, each `dt` after the previous one ran (a busy main thread delays the rest, never reorders).
    private var steps: [(Double, () -> Void)] = []
    private func at(_ dt: Double, _ f: @escaping () -> Void) {
        steps.append((dt, f))
        if steps.count == 1 { DispatchQueue.main.async { [self] in next() } }
    }
    private func next() {
        guard !steps.isEmpty else { return }
        let (dt, f) = steps.removeFirst()
        DispatchQueue.main.asyncAfter(deadline: .now() + dt) { [self] in f(); next() }
    }
    private func check(_ name: String, _ ok: Bool, _ detail: String) { checks.append((name, ok, detail)) }
    private func put(_ id: String, _ o: JSONObject) { try? IslandFile.writeAtomic(o.merging(["id": id]) { a, _ in a }, to: Paths.file(id)) }
    private func read(_ id: String) -> JSONObject? { IslandFile.read(Paths.file(id)) }
    private func responses() -> [JSONObject] {
        ((try? FileManager.default.contentsOfDirectory(at: Paths.responses, includingPropertiesForKeys: nil)) ?? []).compactMap(IslandFile.read)
    }
    private func clearResponses() {
        for f in (try? FileManager.default.contentsOfDirectory(at: Paths.responses, includingPropertiesForKeys: nil)) ?? [] { try? FileManager.default.removeItem(at: f) }
    }
    private func maxScroll(_ c: IslandController) -> CGFloat { c.labMaxScroll }

    private func click(_ c: IslandController, _ match: (Control) -> Bool) {
        guard let p = c.controlCentre(match) else { check("control found on \(c.id)", false, ""); return }
        send(c, .leftMouseDown, p)
        send(c, .leftMouseUp, p)
    }

    /// A click as the window would route it: to the view under the point (the island, or a viewer's action row).
    /// Offscreen windows are never ordered in, so the hit test is done here instead of by the window server.
    private func send(_ c: IslandController, _ type: NSEvent.EventType, _ windowPoint: CGPoint) {
        guard let e = NSEvent.mouseEvent(with: type, location: windowPoint, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                                         windowNumber: c.panel.windowNumber, context: nil, eventNumber: 0, clickCount: 1,
                                         pressure: type == .leftMouseUp ? 0 : 1),
              let content = c.panel.contentView, let frame = content.superview,
              let target = content.hitTest(frame.convert(windowPoint, from: nil)) else { return }
        if type == .leftMouseDown { target.mouseDown(with: e) } else { target.mouseUp(with: e) }
    }

    /// A trackpad scroll event over the island's middle, as the system sends it (pixel deltas, phases).
    /// phase: 1 began, 2 changed, 4 ended (CGScrollPhase); momentum: 1 begin, 2 continue, 3 end (CGMomentumScrollPhase).
    private func scroll(_ c: IslandController, _ dy: Double, phase: Int, momentum: Int) {
        guard let e = CGEvent(scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 1, wheel1: Int32(dy), wheel2: 0, wheel3: 0) else { return }
        e.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
        e.setDoubleValueField(.scrollWheelEventPointDeltaAxis1, value: dy)
        e.setIntegerValueField(.scrollWheelEventScrollPhase, value: Int64(phase))
        e.setIntegerValueField(.scrollWheelEventMomentumPhase, value: Int64(momentum))
        deliver(c, e)
    }

    /// A mouse wheel: `lines` notches (no phases).
    private func wheel(_ c: IslandController, _ lines: Int32) {
        guard let e = CGEvent(scrollWheelEvent2Source: nil, units: .line, wheelCount: 1, wheel1: lines, wheel2: 0, wheel3: 0) else { return }
        deliver(c, e)
    }

    private func deliver(_ c: IslandController, _ e: CGEvent) {
        let r = c.shapeRect
        let h = NSScreen.screens.first!.frame.height
        e.location = CGPoint(x: r.midX, y: h - r.midY)
        guard let ns = NSEvent(cgEvent: e) else { return }
        c.view.scrollWheel(with: ns)
    }

    /// A viewer island as it looks: the shape from the layer tree, its live view (action row, text) drawn on top.
    private func viewerShot(_ name: String, _ c: IslandController) {
        let rect = c.shapeRect.insetBy(dx: -20, dy: -20).integral
        let s: CGFloat = 2
        guard let ctx = CGContext(data: nil, width: Int(rect.width * s), height: Int(rect.height * s), bitsPerComponent: 8, bytesPerRow: 0,
                                  space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return }
        ctx.scaleBy(x: s, y: s)
        ctx.setFillColor(Look.rgb(0x22252B).cgColor)
        ctx.fill(CGRect(origin: .zero, size: rect.size))
        let f = c.panel.frame
        ctx.saveGState()
        ctx.translateBy(x: f.minX - rect.minX, y: f.maxY - rect.minY)
        ctx.scaleBy(x: 1, y: -1)
        (c.view.layer!.presentation() ?? c.view.layer!).render(in: ctx)
        if let live = c.panel.contentView?.subviews.first(where: { $0 is ViewerRootView && !$0.isHidden }), let l = live.layer {
            ctx.saveGState()
            ctx.translateBy(x: live.frame.minX, y: live.frame.minY)
            (l.presentation() ?? l).render(in: ctx)
            ctx.restoreGState()
        }
        ctx.restoreGState()
        guard let img = ctx.makeImage() else { return }
        write(img, render.out.appendingPathComponent(name + ".png"))
    }

    private func write(_ img: CGImage, _ url: URL) {
        guard let d = CGImageDestinationCreateWithURL(url as CFURL, "public.png" as CFString, 1, nil) else { return }
        CGImageDestinationAddImage(d, img, [kCGImagePropertyDPIWidth: 144, kCGImagePropertyDPIHeight: 144] as CFDictionary)
        CGImageDestinationFinalize(d)
    }

    private func around(_ ids: [String], pad: CGFloat = 20) -> CGRect {
        ids.compactMap { app.controllers[$0] }.map { $0.panel.frame.union($0.visibleShapeRect()) }.reduce(CGRect.null) { $0.union($1) }
            .insetBy(dx: -pad, dy: -pad).integral
    }

    /// The islands' current presentation in `rect` (screen points, y up) on a dark desktop, as Render draws them.
    private func image(_ rect: CGRect) -> CGImage? {
        let s: CGFloat = 2
        let w = Int(rect.width * s), h = Int(rect.height * s)
        guard let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.scaleBy(x: s, y: s)
        let g = CGGradient(colorsSpace: nil, colors: [Look.rgb(0x2B2E35).cgColor, Look.rgb(0x1A1C21).cgColor] as CFArray, locations: [0, 1])!
        ctx.drawLinearGradient(g, start: CGPoint(x: 0, y: rect.height), end: .zero, options: [])
        for c in app.controllers.values.sorted(by: { $0.panel.level.rawValue < $1.panel.level.rawValue }) {
            let f = c.panel.frame
            guard f.intersects(rect) else { continue }
            ctx.saveGState()
            ctx.translateBy(x: f.minX - rect.minX, y: f.maxY - rect.minY)
            ctx.scaleBy(x: 1, y: -1)
            let root = c.view.layer!
            (root.presentation() ?? root).render(in: ctx)
            ctx.restoreGState()
        }
        return ctx.makeImage()
    }

    /// Frames at 60 fps for `duration` seconds into `<name>/NNN.png` (the script turns them into an MP4).
    private func film(_ name: String, rect: CGRect, duration: Double) {
        let dir = render.out.appendingPathComponent(name, isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let n = Int(duration * 60)
        for k in 0..<n {
            DispatchQueue.main.asyncAfter(deadline: .now() + Double(k) / 60) { [self] in
                if let img = image(rect) { write(img, dir.appendingPathComponent(String(format: "%03d.png", k))) }
            }
        }
    }
}
