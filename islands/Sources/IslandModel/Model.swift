// The island file format, shared by the app and the `islands` CLI.
// One JSON object per island in <root>/islands/<id>.json; clicks go to <root>/responses/<id>-<ms>.json.
import CoreGraphics
import Foundation

public typealias JSONObject = [String: Any]

public enum Paths {
    /// FLOATING_ISLANDS_DIR overrides the root (lab runs use an isolated folder).
    public static var root: URL {
        if let d = ProcessInfo.processInfo.environment["FLOATING_ISLANDS_DIR"], !d.isEmpty {
            return URL(fileURLWithPath: (d as NSString).expandingTildeInPath, isDirectory: true)
        }
        return FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/FloatingIslands", isDirectory: true)
    }
    public static var islands: URL { root.appendingPathComponent("islands", isDirectory: true) }
    public static var responses: URL { root.appendingPathComponent("responses", isDirectory: true) }
    /// What is on screen now, written by the app: per island open or not, frame, automatic or placed.
    public static var state: URL { root.appendingPathComponent("state.json") }
    public static var pid: URL { root.appendingPathComponent(".app.pid") }

    public static func ensure() {
        for u in [islands, responses] { try? FileManager.default.createDirectory(at: u, withIntermediateDirectories: true) }
    }
    public static func safeID(_ id: String) -> String {
        String(id.map { $0.isLetter || $0.isNumber || $0 == "-" || $0 == "_" || $0 == "." ? $0 : "-" })
    }
    public static func file(_ id: String) -> URL { islands.appendingPathComponent(safeID(id) + ".json") }
}

public enum Presets {
    public static let all = ["task", "waiting", "result", "list", "note", "viewer", "todo"]
    public static let states = ["running", "waiting", "done", "failed"]
}

/// The three stacks an island can dock to: a column at the left or right edge, or a row under the notch.
public enum Dock {
    public static let sides = ["left", "right", "top"]
}

/// A To do item's group, in display order, with the header the user reads.
public enum TodoGroup {
    public static let all = ["running", "waiting", "discussed"]
    /// Discussed: talked about, no decision or steps yet (agreed work goes to Scheduled, finished or agreed to Settled).
    public static func label(_ g: String) -> String {
        switch g { case "running": return "Running"; case "waiting": return "Waiting on you"; default: return "Discussed" }
    }
    /// "Echo bug=waiting" → ("Echo bug", "waiting", false); "=done" ticks it; anything else stays part of the text.
    public static func parse(_ s: String) -> (title: String, group: String?, done: Bool) {
        if let eq = s.lastIndex(of: "=") {
            let tail = String(s[s.index(after: eq)...]).trimmingCharacters(in: .whitespaces).lowercased()
            let head = String(s[..<eq]).trimmingCharacters(in: .whitespaces)
            if all.contains(tail) { return (head, tail, false) }
            if tail == "done" { return (head, nil, true) }
        }
        return (s.trimmingCharacters(in: .whitespaces), nil, false)
    }
}

public enum IslandFile {
    public static func read(_ url: URL) -> JSONObject? {
        guard let d = try? Data(contentsOf: url) else { return nil }
        return (try? JSONSerialization.jsonObject(with: d)) as? JSONObject
    }

    public static func encode(_ o: JSONObject) throws -> Data {
        try JSONSerialization.data(withJSONObject: o, options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes])
    }

    /// Write via a hidden temp file and rename, so a watcher never sees a half-written island.
    public static func writeAtomic(_ o: JSONObject, to url: URL) throws {
        let data = try encode(o)
        let tmp = url.deletingLastPathComponent().appendingPathComponent(".\(url.lastPathComponent).\(getpid()).tmp")
        try data.write(to: tmp)
        guard rename(tmp.path, url.path) == 0 else {
            try? FileManager.default.removeItem(at: tmp)
            throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno))
        }
    }

    /// Top-level merge; NSNull in `over` removes the key.
    public static func merge(_ base: JSONObject, _ over: JSONObject) -> JSONObject {
        var out = base
        for (k, v) in over { if v is NSNull { out.removeValue(forKey: k) } else { out[k] = v } }
        return out
    }

    /// Every read-modify-write (the app's and the CLI's) holds one folder lock, so a drop and an agent's update
    /// never overwrite each other.
    public static func locked<T>(_ body: () throws -> T) rethrows -> T {
        Paths.ensure()
        let fd = open(Paths.islands.appendingPathComponent(".write.lock").path, O_CREAT | O_RDWR, 0o644)
        if fd >= 0 { flock(fd, LOCK_EX) }
        defer { if fd >= 0 { flock(fd, LOCK_UN); close(fd) } }
        return try body()
    }

    /// Read-modify-write one island file. Returns the written object, nil if the island does not exist.
    @discardableResult
    public static func patch(_ id: String, _ change: (inout JSONObject) -> Void) -> JSONObject? {
        locked {
            let url = Paths.file(id)
            guard var o = read(url) else { return nil }
            change(&o)
            try? writeAtomic(o, to: url)
            return o
        }
    }

    private static var responseCount = 0

    /// Record a ready response object (`Responses.tick`, `Responses.approval`); `island` names the file.
    @discardableResult
    public static func respond(_ o: JSONObject) -> URL? {
        respond(island: o["island"] as? String ?? "island", kind: o["kind"] as? String ?? "button", action: o["action"] as? String, value: o["value"])
    }

    /// Record a click (button, list row, toggle, slider) for the shadow to pick up.
    @discardableResult
    public static func respond(island: String, kind: String, action: String?, value: Any?) -> URL? {
        Paths.ensure()
        let ms = Int(Date().timeIntervalSince1970 * 1000)
        var o: JSONObject = ["island": island, "kind": kind, "at": ISO8601DateFormatter().string(from: Date())]
        if let action { o["action"] = action }
        if let value { o["value"] = value }
        responseCount += 1
        let url = Paths.responses.appendingPathComponent("\(Paths.safeID(island))-\(ms)-\(getpid())-\(responseCount).json")
        return (try? writeAtomic(o, to: url)) == nil ? nil : url
    }
}

/// Typed view of one island file. Unknown keys are kept in `raw` and survive every rewrite.
public struct Island {
    public let id: String
    public let raw: JSONObject

    public init(id: String, raw: JSONObject) { self.id = id; self.raw = raw }

    /// The island's preset. The plan islands are known by id (`todo`, `scheduled`, `settled`): they are To do lists
    /// even without `preset`/`list` (the user's lists from before night 1022; Rust migrates them on first contact).
    public var preset: String {
        if PlanList.all.contains(id) { return "todo" }
        return (raw["preset"] as? String).flatMap { Presets.all.contains($0) ? $0 : nil } ?? "task"
    }
    public var title: String { raw["title"] as? String ?? (preset == "todo" ? PlanList.title(list ?? "todo") : id) }
    /// Which plan list a To do island is: `todo`, `scheduled` or `settled` (the `list` key; without one, the island's
    /// id when it is a plan list's, else `todo`). nil for every other preset.
    public var list: String? {
        guard preset == "todo" else { return nil }
        if let l = raw["list"] as? String, PlanList.all.contains(l) { return l }
        return PlanList.all.contains(id) ? id : "todo"
    }
    /// The plan items with their stable keys (ids), in file order.
    public var planItems: [PlanItem] { Plan.items(raw) }
    public var state: String {
        if let s = raw["state"] as? String, Presets.states.contains(s) { return s }
        switch preset {
        case "waiting": return "waiting"
        case "result": return "done"
        case "task": return "running"
        default: return "done"
        }
    }
    public var progress: Double? { (raw["progress"] as? NSNumber).map { min(1, max(0, $0.doubleValue)) } }
    public var value: String? {
        if let s = raw["value"] as? String { return s }
        if let n = raw["value"] as? NSNumber { return Self.format(n.doubleValue) }
        return nil
    }
    public var icon: String? { raw["icon"] as? String ?? list.map(PlanList.icon) }
    /// What a running task is doing, in a word or two ("Building"), shown when there is no live progress value.
    public var stage: String? { raw["stage"] as? String }
    /// The file a viewer island shows (markdown, text, image, PDF).
    public var path: String? { raw["path"] as? String }
    /// What a click on the open island's button opens: a URL or an absolute file path (empty means none).
    public var open: String? { (raw["open"] as? String).flatMap { $0.trimmingCharacters(in: .whitespaces).isEmpty ? nil : $0 } }
    /// One line shown when the island is opened.
    public var summary: String? { (raw["summary"] as? String).flatMap { $0.isEmpty ? nil : $0 } }
    /// A picture shown when the island is opened (a click on it opens the file).
    public var image: String? { (raw["image"] as? String).flatMap { $0.isEmpty ? nil : $0 } }
    /// Steps in order, each `done`, `now`, `failed` or `todo`. Items are {"title", "state"} or plain strings.
    public var steps: [(title: String, mark: String)] {
        (raw["steps"] as? [Any] ?? []).compactMap { v in
            if let s = v as? String { return Step.parse(s) }
            guard let o = v as? JSONObject, let t = o["title"] as? String else { return nil }
            return (t, Step.mark(o["state"] as? String))
        }
    }
    /// A plain list of items in order (no status marks), shown with bullets.
    public var items: [String] { todoItems.map(\.title) }
    /// Items with their place in the file's `items` array, their To do group and whether the user ticked them off.
    /// Items with their To do group (running, waiting, discussed) and whether the user ticked them off.
    public var todoItems: [(index: Int, title: String, group: String?, done: Bool)] {
        var out: [(index: Int, title: String, group: String?, done: Bool)] = []
        for (k, v) in (raw["items"] as? [Any] ?? []).enumerated() {
            if let s = v as? String { if !s.isEmpty { out.append((k, s, nil, false)) }; continue }
            guard let o = v as? JSONObject, let t = o["title"] as? String, !t.isEmpty else { continue }
            let g = (o["group"] as? String).flatMap { TodoGroup.all.contains($0) ? $0 : nil }
            out.append((k, t, g, o["done"] as? Bool ?? false))
        }
        return out
    }
    /// Items shown under group headers (Running, Waiting on you, Discussed) and ticked off by a click: a To do
    /// island, or any list where an item names its group. Ungrouped items there count as discussed.
    public var groupedItems: [(group: String, items: [(index: Int, title: String, done: Bool)])]? {
        let all = todoItems
        guard preset == "todo" || all.contains(where: { $0.group != nil }) else { return nil }
        var out: [(group: String, items: [(index: Int, title: String, done: Bool)])] = []
        for g in TodoGroup.all {
            var rows: [(index: Int, title: String, done: Bool)] = []
            for it in all where (it.group ?? "discussed") == g { rows.append((it.index, it.title, it.done)) }
            if !rows.isEmpty { out.append((g, rows)) }
        }
        return out
    }
    /// A To do item still open and waiting on the user: the island's symbol turns orange.
    public var waitsOnOwner: Bool { todoItems.contains { $0.group == "waiting" && !$0.done } }
    /// A few key values in order: [{"label", "value"}].
    public var facts: [(label: String, value: String)] {
        (raw["kv"] as? [JSONObject] ?? []).compactMap { o in
            guard let l = o["label"] as? String else { return nil }
            let v = (o["value"] as? String) ?? (o["value"] as? NSNumber).map { Self.format($0.doubleValue) } ?? ""
            return (l, v)
        }
    }
    /// Times the CLI records: created, last change of what it shows, finished (done or failed).
    public var started: Date? { Self.date(raw["started"]) }
    public var updated: Date? { Self.date(raw["updated"]) }
    public var ended: Date? { Self.date(raw["ended"]) }

    static func date(_ v: Any?) -> Date? {
        if let n = v as? NSNumber { return Date(timeIntervalSince1970: n.doubleValue) }
        if let s = v as? String { return ISO8601DateFormatter().date(from: s) }
        return nil
    }
    public var expanded: Bool { raw["expanded"] as? Bool ?? false }
    /// Hidden islands keep their file (and state) but are not on screen.
    public var hidden: Bool { raw["hidden"] as? Bool ?? false }
    /// Place in its dock, lower first. Plan islands default to To do 0, Scheduled 1, Settled 2.
    public var order: Double? { (raw["order"] as? NSNumber)?.doubleValue ?? list.map(PlanList.order) }
    public var width: Double? { (raw["width"] as? NSNumber)?.doubleValue }
    public var chosen: String? { raw["chosen"] as? String }
    public var parts: [JSONObject] { raw["parts"] as? [JSONObject] ?? [] }

    /// Where the user or an agent placed it freely: the island's top-left corner in points from the display's top-left
    /// corner (x right, y down). nil means it sits in a dock (`side`).
    public var position: (x: Double, y: Double)? {
        guard let p = raw["position"] as? JSONObject,
              let x = (p["x"] as? NSNumber)?.doubleValue, let y = (p["y"] as? NSNumber)?.doubleValue else { return nil }
        return (x, y)
    }

    /// The height the user pinned it at on its edge (left or right): the island's top in points from the display's top.
    /// nil means it takes its turn in the stack (`order`).
    public var edgeY: Double? {
        guard position == nil, side != "top" else { return nil }
        return (raw["edgeY"] as? NSNumber)?.doubleValue
    }

    /// The dock it lives in when it is not placed freely: "left", "right" or "top". A To do list docks left,
    /// everything else right, unless the file says otherwise.
    public var side: String? {
        if position != nil { return nil }
        if let s = raw["side"] as? String, Dock.sides.contains(s) { return s }
        return preset == "todo" ? "left" : "right"
    }

    /// Where it came from. `{"from": "notch", "attachment": "<id>"}` makes it grow out of the notch, the path a
    /// Heimdall notch attachment dragged out will take; the rest of the file is the same island.
    public var bornFromNotch: Bool { (raw["origin"] as? JSONObject)?["from"] as? String == "notch" }

    /// The look follows the state: a task that is done shows as a result, one waiting shows as waiting.
    public var look: String {
        switch (preset, state) {
        case ("todo", _): return "todo"
        case (_, "failed"): return "failed"
        case (_, "waiting"), ("waiting", _): return state == "done" ? "result" : "waiting"
        case ("task", "done"): return "result"
        default: return preset
        }
    }

    public static func format(_ v: Double) -> String {
        if v == v.rounded() && abs(v) < 1e9 { return String(Int(v)) }
        return String(format: abs(v) >= 100 ? "%.0f" : (abs(v) >= 10 ? "%.1f" : "%.2f"), v)
            .replacingOccurrences(of: #"(\.\d*?)0+$"#, with: "$1", options: .regularExpression)
            .replacingOccurrences(of: #"\.$"#, with: "", options: .regularExpression)
    }
}

public enum Step {
    public static let marks = ["done", "now", "failed", "todo"]
    public static func mark(_ s: String?) -> String {
        switch s?.lowercased() {
        case "done", "ok", "finished": return "done"
        case "now", "current", "running", "active": return "now"
        case "failed", "error": return "failed"
        default: return "todo"
        }
    }
    /// "Download=done" → ("Download", "done"); a suffix that is not a mark stays part of the title.
    public static func parse(_ s: String) -> (title: String, mark: String) {
        if let eq = s.lastIndex(of: "=") {
            let tail = String(s[s.index(after: eq)...]).trimmingCharacters(in: .whitespaces).lowercased()
            if ["done", "ok", "finished", "now", "current", "running", "active", "failed", "error", "todo"].contains(tail) {
                return (String(s[..<eq]).trimmingCharacters(in: .whitespaces), mark(tail))
            }
        }
        return (s.trimmingCharacters(in: .whitespaces), "todo")
    }
}

public enum OpenTarget {
    /// A URL for an open target: a URL with a scheme, an absolute or ~ path, or a bare domain (https).
    public static func url(_ t: String) -> URL? {
        let t = t.trimmingCharacters(in: .whitespacesAndNewlines)
        if t.isEmpty { return nil }
        if t.contains("://") || t.hasPrefix("mailto:") { return URL(string: t) }
        if t.hasPrefix("/") || t.hasPrefix("~") { return URL(fileURLWithPath: (t as NSString).expandingTildeInPath) }
        if !t.contains(" "), t.contains("."), let u = URL(string: "https://" + t), u.host != nil { return u }
        return nil
    }
    /// What the button says: the file's name or the page's host.
    public static func label(_ t: String) -> String {
        guard let u = url(t) else { return t }
        if u.isFileURL { return u.lastPathComponent }
        return (u.host ?? t).replacingOccurrences(of: "www.", with: "")
    }
    /// A file target that is not there (the button shows it is missing instead of doing nothing).
    public static func missing(_ t: String) -> Bool {
        guard let u = url(t) else { return true }
        return u.isFileURL && !FileManager.default.fileExists(atPath: u.path)
    }
}

/// Automatic slots: a column at the display's right edge that starts below the band where a browser's tabs and
/// toolbar live (`top`), stacking downward; a full column continues to its left. Placed islands in the way are
/// stepped past. Display-local, y down (origin top-left of a display `width` wide); returns each island's rect.
public func autoColumn(sizes: [CGSize], width: CGFloat, top: CGFloat, bottom: CGFloat, margin: CGFloat = 8,
                       gap: CGFloat = 8, obstacles: [CGRect] = []) -> [CGRect] {
    var right = width - margin, y = top, colWidth: CGFloat = 0
    var out: [CGRect] = []
    for s in sizes {
        var r = CGRect(x: right - s.width, y: y, width: s.width, height: s.height)
        for _ in 0..<64 {
            r = CGRect(x: right - s.width, y: y, width: s.width, height: s.height)
            if r.maxY > bottom, y > top, right - colWidth - gap - s.width >= margin {
                right -= colWidth + gap; colWidth = 0; y = top; continue   // full: the next column to the left
            }
            if let o = obstacles.first(where: { $0.insetBy(dx: -gap / 2, dy: -gap / 2).intersects(r) }) { y = o.maxY + gap; continue }
            break
        }
        out.append(r)
        y = r.maxY + gap
        colWidth = max(colWidth, s.width)
    }
    return out
}

/// Where a dropped island settles. Placement is free; within `reach` it is pulled to the row under the menu bar, a
/// screen edge or corner, or a neighbour (side by side, stacked, aligned), a gentle suggestion rather than a grid.
/// It never ends in the menu bar strip or on top of a neighbour. Rects are y-up screen rects; `rowTop` is the highest an island's top may be.
public func snap(_ r: CGRect, screen: CGRect, visible: CGRect, rowTop: CGFloat, neighbours: [CGRect],
                 margin m: CGFloat = 8, reach: CGFloat = 14) -> CGRect {
    let w = r.width, h = r.height
    func nearest(_ v: CGFloat, _ cands: [CGFloat]) -> CGFloat? {
        cands.filter { abs($0 - v) <= reach }.min { abs($0 - v) < abs($1 - v) }
    }
    // Vertical (the top edge): the row, the bottom edge, and neighbours' tops and stacking positions.
    var yc: [CGFloat] = [rowTop, visible.minY + m + h]
    for o in neighbours where o.maxX > r.minX - w && o.minX < r.maxX + w {
        yc += [o.maxY, o.minY - m, o.maxY + m + h]
    }
    var top = nearest(r.maxY, yc) ?? r.maxY
    top = min(top, rowTop)
    top = max(top, visible.minY + m + h)
    let y = top - h
    // Horizontal: screen edges, beside neighbours on the same line, aligned with neighbours stacked above or below.
    var xc: [CGFloat] = [screen.minX + m, screen.maxX - m - w, screen.midX - w / 2]
    for o in neighbours {
        let sameLine = o.minY < top && o.maxY > y
        let stacked = abs(o.minY - m - top) < 1 || abs(o.maxY + m - y) < 1
        if sameLine { xc += [o.maxX + m, o.minX - m - w] }
        if stacked { xc += [o.minX, o.maxX - w, o.midX - w / 2] }
    }
    var x = nearest(r.minX, xc) ?? r.minX
    x = min(max(x, screen.minX + m), screen.maxX - m - w)
    // Dropped onto a neighbour: move beside it, to whichever side is nearer.
    for _ in 0..<4 {
        guard let o = neighbours.first(where: { $0.intersects(CGRect(x: x, y: y, width: w, height: h).insetBy(dx: 1, dy: 1)) }) else { break }
        let sides = [o.maxX + m, o.minX - m - w].filter { $0 >= screen.minX + m && $0 <= screen.maxX - m - w }
        guard let s = sides.min(by: { abs($0 - x) < abs($1 - x) }) else { break }
        x = s
    }
    return CGRect(x: x.rounded(), y: y.rounded(), width: w, height: h)
}

/// The left dock: the right column mirrored, its first column 8 pt in from the left edge, a full column continuing
/// to its right. Same coordinates as `autoColumn`.
public func leftColumn(sizes: [CGSize], width: CGFloat, top: CGFloat, bottom: CGFloat, margin: CGFloat = 8,
                       gap: CGFloat = 8, obstacles: [CGRect] = []) -> [CGRect] {
    let mirror = { (r: CGRect) in CGRect(x: width - r.maxX, y: r.minY, width: r.width, height: r.height) }
    return autoColumn(sizes: sizes, width: width, top: top, bottom: bottom, margin: margin, gap: gap,
                      obstacles: obstacles.map(mirror)).map(mirror)
}

/// The top dock: a row under the menu bar centred on the notch (`centre`), islands side by side 8 pt apart in
/// order; a row wider than the display continues in a row below. Display-local, y down.
public func topRow(sizes: [CGSize], width: CGFloat, top: CGFloat, centre: CGFloat? = nil, margin: CGFloat = 8, gap: CGFloat = 8) -> [CGRect] {
    let mid = centre ?? width / 2
    var rows: [[Int]] = [[]]
    var used: CGFloat = 0
    for (k, s) in sizes.enumerated() {
        let need = (rows[rows.count - 1].isEmpty ? 0 : gap) + s.width
        if !rows[rows.count - 1].isEmpty && used + need > width - 2 * margin { rows.append([]); used = 0 }
        used += (rows[rows.count - 1].isEmpty ? 0 : gap) + s.width
        rows[rows.count - 1].append(k)
    }
    var out = Array(repeating: CGRect.zero, count: sizes.count)
    var y = top
    for row in rows where !row.isEmpty {
        let total = row.map { sizes[$0].width }.reduce(0, +) + gap * CGFloat(row.count - 1)
        var x = min(max(margin, (mid - total / 2).rounded()), width - margin - total)
        var h: CGFloat = 0
        for k in row {
            out[k] = CGRect(x: x, y: y, width: sizes[k].width, height: sizes[k].height)
            x += sizes[k].width + gap
            h = max(h, sizes[k].height)
        }
        y += h + gap
    }
    return out
}

/// One edge column (the right edge; `leftEdgeColumn` mirrors it). Islands pinned at a height (`pins[k]`, their top)
/// keep it; the rest stack in order from `top`, 8 pt apart, stepping past those level with or above their slot. Everything is then swept top to
/// bottom by wanted height, so an island that overlaps the one above it moves down just below it (an open island
/// pushes the ones under it down),
/// then swept back up from `bottom` if the column overflows. Pins never go above `highest` (under the menu bar) or
/// below `bottom`. Free-placed islands in the way (`obstacles`) are stepped past. Display-local, y down.
public func edgeColumn(sizes: [CGSize], pins: [CGFloat?], width: CGFloat, top: CGFloat, highest: CGFloat, bottom: CGFloat,
                       margin: CGFloat = 8, gap: CGFloat = 8, obstacles: [CGRect] = []) -> [CGRect] {
    var want = Array(repeating: CGFloat(0), count: sizes.count)
    for k in sizes.indices { if let p = pins[k] { want[k] = min(max(p, highest), bottom - sizes[k].height) } }
    // The rest take their turns by their pill height, stepping past pinned islands; an open one then pushes down
    // whatever is under it in the sweep below.
    let pill = sizes.map(\.height).min() ?? 0
    let pinned = sizes.indices.filter { pins[$0] != nil }.map { CGRect(x: 0, y: want[$0], width: 1, height: sizes[$0].height) }
    var y = top
    for k in sizes.indices where pins[k] == nil {
        for _ in 0..<32 {
            let slot = CGRect(x: 0, y: y, width: 1, height: pill)
            // Only past a pin level with or above its slot: one lower down moves under it in the sweep instead.
            guard let o = pinned.first(where: { $0.insetBy(dx: 0, dy: -gap / 2).intersects(slot) && $0.midY <= slot.midY }) else { break }
            y = o.maxY + gap
        }
        want[k] = y
        y += sizes[k].height + gap
    }
    // Pinned first on a tie: the place the user chose wins over a turn in the stack.
    let order = sizes.indices.sorted { (want[$0], pins[$0] == nil ? 1 : 0, $0) < (want[$1], pins[$1] == nil ? 1 : 0, $1) }
    var out = Array(repeating: CGRect.zero, count: sizes.count)
    var floor = -CGFloat.infinity
    for k in order {
        let s = sizes[k]
        var r = CGRect(x: width - margin - s.width, y: max(want[k], floor), width: s.width, height: s.height)
        for _ in 0..<32 {
            guard let o = obstacles.first(where: { $0.insetBy(dx: -gap / 2, dy: -gap / 2).intersects(r) }) else { break }
            r.origin.y = o.maxY + gap
        }
        out[k] = r
        floor = r.maxY + gap
    }
    // Overflow: sweep back up from the bottom, never above the highest place.
    var ceiling = bottom
    for k in order.reversed() {
        if out[k].maxY > ceiling { out[k].origin.y = max(highest, ceiling - out[k].height) }
        ceiling = out[k].minY - gap
    }
    return out
}

/// The left edge column: `edgeColumn` mirrored, 8 pt in from the left edge.
public func leftEdgeColumn(sizes: [CGSize], pins: [CGFloat?], width: CGFloat, top: CGFloat, highest: CGFloat, bottom: CGFloat,
                           margin: CGFloat = 8, gap: CGFloat = 8, obstacles: [CGRect] = []) -> [CGRect] {
    let mirror = { (r: CGRect) in CGRect(x: width - r.maxX, y: r.minY, width: r.width, height: r.height) }
    return edgeColumn(sizes: sizes, pins: pins, width: width, top: top, highest: highest, bottom: bottom, margin: margin, gap: gap,
                      obstacles: obstacles.map(mirror)).map(mirror)
}

/// Where a released island goes, PiP style: always to the left or right edge, at the height it lands. Rects are
/// y-up screen rects; `velocity` is points per second (y up). The release is projected along the throw as a scroll
/// view decelerates (rate 0.998 per ms: about half a second of travel) when it is a flick (`flickSpeed` or faster);
/// a slow release lands where it was let go. The side is the edge nearer to the projected centre; `top` is the
/// island's top, y up, clamped between `highest` (under the menu bar) and the bottom of the visible area.
public func edgeDrop(_ r: CGRect, velocity v: CGVector, screen: CGRect, highest: CGFloat, lowest: CGFloat,
                     flickSpeed: CGFloat = 500) -> (side: String, top: CGFloat) {
    let k: CGFloat = 0.998 / (1 - 0.998) / 1000
    let p = hypot(v.dx, v.dy) >= flickSpeed ? r.offsetBy(dx: v.dx * k, dy: v.dy * k) : r
    let side = p.midX < screen.midX ? "left" : "right"
    let top = min(max(p.maxY, lowest + r.height), highest)
    return (side, top.rounded())
}
