// Lab self-test: real islands, real windows, mouse events sent through each window's own event path
// (NSWindow.sendEvent → hit test → view). The pointer, keyboard and screen are never driven.
//   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --selftest
import AppKit
import AVFoundation
import IslandModel

final class SelfTest {
    let app: AppController
    var results: [(String, Bool, String)] = []
    init(_ app: AppController) { self.app = app }

    func run() {
        guard ProcessInfo.processInfo.environment["FLOATING_ISLANDS_DIR"] != nil else {
            print("selftest: refusing to run without FLOATING_ISLANDS_DIR (it rewrites the data folder)")
            exit(2)
        }
        let fm = FileManager.default
        setenv("FI_OPEN_LOG", Paths.root.appendingPathComponent("opened.log").path, 1)
        try? fm.removeItem(at: Paths.root.appendingPathComponent("opened.log"))
        for d in [Paths.islands, Paths.responses] {
            for f in (try? fm.contentsOfDirectory(at: d, includingPropertiesForKeys: nil)) ?? [] { try? fm.removeItem(at: f) }
        }
        put("st-wait", ["preset": "waiting", "title": "Publish?", "expanded": true, "position": ["x": 354, "y": 180],
                        "parts": [["type": "text", "value": "Publish the preview build?"],
                                  ["type": "button", "label": "Publish", "action": "publish"],
                                  ["type": "button", "label": "Not now", "action": "later"]]])
        put("st-list", ["preset": "list", "title": "Quotes", "expanded": true, "position": ["x": 684, "y": 180],
                        "parts": [["type": "list", "items": [["id": "a", "title": "Quote A", "value": "4 199"],
                                                             ["id": "b", "title": "Quote B", "value": "3 870"],
                                                             ["id": "c", "title": "Quote C", "value": "4 420"]]]]])
        put("st-note", ["preset": "note", "title": "Settings", "expanded": true, "position": ["x": 1014, "y": 180],
                        "parts": [["type": "toggle", "id": "voice", "label": "Voice", "on": false],
                                  ["type": "slider", "id": "ctx", "min": 0, "max": 100, "step": 1, "value": 20, "unit": "k"]]])
        put("st-drag", ["preset": "task", "title": "Build", "progress": 0.4])

        after(1.2) { [self] in
            check("four islands on screen", app.controllers.count == 4 && app.controllers.values.allSatisfy { $0.panel.isVisible }, "\(app.controllers.count)")
            let w = app.controllers["st-wait"]!
            check("pinned island opens", w.expanded, "")
            click(w) { if case let .button(id, _) = $0 { return id == "publish" } else { return false } }
            w.labConfirm(true)   // answers ask "Sure?" first (lab/lane_islands.sh drives the real second click)
            let l = app.controllers["st-list"]!
            click(l) { if case let .listRow(id, _) = $0 { return id == "b" } else { return false } }
            l.labConfirm(true)
            let n = app.controllers["st-note"]!
            click(n) { if case .toggle = $0 { return true } else { return false } }
            if let p = n.controlCentre({ if case .slider = $0 { return true } else { return false } }) {
                let left = CGPoint(x: p.x - 40, y: p.y)
                send(n, .leftMouseDown, left)
                send(n, .leftMouseDragged, CGPoint(x: p.x + 20, y: p.y))
                send(n, .leftMouseUp, CGPoint(x: p.x + 60, y: p.y))
            }
        }
        after(1.6) { [self] in
            let rs = responses()
            check("button writes its response", rs.contains { $0["island"] as? String == "st-wait" && $0["action"] as? String == "publish" }, "")
            check("button choice kept in the file", read("st-wait")?["chosen"] as? String == "publish", "")
            check("list row writes its response", rs.contains { $0["island"] as? String == "st-list" && $0["action"] as? String == "b" }, "")
            let listSel = ((read("st-list")?["parts"] as? [JSONObject])?.first?["selected"] as? String)
            check("list selection kept in the file", listSel == "b", listSel ?? "nil")
            check("toggle writes its response", rs.contains { $0["island"] as? String == "st-note" && $0["kind"] as? String == "toggle" && $0["value"] as? Bool == true }, "")
            let sv = rs.first { $0["kind"] as? String == "slider" }?["value"] as? Double
            check("slider writes its value", sv != nil && sv! > 20, "\(sv ?? -1)")

            // Drag the task island to just right of the middle and let go: it attaches to the right edge at that height.
            let d = app.controllers["st-drag"]!
            let scr = NSScreen.main!.frame
            let startScreen = CGPoint(x: d.shapeRect.midX, y: d.shapeRect.midY)
            let drop = CGPoint(x: scr.midX + 137, y: scr.midY - 61)
            sendScreen(d, .leftMouseDown, startScreen)
            for k in 1...5 {
                let f = CGFloat(k) / 5
                sendScreen(d, .leftMouseDragged, CGPoint(x: startScreen.x + (drop.x - startScreen.x) * f, y: startScreen.y + (drop.y - startScreen.y) * f))
            }
            sendScreen(d, .leftMouseUp, drop)
            let ey = (read("st-drag")?["edgeY"] as? NSNumber)?.doubleValue ?? .nan
            check("drop attaches to the nearer edge at the height it was let go", abs(d.shapeRect.maxX - (scr.maxX - 8)) < 1 && abs(d.shapeRect.midY - drop.y) < 1.5, "\(d.shapeRect)")
            check("drop stores the edge and the height", read("st-drag")?["side"] as? String == "right" && abs(ey - Double(scr.maxY - d.shapeRect.maxY)) < 0.5 && read("st-drag")?["position"] == nil, "\(read("st-drag") ?? [:])")

            // Drop it in the top right corner, partly over the menu bar: it stays on the right edge, just under the menu bar.
            let n = app.controllers["st-note"]!
            let ns = CGPoint(x: n.shapeRect.midX, y: n.shapeRect.maxY - 14)
            let corner = CGPoint(x: scr.maxX - n.shapeRect.width / 2 - 20, y: scr.maxY - 10)
            sendScreen(n, .leftMouseDown, ns)
            sendScreen(n, .leftMouseDragged, CGPoint(x: (ns.x + corner.x) / 2, y: (ns.y + corner.y) / 2))
            sendScreen(n, .leftMouseDragged, corner)
            sendScreen(n, .leftMouseUp, corner)
            let menuBottom = scr.maxY - max(NSScreen.main!.safeAreaInsets.top, scr.maxY - NSScreen.main!.visibleFrame.maxY)
            let r = n.shapeRect
            check("stops under the menu bar", r.maxY <= menuBottom - 5.5 && r.maxY >= menuBottom - 6.5, "top \(r.maxY) menu bottom \(menuBottom)")
            check("attaches to the right edge, not the middle", abs(r.maxX - (scr.maxX - 8)) < 1, "\(r)")
            check("the edge is stored", read("st-note")?["side"] as? String == "right" && read("st-note")?["edgeY"] != nil, "\(read("st-note") ?? [:])")
            check("an open island stays open through drag and drop", n.expanded && n.island.expanded, "")
            check("drop keeps its size", n.shapeRect.size == n.expandedSize, "\(n.shapeRect.size) vs \(n.expandedSize)")
        }
        after(2.3) { [self] in
            check("an answered question leaves", read("st-wait") == nil, "")
        }
        after(2.4) { [self] in
            let d = app.controllers["st-drag"]!
            let c = CGPoint(x: d.shapeRect.midX, y: d.shapeRect.maxY - 14)
            sendScreen(d, .leftMouseDown, c)
            sendScreen(d, .leftMouseUp, c)
            check("click pins the island open", read("st-drag")?["expanded"] as? Bool == true && d.expanded, "")
            put("st-drag", IslandFile.merge(read("st-drag")!, ["progress": 0.9]))
            try? FileManager.default.removeItem(at: Paths.file("st-list"))
        }
        after(3.0) { [self] in
            check("file update reaches the island", app.controllers["st-drag"]?.island.progress == 0.9, "")
            check("deleting the file removes the island", app.controllers["st-list"] == nil, "")
            put("st-drag", IslandFile.merge(read("st-drag")!, ["position": NSNull(), "edgeY": NSNull(), "side": NSNull()]))
        }
        after(3.3) { [self] in
            // Viewer: a markdown file opens large in the middle of the screen; Esc closes it to its pill.
            let md = Paths.root.appendingPathComponent("selftest-view.md")
            try? "# Viewer\n\nA **markdown** file with a list:\n\n- one\n- two\n\n```\ncode\n```\n".write(to: md, atomically: true, encoding: .utf8)
            put("st-view", ["preset": "viewer", "title": "Viewer", "path": md.path, "expanded": true,
                            "actions": [["label": "Do all of it", "action": "do-all"], ["label": "Not now", "action": "later"]]])
        }
        after(4.4) { [self] in
            let v = app.controllers["st-view"]
            let scr = NSScreen.main!.frame
            check("viewer opens in the middle of the screen", v.map { abs($0.shapeRect.midX - scr.midX) < 2 && $0.shapeRect.width > 400 } ?? false, "\(v?.shapeRect ?? .zero)")
            let root = v?.panel.contentView?.subviews.first { $0 is ViewerRootView && !$0.isHidden }
            check("viewer shows its text in a scroll view", root?.subviews.contains { $0 is NSScrollView } ?? false, "")
            // Proposed actions: a row on top; a click through the window writes a button response and shows the choice.
            if let v, let row = root?.subviews.first(where: { $0 is ActionRow }) {
                let p = row.convert(CGPoint(x: row.bounds.width * 0.25, y: row.bounds.midY), to: nil)
                send(v, .leftMouseDown, p)
                send(v, .leftMouseUp, p)
                check("viewer action asks before it sends", (row as? ActionRow)?.isConfirming == true && !responses().contains { $0["island"] as? String == "st-view" }, "")
                (row as? ActionRow)?.confirm(true)
                check("viewer action writes a button response", responses().contains { $0["island"] as? String == "st-view" && $0["kind"] as? String == "button" && $0["action"] as? String == "do-all" }, "")
            } else { check("viewer shows its proposed actions", false, "") }
            if let v {
                // Carried by its title row: it moves, stays open, keeps its size, and remembers the place.
                let before = v.shapeRect
                let grab = CGPoint(x: before.midX - 60, y: before.maxY - 18)
                sendScreen(v, .leftMouseDown, grab)
                for k in 1...4 { sendScreen(v, .leftMouseDragged, CGPoint(x: grab.x + CGFloat(k) * 30, y: grab.y - CGFloat(k) * 15)) }
                sendScreen(v, .leftMouseUp, CGPoint(x: grab.x + 120, y: grab.y - 60))
                let moved = v.shapeRect
                check("viewer carried by its title row", abs(moved.minX - before.minX - 120) < 2 && abs(moved.minY - before.minY + 60) < 2 && moved.size == before.size && v.expanded, "\(before) → \(moved)")
                check("viewer place is stored", read("st-view")?["openPosition"] != nil, "")
            }
            v?.panel.cancelOperation(nil)
            check("Esc closes the viewer to its pill", read("st-view")?["expanded"] as? Bool == false && v?.expanded == false, "")
        }
        after(5.2) { [self] in
            let v = app.controllers["st-view"]
            check("minimised viewer is a pill under the menu bar", v.map { $0.shapeRect.height == Look.pillH } ?? false, "\(v?.shapeRect ?? .zero)")
            // Open again, then its X closes it for good.
            if let o = IslandFile.patch("st-view", { $0["expanded"] = true }) { v?.update(Island(id: "st-view", raw: o)) }
        }
        after(5.9) { [self] in
            let v = app.controllers["st-view"]
            if let v, let x = v.closeCentre() {
                send(v, .leftMouseDown, x)
                send(v, .leftMouseUp, x)
            } else { check("close button shown on an open island", false, "") }
        }
        after(6.4) { [self] in
            check("X closes an island for good", read("st-view") == nil && app.controllers["st-view"] == nil, "")
            check("the agent hears about the close", responses().contains { $0["island"] as? String == "st-view" && $0["kind"] as? String == "closed" }, "")
            app.controllers["st-note"]?.menuClose()
        }
        after(6.9) { [self] in
            check("right-click Close removes an island", read("st-note") == nil, "")
            try? FileManager.default.removeItem(at: Paths.root.appendingPathComponent("selftest-view.md"))
        }
        after(7.3) { [self] in
            let d = app.controllers["st-drag"]!
            let scr = NSScreen.main!.frame
            let menuBottom = scr.maxY - max(NSScreen.main!.safeAreaInsets.top, scr.maxY - NSScreen.main!.visibleFrame.maxY)
            let colTop = scr.maxY - app.columnTop(NSScreen.main!)
            check("clearing the position returns it to the automatic column at the right edge, below browser chrome",
                  d.island.position == nil && abs(d.anchor.y - colTop) < 1 && abs(d.shapeRect.maxX - (scr.maxX - 8)) < 1, "\(d.anchor) \(d.shapeRect)")
            check("no island in the menu bar strip", app.controllers.values.allSatisfy { $0.shapeRect.maxY <= menuBottom }, "")
            // What an agent really sends: a title and a stage, nothing else; and a finished one with a page to open.
            let page = Paths.root.appendingPathComponent("compare3.html")
            try? "<h1>compare</h1>".write(to: page, atomically: true, encoding: .utf8)
            put("st-bare", ["preset": "task", "title": "Male voices", "state": "running", "stage": "Searching", "icon": "waveform",
                            "started": Date().timeIntervalSince1970 - 725, "position": ["x": 480, "y": 400]])
            put("st-done", ["preset": "task", "title": "Compare", "state": "done", "open": page.path, "position": ["x": 860, "y": 400]])
            put("st-gone", ["preset": "result", "title": "Old", "open": "/nonexistent/old.html", "expanded": true, "position": ["x": 1150, "y": 400]])
        }
        after(8.1) { [self] in
            let b = app.controllers["st-bare"]!
            let c = CGPoint(x: b.shapeRect.midX, y: b.shapeRect.midY)
            sendScreen(b, .leftMouseDown, c)
            sendScreen(b, .leftMouseUp, c)
            let dn = app.controllers["st-done"]!
            let c2 = CGPoint(x: dn.shapeRect.midX, y: dn.shapeRect.midY)
            sendScreen(dn, .leftMouseDown, c2)
            sendScreen(dn, .leftMouseUp, c2)
        }
        after(8.8) { [self] in
            let b = app.controllers["st-bare"]!
            check("a click opens a bare task (title and stage only)", b.expanded && b.shapeRect.height > Look.pillH + 20, "\(b.shapeRect)")
            let texts = b.bodyTexts()
            check("open bare task says its state and stage", texts.contains("Running · Searching"), "\(texts)")
            check("open bare task says how long it has run", texts.contains { $0.hasPrefix("Started 12 min ago") }, "\(texts)")
            check("open bare task has its X", b.closeCentre() != nil, "")
            let dn = app.controllers["st-done"]!
            check("a click opens a done island", dn.expanded, "")
            check("done island shows its open target as a button", dn.bodyTexts().contains("Open compare3.html"), "\(dn.bodyTexts())")
            check("done island says when it finished", dn.bodyTexts().contains { $0.hasPrefix("Finished just now") }, "\(dn.bodyTexts())")
            click(dn) { if case .open = $0 { return true } else { return false } }
            let g = app.controllers["st-gone"]!
            check("a missing file says so instead of a dead button", g.bodyTexts().contains("Not found: old.html") && g.controlCentre({ if case .open = $0 { return true } else { return false } }) == nil, "\(g.bodyTexts())")
            // Details in one call: summary, steps, key values.
            if let o = IslandFile.patch("st-bare", { o in
                o["summary"] = "Ten male voices from the library, ranked by clarity"
                o["steps"] = [["title": "Collect", "state": "done"], ["title": "Rank", "state": "now"], ["title": "Write up", "state": "todo"]]
                o["kv"] = [["label": "Voices", "value": "10"], ["label": "Source", "value": "Library"]]
            }) { _ = o }
        }
        after(9.4) { [self] in
            let log = (try? String(contentsOf: Paths.root.appendingPathComponent("opened.log"), encoding: .utf8)) ?? ""
            check("the open button opens the file in the default app", log.contains("compare3.html") && !log.contains("\tnone"), log.trimmingCharacters(in: .newlines))
            let t = app.controllers["st-bare"]!.bodyTexts()
            check("details arrive in the open island", t.contains("Rank") && t.contains("Library") && t.contains { $0.hasPrefix("Ten male voices") }, "\(t)")
        }
        after(9.5) { [self] in
            // Audio viewer: a 2 s tone (muted here), play, scrub, and Esc pauses it.
            let wav = Paths.root.appendingPathComponent("selftest-tone.wav")
            let fmt = AVAudioFormat(standardFormatWithSampleRate: 22_050, channels: 1)!
            if let buf = AVAudioPCMBuffer(pcmFormat: fmt, frameCapacity: 44_100), let f = try? AVAudioFile(forWriting: wav, settings: fmt.settings) {
                buf.frameLength = 44_100
                for i in 0..<44_100 { buf.floatChannelData![0][i] = Float(sin(Double(i) * 0.06) * 0.3) }
                try? f.write(from: buf)
            }
            put("st-audio", ["preset": "viewer", "title": "Tone", "path": wav.path, "expanded": true])
        }
        after(10.3) { [self] in
            guard let v = app.controllers["st-audio"],
                  let a = v.panel.contentView?.subviews.first(where: { $0 is ViewerRootView })?.subviews.compactMap({ $0 as? AudioPlayerView }).first else {
                check("audio viewer shows its player", false, ""); return
            }
            a.mute()
            let play = a.convert(a.buttonCentre, to: nil)
            send(v, .leftMouseDown, play)
            send(v, .leftMouseUp, play)
            check("audio plays on a click", a.isPlaying, "")
            let mid = a.convert(a.wavePoint(0.5), to: nil)
            send(v, .leftMouseDown, mid)
            send(v, .leftMouseUp, mid)
            after(0.15) { [self] in
                check("a click on the waveform scrubs", abs(a.seconds - 1.0) < 0.25, "\(a.seconds)")
                v.panel.cancelOperation(nil)
                check("closing the viewer pauses the audio", !a.isPlaying, "")
                try? FileManager.default.removeItem(at: Paths.file("st-audio"))
                try? FileManager.default.removeItem(at: Paths.root.appendingPathComponent("selftest-tone.wav"))
            }
        }
        after(11.0) { [self] in
            // Every attention pulse is recorded with its time and what is animating as it starts, so the pulse checks
            // do not depend on when a fixed-time step happens to run (a busy machine delays those).
            IslandController.labPulse = { [self] c in
                pulses[c.id, default: []].append((ProcessInfo.processInfo.systemUptime, c.animatedLayers()))
                if c.id == "st-bell", pulses["st-bell"]?.count == 1 {
                    // Mid-way to the next pulse: nothing may be animating on any island that has pulsed (the attention
                    // timer draws nothing between pulses). Other islands may still be settling from this test's own
                    // steps; the whole app's stillness is the at-rest check's job.
                    after(Attention.interval / 2) { [self] in
                        let between = app.controllers.values.filter { $0.pulseCount > 0 }.flatMap { c in c.animatedLayers().filter { !$0.hasSuffix(".work") }.map { "\(c.id): \($0)" } }
                        check("between attention pulses nothing animates", between.isEmpty, between.joined(separator: ", "))
                        check("attention stays set while the key is", app.controllers["st-bell"]?.attentionShown == "waiting" && app.controllers["st-bell"]?.pulseCount == 1, "")
                    }
                }
            }
            // Plan islands: To do, Scheduled, Settled docked left in that order; Settled a pill. Plus attention islands
            // and one born from the notch.
            put("todo", ["preset": "todo", "list": "todo", "title": "To do", "expanded": true, "items": [
                ["id": "a1", "title": "Arsenal: push branch", "group": "running"],
                ["id": "a2", "title": "Qwen-Image into Arsenal", "group": "running"],
                ["id": "caches", "title": "Delete old caches", "group": "waiting", "proposal": ["text": "~/.cache/huggingface 12 GB\n~/Library/Caches 4 GB"]],
                ["id": "echo", "title": "Echo: Heimdall hears itself", "group": "discussed"]]])
            put("scheduled", ["preset": "todo", "list": "scheduled", "title": "Scheduled", "expanded": true, "items": [
                ["id": "menu", "title": "Heimdall: menu revamp", "when": "Tonight"],
                // Due times: one counts down, one falls due in 3 s and moves itself to To do (Waiting on you).
                ["id": "tap", "title": "Arsenal: tap", "due": Date().timeIntervalSince1970 + 150],
                ["id": "due-now", "title": "Review the night pass", "due": Date().timeIntervalSince1970 + 3, "when": "Soon", "detail": "kept"]]])
            put("settled", ["preset": "todo", "list": "settled", "title": "Settled", "items": [["id": "s1", "title": "Night pass stage 1"]]])
            put("st-q", ["title": "Port", "state": "waiting", "attention": "question", "position": ["x": 900, "y": 640],
                         "parts": [["type": "button", "label": "Ship", "action": "t3q:r1:0"]]])
            put("st-bell", ["title": "Build", "state": "running", "attention": "waiting", "position": ["x": 1100, "y": 640]])
            put("st-approvals", ["preset": "todo", "title": "Approvals", "position": ["x": 700, "y": 640],
                                 "items": [["id": "w", "title": "Publish preview", "group": "waiting"]]])
            put("st-notch", ["title": "New job", "state": "running", "stage": "Starting", "origin": ["from": "notch"]])
        }
        after(11.9) { [self] in
            let scr = NSScreen.main!.frame
            guard let td = app.controllers["todo"], let sc = app.controllers["scheduled"], let se = app.controllers["settled"] else {
                check("plan islands on screen", false, ""); return
            }
            check("plan islands dock left", [td, sc, se].allSatisfy { abs($0.shapeRect.minX - (scr.minX + 8)) < 1 }, "\(td.shapeRect) \(sc.shapeRect) \(se.shapeRect)")
            check("plan islands in order To do, Scheduled, Settled", td.shapeRect.minY > sc.shapeRect.maxY && sc.shapeRect.minY > se.shapeRect.maxY, "")
            check("Settled is a pill", !se.expanded && se.shapeRect.height == Look.pillH, "\(se.shapeRect)")
            check("To do shows its groups", td.bodyTexts().contains("RUNNING") && td.bodyTexts().contains("WAITING ON YOU") && td.bodyTexts().contains("DISCUSSED"), "\(td.bodyTexts())")
            check("Scheduled shows when", sc.bodyTexts().contains("Tonight"), "\(sc.bodyTexts())")
            check("Scheduled counts down to a due time", sc.bodyTexts().contains("in 3 min"), "\(sc.bodyTexts())")
            eventually("a due Scheduled item moves to To do by itself", timeout: 4, { [self] in
                let t = (read("todo")?["items"] as? [JSONObject] ?? []).first { $0["id"] as? String == "due-now" }
                let still = (read("scheduled")?["items"] as? [JSONObject] ?? []).contains { $0["id"] as? String == "due-now" }
                return t?["group"] as? String == "waiting" && t?["when"] == nil && t?["detail"] as? String == "kept" && !still
                    && responses().contains { $0["kind"] as? String == "due" && $0["value"] as? String == "due-now" && $0["island"] as? String == "scheduled" }
            }, { [self] in "\(read("todo")?["items"] ?? []) / \(read("scheduled")?["items"] ?? [])" })
            let q = app.controllers["st-q"]!, bell = app.controllers["st-bell"]!, ap = app.controllers["st-approvals"]!
            check("question attention shown", q.attentionShown == "question", "\(q.attentionShown ?? "nil")")
            check("waiting attention shown", bell.attentionShown == "waiting", "")
            check("a plan island with an open waiting item asks for attention", ap.attentionShown == "waiting", "")
            check("an open plan island does not pulse", td.attentionShown == nil, "")
            // Tick a running item: a `tick` response with its id, the file updated, the row sinks below the other.
            click(td) { if case let .tick(key, _) = $0 { return key == "a1" } else { return false } }
            // Next run-loop turn: the springs are committed (and timed) by then.
            DispatchQueue.main.async { [self] in
                check("a tick starts the sink animation", td.animatedLayers().contains { $0.contains("slide") }, "\(td.animatedLayers())")
            }
        }
        after(12.3) { [self] in
            let td = app.controllers["todo"]!
            let q = app.controllers["st-q"]!
            _ = q
            eventually("attention pulses: glow and symbol", { [self] in
                pulses["st-q"]?.first.map { p in p.1.contains { $0.contains("attnGlow") } && p.1.contains { $0.contains("attnRing") } } ?? false
            }, { [self] in "\(pulses["st-q"].map { $0.map(\.1) } ?? [])" })
            let r = responses().first { $0["kind"] as? String == "tick" }
            check("tick writes a tick response with the item id", r?["island"] as? String == "todo" && r?["value"] as? String == "a1" && r?["action"] as? String == "done", "\(r ?? [:])")
            let a1 = ((read("todo")?["items"] as? [JSONObject]) ?? []).first { $0["id"] as? String == "a1" }
            check("tick updates the file", a1?["done"] as? Bool == true, "\(a1 ?? [:])")
            check("a ticked item sinks to the bottom of its group", Array(td.rowOrder().prefix(2)) == ["a2", "a1"], "\(td.rowOrder())")
            let n = app.controllers["st-notch"]!
            let notch = IslandController.notchRect(NSScreen.main)
            check("notch origin: grows out of the notch", n.bornFrom == notch && notch.width > 0, "\(n.bornFrom ?? .zero) vs \(notch)")
            // Dropped 0.7 s after it lands (the grow spring has settled by then).
            eventually("notch origin: the key is dropped", { [self] in read("st-notch")?["origin"] == nil }, { [self] in "\(read("st-notch") ?? [:])" })
            check("notch origin: lands in its slot", abs(n.shapeRect.maxX - (NSScreen.main!.frame.maxX - 8)) < 1, "\(n.shapeRect)")
            // The agent adds an item: it slides in with a highlight.
            _ = IslandFile.patch("todo") { o in o["items"] = (o["items"] as? [Any] ?? []) + [["id": "n1", "title": "Phonon voices as presets", "group": "discussed"]] }
            // The question's key goes away: attention cleared.
            _ = IslandFile.patch("st-q") { $0.removeValue(forKey: "attention") }
        }
        after(12.6) { [self] in
            let td = app.controllers["todo"]!
            // The file change reaches the app when the watcher's rescan runs: wait for it, then the row must be
            // arriving with its highlight (1.4 s, far longer than the poll step).
            eventually("an added item arrives with a highlight", { td.rowOrder().contains("n1") && td.animatedLayers().contains { $0.contains("highlight") } },
                       { "\(td.animatedLayers()) \(td.rowOrder())" })
            eventually("removing the attention key clears it", { [self] in app.controllers["st-q"]?.attentionShown == nil }, { "" })
            // Open the approval: its text and Yes / No / Later.
            click(td) { if case let .disclose(key) = $0 { return key == "caches" } else { return false } }
            // The user opens the derived-attention island: cleared.
            let ap = app.controllers["st-approvals"]!
            let c = CGPoint(x: ap.shapeRect.midX, y: ap.shapeRect.midY)
            sendScreen(ap, .leftMouseDown, c)
            sendScreen(ap, .leftMouseUp, c)
        }
        after(13.2) { [self] in
            let td = app.controllers["todo"]!
            check("an approval opens to its proposal", td.bodyTexts().contains { $0.contains("12 GB") } && td.bodyTexts().contains("Yes") && td.bodyTexts().contains("Later"), "\(td.bodyTexts())")
            check("opening the island clears its attention", app.controllers["st-approvals"]?.attentionShown == nil && app.controllers["st-approvals"]?.expanded == true, "")
            // An answer asks "Sure?" first; a second click on the same button sends it.
            click(td) { if case let .approve(key, action) = $0 { return key == "caches" && action == "yes" } else { return false } }
            check("an approval asks before it sends", td.isConfirming && !responses().contains { $0["kind"] as? String == "approval" }, "")
            td.labConfirm(true)
            let r = responses().first { $0["kind"] as? String == "approval" }
            check("approval writes its response", r?["island"] as? String == "todo" && r?["action"] as? String == "yes" && r?["value"] as? String == "caches", "\(r ?? [:])")
            let it = ((read("todo")?["items"] as? [JSONObject]) ?? []).first { $0["id"] as? String == "caches" }
            check("the choice is kept on the item", it?["chosen"] as? String == "yes", "")
            check("the row shows the choice", td.controlCentre({ if case .approve = $0 { return true } else { return false } }) == nil, "")
            // Moved to another group by the agent: it slides there.
            _ = IslandFile.patch("todo") { o in
                o["items"] = (o["items"] as? [JSONObject] ?? []).map { var i = $0; if i["id"] as? String == "echo" { i["group"] = "running" }; return i }
            }
        }
        after(13.5) { [self] in
            let td = app.controllers["todo"]!
            check("a moved item slides to its new group", td.rowOrder().firstIndex(of: "echo").map { $0 < (td.rowOrder().firstIndex(of: "a1") ?? 0) } ?? false, "\(td.rowOrder())")
            // Removed by the agent: it fades, the rest close up.
            _ = IslandFile.patch("todo") { o in o["items"] = (o["items"] as? [JSONObject] ?? []).filter { $0["id"] as? String != "n1" } }
        }
        after(15.0) { [self] in
            let td = app.controllers["todo"]!
            check("after the choice the proposal closes", td.rowFrame("prop:caches") == nil && td.bodyTexts().contains("Yes"), "\(td.bodyTexts())")
            check("a removed item is gone", !td.rowOrder().contains("n1"), "\(td.rowOrder())")
            check("the open list's shape fits its rows", abs(td.shapeRect.height - td.expandedSize.height) < 1, "\(td.shapeRect) \(td.expandedSize)")
        }
        after(16.9) { [self] in
            // Idle: nothing changing, so nothing may be animating anywhere (no loops in the render server). The only
            // exception is an attention pulse, and only on an island whose attention is set; between pulses (here,
            // 5 s after the last) even those are still.
            let busy = app.controllers.values.flatMap { c in
                c.animatedLayers().filter { !(c.attentionShown != nil && $0.contains("attn")) && !$0.hasSuffix(".work") }.map { "\(c.id): \($0)" }
            }
            // The running islands' sign of work (one short turn every 4 s) is the other exception.
            check("at rest no layer is animating", busy.isEmpty, busy.joined(separator: ", "))
        }
        after(17.4) { [self] in
            // The second pulse, 6 s after the first (timer leeway 0.25 s; a busy main thread may add a little).
            eventually("the next attention pulse comes 6 s later", timeout: 4, { [self] in
                guard let p = pulses["st-bell"], p.count >= 2 else { return false }
                return abs(p[1].0 - p[0].0 - Attention.interval) < 0.6 && p[1].1.contains { $0.contains("attnGlow") }
            }, { [self] in "\(pulses["st-bell"].map { $0.map { String(format: "%.2f %@", $0.0, $0.1.description) } } ?? [])" })
        }
        // Viewer refresh (bugs 22 Oct 20:45 and 20:46): an open viewer follows its file, keeps its scroll position,
        // and reads the file anew when opened again.
        let refresh = Paths.root.appendingPathComponent("selftest-refresh.md")
        let lines = { (tag: String) in "# \(tag)\n\n" + (1...120).map { "Line \($0) of \(tag)." }.joined(separator: "\n\n") }
        after(18.5) { [self] in
            try? lines("First").write(to: refresh, atomically: true, encoding: .utf8)
            put("st-refresh", ["preset": "viewer", "title": "Refresh", "path": refresh.path, "expanded": true])
        }
        after(19.4) { [self] in
            // Once the live view has taken over from the opening still: scroll down, then replace the file by rename,
            // the way editors and agents save.
            waitFor(3, { [self] in app.controllers["st-refresh"].flatMap { viewerText($0) }?.hasPrefix("First") == true }) { [self] ok in
                check("refresh viewer shows its text", ok, app.controllers["st-refresh"].flatMap { viewerText($0) }?.prefix(40).description ?? "nil")
                guard ok, let v = app.controllers["st-refresh"], let sv = viewerScroll(v) else { return }
                sv.contentView.scroll(to: CGPoint(x: 0, y: 400))
                sv.reflectScrolledClipView(sv.contentView)
                try? lines("Second").write(to: refresh, atomically: true, encoding: .utf8)
                eventually("an open viewer re-renders when its file is replaced", timeout: 2, { [self] in viewerText(v)?.hasPrefix("Second") == true },
                           { [self] in viewerText(v)?.prefix(40).description ?? "nil" })
            }
        }
        after(21.0) { [self] in
            guard let v = app.controllers["st-refresh"] else { return }
            let y = viewerScroll(v)?.contentView.bounds.origin.y ?? -1
            check("a re-render keeps the scroll position", abs(y - 400) < 2, "\(y)")
            // Written in place (same inode).
            if let h = FileHandle(forWritingAtPath: refresh.path) { h.truncateFile(atOffset: 0); h.write(lines("Third").data(using: .utf8)!); h.closeFile() }
            eventually("an open viewer re-renders when its file is written in place", timeout: 2, { [self] in viewerText(v)?.hasPrefix("Third") == true },
                       { [self] in viewerText(v)?.prefix(40).description ?? "nil" })
        }
        after(21.9) { [self] in
            guard let v = app.controllers["st-refresh"] else { return }
            v.panel.cancelOperation(nil)
            try? lines("Fourth").write(to: refresh, atomically: true, encoding: .utf8)
        }
        after(22.4) { [self] in
            guard let v = app.controllers["st-refresh"] else { return }
            if let o = IslandFile.patch("st-refresh", { $0["expanded"] = true; $0["updated"] = Date().timeIntervalSince1970 }) { v.update(Island(id: "st-refresh", raw: o)) }
            eventually("viewing the same path again reads the file anew", timeout: 3, { [self] in viewerText(v)?.hasPrefix("Fourth") == true },
                       { [self] in viewerText(v)?.prefix(40).description ?? "nil" })
        }
        after(23.8) { [self] in
            app.controllers["st-refresh"]?.panel.cancelOperation(nil)
            try? FileManager.default.removeItem(at: refresh)
            finish()
        }
    }

    /// Waits for checks still polling, then prints the results and quits.
    private func finish() {
        guard pending == 0 else { after(0.1) { [self] in finish() }; return }
        check("a cleared attention never pulses again", app.controllers["st-q"]?.pulseCount == 1 && app.controllers["st-approvals"]?.pulseCount == 1,
              "\(app.controllers["st-q"]?.pulseCount ?? -1) \(app.controllers["st-approvals"]?.pulseCount ?? -1)")
        do {
            let failed = results.filter { !$0.1 }
            for r in results { print("\(r.1 ? "PASS" : "FAIL")  \(r.0)\(r.2.isEmpty || r.1 ? "" : "  (\(r.2))")") }
            print("selftest: \(results.count - failed.count)/\(results.count) passed")
            for d in [Paths.islands, Paths.responses] {
                for f in (try? FileManager.default.contentsOfDirectory(at: d, includingPropertiesForKeys: nil)) ?? [] { try? FileManager.default.removeItem(at: f) }
            }
            for f in ["compare3.html", "opened.log"] { try? FileManager.default.removeItem(at: Paths.root.appendingPathComponent(f)) }
            after(0.5) {
                try? FileManager.default.removeItem(at: Paths.pid)
                try? FileManager.default.removeItem(at: Paths.state)
                exit(failed.isEmpty ? 0 : 1)
            }
        }
    }

    private var pulses: [String: [(TimeInterval, [String])]] = [:]
    private var pending = 0

    /// A check on something that follows a file change or a timer: polled every 20 ms until it holds or `timeout`
    /// passes. Strict as a plain check, but not tied to when a fixed-time step runs on a busy machine.
    private func eventually(_ name: String, timeout: Double = 3, _ cond: @escaping () -> Bool, _ detail: @escaping () -> String) {
        pending += 1
        let start = ProcessInfo.processInfo.systemUptime
        var poll: (() -> Void)!
        poll = { [self] in
            if cond() { check(name, true, ""); pending -= 1; poll = nil; return }
            if ProcessInfo.processInfo.systemUptime - start > timeout { check(name, false, detail()); pending -= 1; poll = nil; return }
            after(0.02) { poll?() }
        }
        poll()
    }

    /// Polls `cond` every 20 ms for up to `timeout` s, then runs `then` with whether it held (finish waits for it).
    private func waitFor(_ timeout: Double, _ cond: @escaping () -> Bool, then: @escaping (Bool) -> Void) {
        pending += 1
        let start = ProcessInfo.processInfo.systemUptime
        var poll: (() -> Void)!
        poll = { [self] in
            let ok = cond()
            if ok || ProcessInfo.processInfo.systemUptime - start > timeout { pending -= 1; poll = nil; then(ok); return }
            after(0.02) { poll?() }
        }
        poll()
    }

    private func viewerScroll(_ c: IslandController) -> NSScrollView? {
        c.panel.contentView?.subviews.first { $0 is ViewerRootView && !$0.isHidden }?.subviews.compactMap { $0 as? NSScrollView }.first
    }
    private func viewerText(_ c: IslandController) -> String? { (viewerScroll(c)?.documentView as? NSTextView)?.string }

    private func put(_ id: String, _ o: JSONObject) { try? IslandFile.writeAtomic(o.merging(["id": id]) { a, _ in a }, to: Paths.file(id)) }
    private func read(_ id: String) -> JSONObject? { IslandFile.read(Paths.file(id)) }
    private func responses() -> [JSONObject] {
        ((try? FileManager.default.contentsOfDirectory(at: Paths.responses, includingPropertiesForKeys: nil)) ?? []).compactMap(IslandFile.read)
    }
    private func check(_ name: String, _ ok: Bool, _ detail: String) { results.append((name, ok, detail)) }
    private func after(_ s: Double, _ f: @escaping () -> Void) { DispatchQueue.main.asyncAfter(deadline: .now() + s, execute: f) }

    private func click(_ c: IslandController, _ match: (Control) -> Bool) {
        guard let p = c.controlCentre(match) else { check("control found on \(c.id)", false, ""); return }
        send(c, .leftMouseDown, p)
        send(c, .leftMouseUp, p)
    }

    private func send(_ c: IslandController, _ type: NSEvent.EventType, _ windowPoint: CGPoint) {
        guard let e = NSEvent.mouseEvent(with: type, location: windowPoint, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                                         windowNumber: c.panel.windowNumber, context: nil, eventNumber: 0, clickCount: 1,
                                         pressure: type == .leftMouseUp ? 0 : 1) else { return }
        c.panel.sendEvent(e)
    }

    private func sendScreen(_ c: IslandController, _ type: NSEvent.EventType, _ screenPoint: CGPoint) {
        send(c, type, c.panel.convertPoint(fromScreen: screenPoint))
    }
}
