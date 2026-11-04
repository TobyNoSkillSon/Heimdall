import AppKit
import IslandModel

/// Watches the islands folder with kqueue (no polling): one source on the folder (create, rename, delete) and one
/// per island file (written in place), each triggering one coalesced rescan; only files whose bytes changed are parsed.
final class Store {
    private let dir: URL
    private var source: DispatchSourceFileSystemObject?
    private var fileSources: [String: (ino: UInt64, src: DispatchSourceFileSystemObject)] = [:]
    private var retried = Set<String>()
    private var cache: [String: Data] = [:]
    private var scheduled = false
    private let onChange: (_ upserts: [Island], _ removed: [String]) -> Void

    init(dir: URL, onChange: @escaping ([Island], [String]) -> Void) {
        self.dir = dir
        self.onChange = onChange
    }

    func start() {
        let fd = open(dir.path, O_EVTONLY)
        guard fd >= 0 else { NSLog("FloatingIslands: cannot watch \(dir.path)"); return }
        let s = DispatchSource.makeFileSystemObjectSource(fileDescriptor: fd, eventMask: [.write, .rename, .delete], queue: .main)
        s.setEventHandler { [weak self] in self?.schedule() }
        s.setCancelHandler { close(fd) }
        s.resume()
        source = s
        scan()
    }

    private func watchFile(_ id: String, _ path: String) {
        var st = stat()
        guard stat(path, &st) == 0 else { return }
        let ino = UInt64(st.st_ino)
        if fileSources[id]?.ino == ino { return }
        fileSources.removeValue(forKey: id)?.src.cancel()
        let fd = open(path, O_EVTONLY)
        guard fd >= 0 else { return }
        let s = DispatchSource.makeFileSystemObjectSource(fileDescriptor: fd, eventMask: [.write, .extend], queue: .main)
        s.setEventHandler { [weak self] in self?.schedule() }
        s.setCancelHandler { close(fd) }
        s.resume()
        fileSources[id] = (ino, s)
    }

    private func schedule() {
        guard !scheduled else { return }
        scheduled = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.012) { [weak self] in
            self?.scheduled = false
            self?.scan()
        }
    }

    func scan() {
        let names = (try? FileManager.default.contentsOfDirectory(atPath: dir.path)) ?? []
        var seen = Set<String>()
        var upserts: [(Date, String, Island)] = []
        for name in names.sorted() where name.hasSuffix(".json") && !name.hasPrefix(".") {
            let id = String(name.dropLast(5))
            seen.insert(id)
            let path = dir.appendingPathComponent(name).path
            watchFile(id, path)
            guard let data = FileManager.default.contents(atPath: path) else { continue }
            if cache[id] == data { continue }
            guard let o = (try? JSONSerialization.jsonObject(with: data)) as? JSONObject else {
                // Possibly caught mid-write by a writer that does not rename: look once more shortly.
                if !retried.contains(id) {
                    retried.insert(id)
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.06) { [weak self] in self?.scan() }
                }
                continue
            }
            retried.remove(id)
            let isNew = cache[id] == nil
            cache[id] = data
            // New islands keep the order they were created in (one scan can pick up several at once).
            let born = isNew ? ((try? FileManager.default.attributesOfItem(atPath: dir.appendingPathComponent(name).path))?[.creationDate] as? Date ?? .distantPast) : .distantPast
            upserts.append((born, name, Island(id: id, raw: o)))
        }
        let removed = cache.keys.filter { !seen.contains($0) }
        removed.forEach { cache.removeValue(forKey: $0) }
        for id in fileSources.keys where !seen.contains(id) { fileSources.removeValue(forKey: id)?.src.cancel() }
        upserts.sort { ($0.0, $0.1) < ($1.0, $1.1) }
        if !upserts.isEmpty || !removed.isEmpty { onChange(upserts.map(\.2), removed) }
    }
}

final class AppController: NSObject {
    private(set) var controllers: [String: IslandController] = [:]
    private var store: Store!
    private var seq = 0
    private var monitors: [Any] = []
    private var currentScreen: CGRect = .zero
    private var relayoutQueued = false
    private var running = false

    func start() {
        Paths.ensure()
        store = Store(dir: Paths.islands) { [weak self] up, rm in self?.apply(up, rm) }
        store.start()
        // From here on a new island has just been made: it drops out of the notch (the first scan's were already there).
        running = true
        NotificationCenter.default.addObserver(self, selector: #selector(screensChanged),
                                               name: NSApplication.didChangeScreenParametersNotification, object: nil)
        screensChanged()
        // The due clock follows the wall clock: re-read after sleep and when the time or time zone changes.
        NSWorkspace.shared.notificationCenter.addObserver(self, selector: #selector(clockChanged), name: NSWorkspace.didWakeNotification, object: nil)
        for n in [Notification.Name.NSSystemClockDidChange, .NSSystemTimeZoneDidChange] {
            NotificationCenter.default.addObserver(self, selector: #selector(clockChanged), name: n, object: nil)
        }
        reviewDue()
        Streams.start { [weak self] in self?.controllers.values.map { $0.panel } ?? [] }
    }

    // MARK: Scheduled due times: the app counts down and moves due items to To do by itself.

    private var dueTimer: DispatchSourceTimer?
    private var dueNext: Date?

    @objc private func clockChanged() { reviewDue(force: true) }

    /// Moves every due Scheduled item to To do (Waiting), one `due` response each, then sets one one-shot timer for
    /// the next moment a label changes or an item falls due. No due items: no timer. Called after every change in the
    /// folder (cheap: one small file read), by the timer and after sleep or a clock change.
    func reviewDue(force: Bool = false) {
        let now = Date()
        let moved: [(id: String, title: String)] = IslandFile.locked {
            guard let s = IslandFile.read(Paths.file("scheduled")) else { return [] }
            // A To do file that exists but does not parse is never replaced: the move waits for a readable one.
            let todoURL = Paths.file("todo")
            let t: JSONObject
            if let o = IslandFile.read(todoURL) { t = o }
            else if FileManager.default.fileExists(atPath: todoURL.path) { return [] }
            else { t = PlanList.newIsland("todo") }
            guard let r = Due.move(scheduled: s, todo: t, now: now.timeIntervalSince1970) else { return [] }
            // To do first: a crash between the two writes leaves a duplicate the next pass removes, never a loss.
            do {
                // Nothing new for To do (a crash left the item in both): only Scheduled is written, so a Scheduled
                // write that keeps failing never loops through To do's watch.
                if r.todoChanged { try IslandFile.writeAtomic(r.todo, to: Paths.file("todo")) }
                try IslandFile.writeAtomic(r.scheduled, to: Paths.file("scheduled"))
            } catch { NSLog("FloatingIslands: due move not written: \(error)"); return [] }
            return r.moved
        }
        for m in moved { IslandFile.respond(island: "scheduled", kind: "due", action: "moved", value: m.id) }
        let dues = Plan.items(IslandFile.read(Paths.file("scheduled")) ?? [:]).compactMap(\.due)
        let next = dues.map { Due.nextChange($0, now: now) }.min().flatMap { $0 == .distantFuture ? nil : $0 }
        if !force, next == dueNext, dueTimer != nil { return }
        dueTimer?.cancel()
        dueTimer = nil
        dueNext = next
        guard let next else { return }
        let t = DispatchSource.makeTimerSource(queue: .main)
        // Wall-clock deadline: a sleeping Mac wakes to the right label.
        t.schedule(wallDeadline: .now() + max(0.05, next.timeIntervalSince(now)) + 0.02, leeway: .milliseconds(500))
        t.setEventHandler { [weak self] in
            guard let self else { return }
            self.dueTimer = nil
            self.dueNext = nil
            self.controllers["scheduled"]?.refreshPlanLabels()
            self.reviewDue()
        }
        t.resume()
        dueTimer = t
    }

    private func apply(_ upserts: [Island], _ removed: [String]) {
        for id in removed {
            guard let c = controllers.removeValue(forKey: id) else { continue }
            c.remove { c.panel.close() }
        }
        if !removed.isEmpty { stateChanged() }
        for i in upserts {
            if i.hidden {
                if let c = controllers.removeValue(forKey: i.id) { c.remove { c.panel.close() } }
                continue
            }
            if let c = controllers[i.id] { c.update(i); continue }
            seq += 1
            let c = IslandController(i, seq: seq)
            c.app = self
            c.fromNotch = running
            controllers[i.id] = c
            if i.expanded { c.setInitialExpanded() }
        }
        relayout()
        if running && (upserts.contains { $0.id == "scheduled" || $0.id == "todo" } || removed.contains("scheduled")) { reviewDue() }
    }

    // MARK: State readback for agents (state.json), written shortly after anything changes; nothing at rest.

    private var stateQueued = false

    func stateChanged() {
        guard !stateQueued else { return }
        stateQueued = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) { [weak self] in
            self?.stateQueued = false
            self?.writeState()
        }
    }

    /// Positions in screen points from the display's top-left corner (x right, y down): the island's top-left
    /// corner, the same numbers `islands put --at X,Y` takes.
    private func writeState() {
        var out: JSONObject = [:]
        for (id, c) in controllers where !c.removing {
            let r = c.shapeRect
            let scr = NSScreen.screens.first { $0.frame.intersects(r) }?.frame ?? currentScreen
            var o: JSONObject = ["open": c.expanded, "placed": c.island.position != nil,
                                 "x": Double(r.minX - scr.minX), "y": Double(scr.maxY - r.maxY),
                                 "width": Double(r.width), "height": Double(r.height)]
            if let side = c.island.side { o["side"] = side }
            if c.island.edgeY != nil { o["pinned"] = true }
            out[id] = o
        }
        try? IslandFile.writeAtomic(["islands": out, "display": ["width": Double(currentScreen.width), "height": Double(currentScreen.height)]], to: Paths.state)
    }

    /// The display the islands live on: the one with the pointer.
    private func activeScreen() -> NSScreen? {
        let p = NSEvent.mouseLocation
        return NSScreen.screens.first { NSMouseInRect(p, $0.frame, false) } ?? NSScreen.main ?? NSScreen.screens.first
    }

    /// Coalesced: many file changes in one run-loop turn produce one layout pass.
    func relayout() {
        guard !relayoutQueued else { return }
        relayoutQueued = true
        DispatchQueue.main.async { [weak self] in
            self?.relayoutQueued = false
            self?.layoutNow()
        }
    }

    /// The top of the row just under the menu bar: islands never enter the menu bar strip.
    private func rowTop(_ screen: NSScreen) -> CGFloat {
        let band = max(screen.safeAreaInsets.top, screen.frame.maxY - screen.visibleFrame.maxY, NSStatusBar.system.thickness)
        return screen.frame.maxY - band - 6
    }

    func layoutNow(_ spring: Spring = .grow) {
        guard let screen = activeScreen() else { return }
        let f = screen.frame
        currentScreen = f
        let top = rowTop(screen)
        let live = controllers.values.filter { !$0.removing }
        // Never taller than the display: a little under the room between the menu bar row and the Dock; the rest
        // scrolls inside.
        let cap = heightCap(screen)
        for c in live { c.heightCap = cap }
        let placed = live.filter { $0.island.position != nil }
        for c in placed {
            // Free placement: the stored point is the island's top-left corner from the display's top-left.
            let p = c.island.position!
            c.place(anchor: CGPoint(x: f.minX + CGFloat(p.x) + c.size.width / 2, y: f.maxY - CGFloat(p.y)), screen: f, rowTop: top, spring)
        }
        // Docks: a column at each side edge, below the band of a browser's tabs and toolbar, stepping past islands
        // placed freely there, and a row under the notch. Each slot is the island's current size, so an open one
        // pushes the rest of its stack along.
        let local = { (r: CGRect) in CGRect(x: r.minX - f.minX, y: f.maxY - r.maxY, width: r.width, height: r.height) }
        var obstacles = placed.map { local($0.shapeRect) }
        // The top row first: the side columns step past it too (an open island there can reach down past their tops).
        for side in ["top", "left", "right"] {
            let stack = dockOrder(live.filter { $0.island.side == side })
            guard !stack.isEmpty else { continue }
            let sizes = stack.map(\.slotSize)
            let bottom = f.maxY - screen.visibleFrame.minY - 8
            let slots: [CGRect]
            // On an edge, an island the user pinned keeps its height; the rest take their turn around it.
            let pins = stack.map { $0.island.edgeY.map { CGFloat($0) } }
            switch side {
            case "left": slots = leftEdgeColumn(sizes: sizes, pins: pins, width: f.width, top: columnTop(screen), highest: f.maxY - top, bottom: bottom, obstacles: obstacles)
            case "top": slots = topRow(sizes: sizes, width: f.width, top: f.maxY - top)
            default: slots = edgeColumn(sizes: sizes, pins: pins, width: f.width, top: columnTop(screen), highest: f.maxY - top, bottom: bottom, obstacles: obstacles)
            }
            for (c, r) in zip(stack, slots) {
                c.place(anchor: CGPoint(x: f.minX + r.midX, y: f.maxY - r.minY), screen: f, rowTop: top, spring)
            }
            if side == "top" { obstacles += slots }
        }
    }

    /// The tallest an island may be on `screen`: the visible height from under the menu bar row to 8 pt above the
    /// Dock, less 24 pt, so a full-height island still shows the desktop around it.
    func heightCap(_ screen: NSScreen) -> CGFloat {
        max(200, rowTop(screen) - (screen.visibleFrame.minY + 8) - 24)
    }

    private func dockOrder(_ cs: [IslandController]) -> [IslandController] {
        cs.sorted { ($0.island.order ?? Double($0.seq), $0.seq) < ($1.island.order ?? Double($1.seq), $1.seq) }
    }

    /// Where the side columns start, from the display's top: 60 % of the display's height up from its bottom, so the
    /// top corners, where websites keep their controls, stay empty; never higher than under the
    /// menu bar plus a browser's tab strip and toolbar. A tall stack still moves up as far as it must to stay on screen.
    func columnTop(_ screen: NSScreen) -> CGFloat {
        let band = max(screen.safeAreaInsets.top, screen.frame.maxY - screen.visibleFrame.maxY, NSStatusBar.system.thickness)
        return max(screen.frame.height * (1 - Self.columnStart), band + Self.browserChrome)
    }
    static let browserChrome: CGFloat = 88
    /// The side columns' first island starts this far up the display, as a share of its height from the bottom.
    static let columnStart: CGFloat = 0.6

    /// Where a released island goes, on the display it was released over: thrown or dropped, it attaches to the
    /// nearer side edge at the height it lands (a throw carries on along its path first) and keeps that height.
    /// Islands in that column make room around it. Writes the island's file.
    func drop(_ c: IslandController, _ r: CGRect, velocity v: CGVector) -> JSONObject? {
        // The display under the pointer: the one layoutNow will place it on.
        guard let screen = activeScreen() else { return nil }
        let f = screen.frame
        let d = edgeDrop(r, velocity: v, screen: f, highest: rowTop(screen), lowest: screen.visibleFrame.minY + 8)
        return IslandFile.patch(c.id) { j in
            j.removeValue(forKey: "position")
            j["side"] = d.side
            j["edgeY"] = Double(f.maxY - d.top)
        }
    }

    @objc private func screensChanged() {
        // Follow the pointer between displays only when there is more than one (no monitor otherwise).
        monitors.forEach(NSEvent.removeMonitor)
        monitors = []
        if NSScreen.screens.count > 1 {
            let check: () -> Void = { [weak self] in
                guard let self, let s = self.activeScreen(), s.frame != self.currentScreen else { return }
                self.relayout()
            }
            if let g = NSEvent.addGlobalMonitorForEvents(matching: [.mouseMoved, .leftMouseDragged], handler: { _ in check() }) { monitors.append(g) }
            if let l = NSEvent.addLocalMonitorForEvents(matching: [.mouseMoved], handler: { e in check(); return e }) { monitors.append(l) }
        }
        relayout()
    }
}
