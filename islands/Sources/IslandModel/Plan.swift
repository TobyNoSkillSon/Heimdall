// Plan islands (To do, Scheduled, Settled), their items, attention and the responses a click writes.
// Shared by the app, the CLI and the tests; pure functions only (no AppKit).
import Foundation

/// The three plan lists, docked left in this order by default. Each is one island with `preset: "todo"` and
/// `list` naming which one it is; the island id is the list name.
public enum PlanList {
    public static let all = ["todo", "scheduled", "settled"]
    public static func title(_ l: String) -> String {
        switch l { case "scheduled": return "Scheduled"; case "settled": return "Settled"; default: return "To do" }
    }
    public static func icon(_ l: String) -> String {
        switch l { case "scheduled": return "calendar"; case "settled": return "checkmark.seal"; default: return "checklist" }
    }
    public static func order(_ l: String) -> Double { Double(all.firstIndex(of: l) ?? 0) }
    /// A fresh plan island: what the CLI (and Rust) write when the list does not exist yet. Settled stays a pill.
    public static func newIsland(_ l: String) -> JSONObject {
        ["id": l, "preset": "todo", "list": l, "title": title(l), "icon": icon(l), "items": [Any]()]
    }
}

/// An approval proposal on a waiting item: the text and its buttons (Yes / No / Later unless the file says otherwise).
public struct Proposal: Equatable {
    public let text: String
    public let actions: [(label: String, action: String)]
    public static let defaultActions: [(label: String, action: String)] = [("Yes", "yes"), ("No", "no"), ("Later", "later")]

    public init?(_ v: Any?) {
        if let s = v as? String, !s.isEmpty { text = s; actions = Self.defaultActions; return }
        guard let o = v as? JSONObject else { return nil }
        text = o["text"] as? String ?? ""
        let a = (o["actions"] as? [JSONObject] ?? []).compactMap { a -> (String, String)? in
            guard let l = a["label"] as? String, !l.isEmpty else { return nil }
            return (l, a["action"] as? String ?? l.lowercased())
        }
        actions = a.isEmpty ? Self.defaultActions : a
        if text.isEmpty && (o["actions"] == nil) { return nil }
    }

    public static func == (a: Proposal, b: Proposal) -> Bool {
        a.text == b.text && a.actions.map(\.action) == b.actions.map(\.action) && a.actions.map(\.label) == b.actions.map(\.label)
    }
}

/// One plan item as the app shows it.
public struct PlanItem: Equatable {
    /// Place in the file's `items` array.
    public let index: Int
    /// Stable identity for diffing and responses: the file's `id`, else the title (made unique with "#2", "#3").
    public let key: String
    /// The id the agent wrote (nil when it left it out).
    public let id: String?
    public let title: String
    public let group: String?
    public let done: Bool
    public let when: String?
    /// When it is due: epoch seconds (Scheduled items; the app counts down to it and moves the item to To do).
    public let due: Double?
    public let detail: String?
    public let proposal: Proposal?
    /// The approval the user picked (written by the app when he clicks; Rust then moves the item).
    public let chosen: String?
    /// The item's own SF Symbol (`icon`), else one is picked from its title (`symbol(list:)`).
    public var icon: String? = nil

    /// The small symbol beside the title: the item's own `icon`, else the first `Plan.symbols` word in its project
    /// prefix, then in its title, else one for its kind (waiting on the user, discussed, running, scheduled, settled).
    public func symbol(list: String) -> String {
        if let icon { return icon }
        let title = self.title.lowercased()
        let prefix = title.contains(":") ? String(title[..<title.firstIndex(of: ":")!]) : ""
        // A word matches its key exactly or as a plural or compound ("voices", "speakers", "acks", "trash-rule").
        func hit(_ s: String) -> String? {
            let ws = s.split(whereSeparator: { !($0.isLetter || $0.isNumber) }).map(String.init)
            return Plan.symbols.first { k, _ in ws.contains { $0 == k || ($0.hasSuffix("s") && String($0.dropLast()) == k) } }?.symbol
        }
        if let s = hit(prefix) ?? hit(title) { return s }
        if list == "scheduled" { return "clock" }
        if list == "settled" { return "checkmark" }
        switch group {
        case "waiting": return "hand.raised"
        case "running": return "gearshape"
        default: return "bubble.left.and.bubble.right"
        }
    }

    /// Whether a click on the row opens it: there is more than the title to read or answer.
    public var discloses: Bool { detail != nil || (proposal != nil && !done) }
}

public enum Plan {
    /// Words that name a project or kind, and the small symbol a plan row shows for them. The title's project prefix
    /// ("Echo guard: …") is matched first, then the whole title; the first entry that matches wins. Rust
    /// `PLAN_SYMBOLS` holds the same table.
    public static let symbols: [(word: String, symbol: String)] = [
        ("heimdall", "eye"), ("echo", "speaker.wave.2"), ("ack", "bubble.left"), ("jarvis", "person.wave.2"),
        ("voice", "waveform"), ("speaker", "person.2.wave.2"), ("vella", "text.quote"), ("verdict", "checkmark.shield"),
        ("vireo", "bird"), ("arsenal", "shippingbox"), ("decider", "arrow.triangle.branch"), ("model", "cpu"),
        ("bench", "chart.bar"), ("training", "chart.line.uptrend.xyaxis"), ("islands", "capsule"), ("menu", "menubar.rectangle"),
        ("iphone", "iphone"), ("ipad", "ipad"), ("mac", "laptopcomputer"), ("screen", "display"), ("vault", "archivebox"),
        ("trash", "trash"), ("cache", "trash"), ("night", "moon.stars"), ("agent", "sparkles"), ("plan", "list.bullet"),
        ("scheduled", "calendar"), ("bug", "ladybug"), ("failed", "arrow.clockwise"), ("stream", "dot.radiowaves.left.and.right"),
        ("brand", "star"), ("warsaw", "mappin.and.ellipse"), ("event", "calendar"), ("install", "square.and.arrow.down"),
        ("download", "arrow.down.circle"), ("release", "flag"), ("test", "testtube.2"), ("image", "photo"), ("photo", "photo"),
        ("site", "globe"),
    ]

    /// Items in file order. Strings are plain items; objects need a title.
    public static func items(_ raw: JSONObject) -> [PlanItem] {
        var out: [PlanItem] = []
        var seen: [String: Int] = [:]
        func unique(_ k: String) -> String {
            let n = (seen[k] ?? 0) + 1
            seen[k] = n
            return n == 1 ? k : "\(k)#\(n)"
        }
        for (k, v) in (raw["items"] as? [Any] ?? []).enumerated() {
            if let s = v as? String {
                if !s.isEmpty { out.append(PlanItem(index: k, key: unique(s), id: nil, title: s, group: nil, done: false, when: nil, due: nil, detail: nil, proposal: nil, chosen: nil)) }
                continue
            }
            guard let o = v as? JSONObject, let t = o["title"] as? String, !t.isEmpty else { continue }
            let id = (o["id"] as? String).flatMap { $0.isEmpty ? nil : $0 } ?? (o["id"] as? NSNumber).map { "\($0)" }
            let g = (o["group"] as? String).flatMap { TodoGroup.all.contains($0) ? $0 : nil }
            out.append(PlanItem(index: k, key: unique(id ?? t), id: id, title: t, group: g, done: o["done"] as? Bool ?? false,
                                when: (o["when"] as? String).flatMap { $0.isEmpty ? nil : $0 },
                                due: Due.seconds(o["due"]),
                                detail: (o["detail"] as? String).flatMap { $0.isEmpty ? nil : $0 },
                                proposal: Proposal(o["proposal"]), chosen: o["chosen"] as? String,
                                icon: (o["icon"] as? String).flatMap { $0.isEmpty ? nil : $0 }))
        }
        return out
    }

    /// The groups a list shows, in order. To do: Running, Waiting on you, Discussed (ungrouped items count as
    /// discussed). Scheduled and Settled: one group without a header (`""`). Within a group open items keep their
    /// file order and ticked ones sink to the bottom, in file order.
    public static func grouped(_ items: [PlanItem], list: String) -> [(group: String, items: [PlanItem])] {
        let sink = { (rows: [PlanItem]) in rows.filter { !$0.done } + rows.filter(\.done) }
        if list != "todo" { return items.isEmpty ? [] : [("", sink(items))] }
        return TodoGroup.all.compactMap { g in
            let rows = items.filter { ($0.group ?? "discussed") == g }
            return rows.isEmpty ? nil : (g, sink(rows))
        }
    }

    /// Keys in display order (what the CLI numbers and the app draws top to bottom).
    public static func displayOrder(_ raw: JSONObject, list: String) -> [PlanItem] {
        grouped(items(raw), list: list).flatMap(\.items)
    }

    /// A stable id for a new item: its title as a short slug, made unique among `taken`.
    public static func slug(_ title: String, taken: Set<String>) -> String {
        var s = String(title.lowercased().map { $0.isLetter || $0.isNumber ? $0 : "-" })
        while s.contains("--") { s = s.replacingOccurrences(of: "--", with: "-") }
        s = String(s.trimmingCharacters(in: CharacterSet(charactersIn: "-")).prefix(40)).trimmingCharacters(in: CharacterSet(charactersIn: "-"))
        if s.isEmpty { s = "item" }
        var out = s, n = 2
        while taken.contains(out) { out = "\(s)-\(n)"; n += 1 }
        return out
    }
}

/// What changed between two renders of a list, by key: drives the row animations.
public struct ItemDiff: Equatable {
    public var added: [String] = []
    public var removed: [String] = []
    /// Kept rows whose place in the display order changed (moved up or down, or to another group).
    public var moved: [String] = []
    /// Kept rows ticked off or back on.
    public var ticked: [String] = []
    public var untouched: [String] = []

    public init(old: [PlanItem], new: [PlanItem]) {
        let oldKeys = old.map(\.key), newKeys = new.map(\.key)
        let o = Dictionary(old.map { ($0.key, $0) }, uniquingKeysWith: { a, _ in a })
        let oldSet = Set(oldKeys), newSet = Set(newKeys)
        removed = oldKeys.filter { !newSet.contains($0) }
        added = newKeys.filter { !oldSet.contains($0) }
        // Relative order among the rows both renders share, leaving out ticked rows (a row sinking past others does
        // not make them "moved"): a row moved when its group or its place among the rest changed.
        let n = Dictionary(new.map { ($0.key, $0) }, uniquingKeysWith: { a, _ in a })
        let tickedSet = Set(newKeys.filter { k in o[k].map { $0.done != n[k]!.done } ?? false })
        let keptOld = oldKeys.filter { newSet.contains($0) && !tickedSet.contains($0) }
        let keptNew = newKeys.filter { oldSet.contains($0) && !tickedSet.contains($0) }
        let posOld = Dictionary(keptOld.enumerated().map { ($1, $0) }, uniquingKeysWith: { a, _ in a })
        let posNew = Dictionary(keptNew.enumerated().map { ($1, $0) }, uniquingKeysWith: { a, _ in a })
        for key in newKeys where oldSet.contains(key) {
            guard let was = o[key], let now = n[key] else { continue }
            if tickedSet.contains(key) { ticked.append(key) }
            else if posOld[key] != posNew[key] || (was.group ?? "discussed") != (now.group ?? "discussed") { moved.append(key) }
            else { untouched.append(key) }
        }
    }
    public init() {}
}

public enum Attention {
    public static let kinds = ["waiting", "question"]
    /// The attention an island asks for: its own `attention` key, or `waiting` for a plan island with an open
    /// waiting item (derived, never written).
    public static func of(_ i: Island) -> String? {
        if let a = i.raw["attention"] as? String, kinds.contains(a) { return a }
        if i.preset == "todo", i.waitsOnOwner { return "waiting" }
        return nil
    }
    /// What the user has seen once he opens the island: the same kind with the same open waiting items and question
    /// buttons does not pulse again; a new one does.
    public static func signature(_ i: Island) -> String? {
        guard let kind = of(i) else { return nil }
        let waiting = Plan.items(i.raw).filter { $0.group == "waiting" && !$0.done }.map(\.key).sorted()
        let actions = i.parts.compactMap { $0["action"] as? String }
        return ([kind] + waiting + actions).joined(separator: "|")
    }
    /// Seconds between two pulses while attention is set and the island is closed.
    public static let interval: Double = 6
}

/// The response objects a click writes (`IslandFile.respond` adds `at`).
public enum Responses {
    /// The user ticked a plan item off (`done`) or back on (`undo`).
    public static func tick(island: String, item: String, done: Bool) -> JSONObject {
        ["island": island, "kind": "tick", "action": done ? "done" : "undo", "value": item]
    }
    /// The user answered an approval: `action` is the button's action ("yes", "no", "later").
    public static func approval(island: String, item: String, action: String) -> JSONObject {
        ["island": island, "kind": "approval", "action": action, "value": item]
    }
}

public enum Lifetime {
    /// `--max-hours N`: hours until the app quits itself; 0 (or less) means no limit (Heimdall supervises the
    /// helper). nil = no limit. Default 24 h; an unreadable value keeps the default.
    public static func maxHours(_ args: [String]) -> Double? {
        var h = 24.0
        if let i = args.firstIndex(of: "--max-hours"), i + 1 < args.count, let v = Double(args[i + 1]) { h = v }
        return h > 0 ? h : nil
    }

    /// `--parent-pid N`: the process (Heimdall) whose exit quits the app, so a helper never outlives a crashed or
    /// killed parent. nil when absent or not a real pid.
    public static func parentPID(_ args: [String]) -> pid_t? {
        guard let i = args.firstIndex(of: "--parent-pid"), i + 1 < args.count, let v = Int32(args[i + 1]), v > 1 else { return nil }
        return v
    }
}
