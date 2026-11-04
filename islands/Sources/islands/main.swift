// `islands` — writes the island files the app watches. Every command prints one short line.
import Foundation
import IslandModel

let putUsage = """
islands put <id> [options]   Create or update one island in one call (merges; a place User chose is kept).
Words follow docs/island-style.md; put refuses text that breaks the checkable rules.
  --title T                  1-3 words naming the thing: "Qwen-Image", "iPad sale". Never the id. Needed for a new island.
  --state S                  running (working in the background) | waiting (needs User) | done | failed
  --stage WORDS              1-2 words, the present activity: "Downloading", "Final tests", "Your answer"
  --summary TEXT             one plain line: what it is and what is next
  --step "TEXT[=done|now|failed]"   a sequence, once per step in order (replaces the list; --step none clears it)
  --item "TEXT[=running|waiting|discussed|done]"   a plain list, once per item (replaces it; --item none clears it).
                             With a group, the list shows Running / Waiting on you / Discussed headers and ticks.
  --kv "Key=Value"           a few key values (replaces them; --kv none clears them)
  --progress 0..1|none       only a live measured value (a ring and %)
  --image PATH               a picture shown when it is opened
  --open URL|PATH            a button that opens the page or file in the default browser or app
  --icon SF.Symbol  --value V (a result's headline)  --preset task|waiting|result|list|note|viewer|todo
  --parts JSON  --json JSON|-  --expanded | --collapsed  --from-notch
Where:
  --side left|right|top      dock to that edge's stack (top: the row under the notch); default right, To do left
  --order N                  place in its stack (lower first)
  --at X,Y                   place freely: its top-left corner in screen points from the display's top-left (y down)
  --auto                     back to its default dock
  Without these, put keeps the place User chose (an island he threw keeps its edge and height).
Example: islands put img --title "Qwen-Image" --state running --stage Downloading --icon photo \\
           --summary "20 GB of weights, then convert and install" --step "Download=now" --step "Convert" --step "Install"
"""

let planUsage = """
islands plan                              All three plan lists (left dock): To do, Scheduled, Settled, numbered.
islands plan [todo|scheduled|settled] list
islands plan LIST add "TEXT" [--running|--waiting|--discussed] [--due TIME] [--when W] [--detail T] [--proposal TEXT] [--item-id ID]
                                          Add an item (To do default: discussed; --proposal makes it a waiting approval
                                          with Yes / No / Later). Each item gets a stable id (a slug of its text).
islands plan LIST done|undo|rm N|ID|"TEXT"...   Tick off / back on / remove (N from the listing, an id, or a unique part).
islands plan LIST move N|ID|"TEXT" [--running|--waiting|--discussed] [--to todo|scheduled|settled] [--due TIME] [--when W]
islands plan LIST prune                   Remove ticked items.
LIST defaults to todo. Discussed = talked about, no decision yet; Scheduled = agreed for later or tonight, with a
due time (--due "2026-10-24 10:00" | "tomorrow 10:00" | "today 22:00" | tonight | +2h | +30m | +3d): the app counts
down to it and moves the item to To do (Waiting on you) when it is due; Settled = done or agreed.
"""

let todoUsage = """
islands todo [list]                       The To do island (left dock), numbered: Running, Waiting on you, Discussed.
islands todo add "TEXT" [--running|--waiting|--discussed]   Add an item (default discussed).
islands todo done|undo N|"TEXT"...        Tick off / back on (N from `islands todo`, or a unique part of the text).
islands todo move N|"TEXT" --running|--waiting|--discussed
islands todo rm N|"TEXT"...  |  islands todo prune   (remove ticked items)
  --id ID                                 another To do island (default: the To do list, else "todo")
Same as `islands plan todo …` (see islands plan --help for Scheduled and Settled).
"""

let usage = """
\(putUsage)
\(planUsage)
\(todoUsage)
islands view <path> [--id ID] [--title T] [--action "Label=action"]...   Show any file large in the middle of the screen
                 (markdown, text/code, image, PDF, audio, video, HTML, else QuickLook); --action adds proposed-action buttons on top.
islands open|close <id>...              Open it / back to its pill (aliases expand|collapse, pin|unpin).
islands hide|show <id>...               Take it off screen / bring it back, keeping its file and place.
islands move <id> X Y | <id> --side left|right|top | <id> --auto
                                        X,Y: top-left corner in screen points from the display's top-left.
islands rm <id>... | --all              Remove islands.
islands clear                           Remove every island and every pending response.
islands list [--json]                   Islands with where they are now: dock or placed, top-left x,y in screen points, size.
islands state [<id>] [--json]           What is on screen: open, side, x, y (top-left, screen points), width, height.
islands quit                            Quit the app (islands vanish; files stay unless cleared).
islands responses [--consume] [--json]  Show clicks User made (and delete them with --consume).
islands dir                             Print the data folder.
"""

func fail(_ s: String) -> Never { FileHandle.standardError.write((s + "\n").data(using: .utf8)!); exit(2) }

func parseJSON(_ s: String) -> Any {
    guard let d = s.data(using: .utf8), let v = try? JSONSerialization.jsonObject(with: d, options: [.fragmentsAllowed]) else {
        fail("islands: invalid JSON: \(s.prefix(80))")
    }
    return v
}

func words(_ s: String) -> Int { s.split(whereSeparator: { $0.isWhitespace }).count }

/// The checkable half of docs/island-style.md, applied to what this call sets (old files are left alone).
func styleProblem(_ o: JSONObject, over: JSONObject, id: String, new: Bool) -> String? {
    if new && (o["title"] as? String ?? "").isEmpty && o["preset"] as? String != "viewer" {
        return "a new island needs --title: 1-3 words naming the thing (\"Qwen-Image\", \"iPad sale\")"
    }
    if let t = over["title"] as? String {
        let n = words(t)
        if n < 1 || n > 3 { return "--title is 1-3 words naming the thing; \"\(t)\" has \(n)" }
        let norm = { (s: String) in s.lowercased().replacingOccurrences(of: "_", with: "-").replacingOccurrences(of: " ", with: "-") }
        if t == t.lowercased() && norm(t) == norm(id) {
            return "--title names the thing for User, not an id: \"\(t)\""
        }
    }
    if let s = over["stage"] as? String, words(s) > 2 { return "--stage is one or two present-tense words (\"Downloading\", \"Final tests\"); \"\(s)\" has \(words(s))" }
    if let s = over["summary"] as? String {
        if s.contains("\n") { return "--summary is one line" }
        if s.count > 110 { return "--summary is one plain line (110 characters at most); this one has \(s.count)" }
    }
    return nil
}

func summary(_ o: JSONObject, id: String) -> String {
    let i = Island(id: id, raw: o)
    var bits = [i.preset, i.state]
    if let p = i.progress, i.state == "running" { bits.append("\(Int(p * 100))%") }
    if let v = i.value { bits.append(v) }
    if let pos = i.position { bits.append("placed at \(Island.format(pos.x)),\(Island.format(pos.y))") } else if let side = i.side { bits.append("dock \(side)" + (i.edgeY.map { " at y \(Island.format($0))" } ?? "")) }
    if i.expanded { bits.append("open") }
    return "\(id) · \(i.title) · " + bits.joined(separator: " ")
}

func screenState() -> JSONObject { (IslandFile.read(Paths.state)?["islands"] as? JSONObject) ?? [:] }

func stateLine(_ id: String) -> String {
    guard let s = screenState()[id] as? JSONObject else { return " · not on screen" }
    let n = { (k: String) in Island.format(((s[k] as? NSNumber)?.doubleValue ?? 0).rounded()) }
    return " · \((s["open"] as? Bool) == true ? "open" : "pill") at \(n("x")),\(n("y")) \(n("width"))×\(n("height"))"
}

func setFlags(_ ids: [String], _ verb: String, _ change: [String: Any]) {
    guard !ids.isEmpty else { fail("islands \(verb) <id>...") }
    for id in ids {
        guard let o = IslandFile.patch(id, { o in for (k, v) in change { o[k] = v } }) else { fail("islands \(verb): no island \(id)") }
        print("\(verb) " + summary(o, id: id))
    }
}

/// An absolute path for a file the agent named relative to where it runs.
func absolute(_ t: String) -> String {
    if t.hasPrefix("/") || t.hasPrefix("~") || t.contains("://") { return (t as NSString).expandingTildeInPath }
    return URL(fileURLWithPath: FileManager.default.currentDirectoryPath).appendingPathComponent(t).standardizedFileURL.path
}

/// URLs stay as they are; a file (absolute, ~ or relative to the caller) becomes an absolute path; a bare domain gets https.
func resolveTarget(_ t: String) -> String {
    if t.contains("://") || t.hasPrefix("mailto:") { return t }
    let a = absolute(t)
    if t.hasPrefix("/") || t.hasPrefix("~") || FileManager.default.fileExists(atPath: a) { return a }
    if let u = OpenTarget.url(t), !u.isFileURL { return u.absoluteString }
    return a
}

/// Times for "started 12 min ago · updated 2 min ago": set when it is created, when what it shows changes, when it ends.
func stamp(_ o: inout JSONObject, old: JSONObject?) {
    let now = (Date().timeIntervalSince1970 * 10).rounded() / 10
    if o["started"] == nil { o["started"] = old == nil ? now : nil }
    let shown = ["title", "state", "stage", "progress", "value", "summary", "steps", "items", "kv", "image", "open", "parts", "path", "preset", "icon"]
    let changed = old == nil || shown.contains { k in
        let a = old?[k], b = o[k]
        if a == nil && b == nil { return false }
        guard let a, let b else { return true }
        return !(a as AnyObject).isEqual(b)
    }
    if changed { o["updated"] = now }
    let state = Island(id: "", raw: o).state, was = old.map { Island(id: "", raw: $0).state }
    if state == "done" || state == "failed" { if was != state || o["ended"] == nil { o["ended"] = now } } else { o.removeValue(forKey: "ended") }
}

func allIslands() -> [(String, JSONObject)] {
    let urls = (try? FileManager.default.contentsOfDirectory(at: Paths.islands, includingPropertiesForKeys: nil)) ?? []
    return urls.filter { $0.pathExtension == "json" && !$0.lastPathComponent.hasPrefix(".") }
        .sorted { $0.lastPathComponent < $1.lastPathComponent }
        .compactMap { u in IslandFile.read(u).map { (u.deletingPathExtension().lastPathComponent, $0) } }
}

var args = Array(CommandLine.arguments.dropFirst())
guard let cmd = args.first else { print(usage); exit(0) }
args.removeFirst()
Paths.ensure()

switch cmd {
case "put":
    if args.contains("--help") || args.contains("-h") { print(putUsage); exit(0) }
    guard let id = args.first, !id.hasPrefix("-") else { fail("islands put: missing <id>\n\n\(putUsage)") }
    args.removeFirst()
    var over: JSONObject = [:]
    var steps: [JSONObject] = [], facts: [JSONObject] = [], items: [String] = []
    func itemObject(_ t: String) -> Any {
        let p = TodoGroup.parse(t)
        if p.group == nil && !p.done { return p.title }
        var o: JSONObject = ["title": p.title]
        if let g = p.group { o["group"] = g }
        if p.done { o["done"] = true }
        return o
    }
    var resetSteps = false, resetFacts = false, resetItems = false
    var i = 0
    func next() -> String {
        i += 1
        guard i < args.count else { fail("islands put: \(args[i - 1]) needs a value") }
        return args[i]
    }
    while i < args.count {
        let a = args[i]
        switch a {
        case "--preset":
            let p = next(); guard Presets.all.contains(p) else { fail("islands put: preset must be one of \(Presets.all.joined(separator: ", "))") }
            over["preset"] = p
        case "--title": over["title"] = next()
        case "--side":
            let v = next(); guard Dock.sides.contains(v) else { fail("islands put: --side takes left, right or top") }
            over["side"] = v; over["position"] = NSNull(); over["edgeY"] = NSNull()
        case "--state":
            let s = next(); guard Presets.states.contains(s) else { fail("islands put: state must be one of \(Presets.states.joined(separator: ", "))") }
            over["state"] = s
        case "--progress":
            let v = next()
            if v == "none" { over["progress"] = NSNull() } else if let d = Double(v) { over["progress"] = d } else { fail("islands put: --progress takes 0..1 or none") }
        case "--value": over["value"] = next()
        case "--stage": over["stage"] = next()
        case "--path": over["path"] = next()
        case "--icon": over["icon"] = next()
        case "--open":
            let t = next()
            over["open"] = t.isEmpty ? NSNull() : resolveTarget(t)
        case "--summary": let t = next(); over["summary"] = t.isEmpty ? NSNull() : t
        case "--image":
            let t = next()
            over["image"] = t.isEmpty ? NSNull() : absolute(t)
        case "--step":
            let t = next()
            if !resetSteps { resetSteps = true; steps = [] }
            if t != "none" && !t.isEmpty { let p = Step.parse(t); steps.append(["title": p.title, "state": p.mark]) }
            over["steps"] = steps.isEmpty ? NSNull() : steps
        case "--item":
            let t = next()
            if !resetItems { resetItems = true; items = [] }
            if t != "none" && !t.isEmpty { items.append(t) }
            over["items"] = items.isEmpty ? NSNull() : items.map(itemObject)
        case "--kv":
            let t = next()
            if !resetFacts { resetFacts = true; facts = [] }
            if t != "none" && !t.isEmpty {
                guard let eq = t.firstIndex(of: "=") else { fail("islands put: --kv takes Key=Value") }
                facts.append(["label": String(t[..<eq]).trimmingCharacters(in: .whitespaces),
                              "value": String(t[t.index(after: eq)...]).trimmingCharacters(in: .whitespaces)])
            }
            over["kv"] = facts.isEmpty ? NSNull() : facts
        case "--parts":
            guard let p = parseJSON(next()) as? [Any] else { fail("islands put: --parts takes a JSON array") }
            over["parts"] = p
        case "--at":
            let xy = next().split(separator: ",").compactMap { Double($0.trimmingCharacters(in: .whitespaces)) }
            guard xy.count == 2 else { fail("islands put: --at takes X,Y") }
            over["position"] = ["x": xy[0], "y": xy[1]]; over["side"] = NSNull(); over["edgeY"] = NSNull()
        case "--auto": over["position"] = NSNull(); over["side"] = NSNull(); over["edgeY"] = NSNull()
        case "--expanded": over["expanded"] = true
        case "--collapsed": over["expanded"] = false
        case "--order": guard let d = Double(next()) else { fail("islands put: --order takes a number") }; over["order"] = d; over["edgeY"] = NSNull()
        case "--from-notch": over["origin"] = ["from": "notch"]
        case "--json", "-":
            let text = a == "-" ? String(data: FileHandle.standardInput.readDataToEndOfFile(), encoding: .utf8) ?? "" : next()
            guard let o = parseJSON(text) as? JSONObject else { fail("islands put: --json takes a JSON object") }
            for (k, v) in o { over[k] = v }   // keep nulls: they delete keys in the final merge
        default: fail("islands put: unknown option \(a)\n\n\(putUsage)")
        }
        i += 1
    }
    let url = Paths.file(id)
    let out: JSONObject = IslandFile.locked {
        let old = IslandFile.read(url)
        var out = IslandFile.merge(old ?? ["id": id], over)
        out["id"] = id
        if let problem = styleProblem(out, over: over, id: id, new: old == nil) { fail("islands put: \(problem)\nSee docs/island-style.md or islands put --help.") }
        stamp(&out, old: old)
        if let p = out["preset"] as? String, !Presets.all.contains(p) { fail("islands put: unknown preset \(p)") }
        do { try IslandFile.writeAtomic(out, to: url) } catch { fail("islands put: \(error.localizedDescription)") }
        return out
    }
    print("put " + summary(out, id: id))

case "move":
    guard args.count >= 2 else { fail("islands move <id> X Y | <id> --side left|right|top | <id> --auto") }
    let id = args[0]
    let done = IslandFile.patch(id) { o in
        o.removeValue(forKey: "edgeY")
        if args[1] == "--auto" { o.removeValue(forKey: "position"); o.removeValue(forKey: "side"); return }
        if args[1] == "--side" {
            guard args.count >= 3, Dock.sides.contains(args[2]) else { fail("islands move <id> --side left|right|top") }
            o.removeValue(forKey: "position"); o["side"] = args[2]; return
        }
        guard args.count >= 3, let x = Double(args[1]), let y = Double(args[2]) else { fail("islands move <id> X Y") }
        o["position"] = ["x": x, "y": y]
        o.removeValue(forKey: "side")
    }
    guard let done else { fail("islands move: no island \(id)") }
    print("moved " + summary(done, id: id))

case "view":
    guard let path = args.first, !path.hasPrefix("-") else { fail("islands view <path> [--id ID] [--title T] [--action \"Label=action\"]...") }
    let abs = URL(fileURLWithPath: (path as NSString).expandingTildeInPath).standardizedFileURL.path
    guard FileManager.default.fileExists(atPath: abs) else { fail("islands view: no file \(path)") }
    func opt(_ k: String) -> String? { args.firstIndex(of: k).flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil } }
    let id = opt("--id") ?? "view-" + Paths.safeID((abs as NSString).lastPathComponent)
    let o: JSONObject = IslandFile.locked {
        var o = IslandFile.read(Paths.file(id)) ?? [:]
        o = IslandFile.merge(o, ["id": id, "preset": "viewer", "path": abs, "title": opt("--title") ?? ((abs as NSString).lastPathComponent as NSString).deletingPathExtension,
                                 "expanded": true, "hidden": false])
        // Proposed actions (viewer): each --action replaces the list; none given keeps what is there.
        let actions = args.indices.filter { args[$0] == "--action" && $0 + 1 < args.count }.compactMap { ViewerAction.fromCLI(args[$0 + 1]) }
        if !actions.isEmpty { o["actions"] = actions.map(\.json) }
        try? IslandFile.writeAtomic(o, to: Paths.file(id))
        return o
    }
    print("view " + summary(o, id: id))

case "open", "expand", "pin": setFlags(args, cmd, ["expanded": true, "hidden": false])
case "close", "collapse", "unpin": setFlags(args, cmd, ["expanded": false])
case "hide": setFlags(args, cmd, ["hidden": true])
case "show": setFlags(args, cmd, ["hidden": false])

case "clear":
    var n = 0
    for (id, _) in allIslands() where (try? FileManager.default.removeItem(at: Paths.file(id))) != nil { n += 1 }
    for u in (try? FileManager.default.contentsOfDirectory(at: Paths.responses, includingPropertiesForKeys: nil)) ?? [] { try? FileManager.default.removeItem(at: u) }
    print("cleared \(n) island\(n == 1 ? "" : "s")")

case "state":
    let st = screenState()
    let pick = args.first { !$0.hasPrefix("-") }
    let sel: Any = pick.map { st[$0] ?? NSNull() } ?? st
    if args.contains("--json") {
        let d = try! JSONSerialization.data(withJSONObject: sel, options: [.sortedKeys, .withoutEscapingSlashes, .fragmentsAllowed])
        print(String(data: d, encoding: .utf8)!)
    } else {
        let ids = pick.map { [$0] } ?? st.keys.sorted()
        if ids.isEmpty { print("nothing on screen") }
        for id in ids { print(id + stateLine(id)) }
    }

case "quit":
    guard let s = try? String(contentsOf: Paths.pid, encoding: .utf8), let pid = Int32(s.trimmingCharacters(in: .whitespacesAndNewlines)),
          kill(pid, 0) == 0 else { print("app not running"); exit(0) }
    kill(pid, SIGTERM)
    for _ in 0..<100 where kill(pid, 0) == 0 { usleep(50_000) }
    print(kill(pid, 0) == 0 ? "app still running (pid \(pid))" : "app quit")

case "rm":
    let ids = args == ["--all"] ? allIslands().map(\.0) : args
    guard !ids.isEmpty || args == ["--all"] else { fail("islands rm <id>... | --all") }
    var n = 0
    for id in ids where (try? FileManager.default.removeItem(at: Paths.file(id))) != nil { n += 1 }
    print("removed \(n) island\(n == 1 ? "" : "s")")

case "list":
    let all = allIslands()
    if args.contains("--json") {
        let d = try! JSONSerialization.data(withJSONObject: all.map(\.1), options: [.sortedKeys, .withoutEscapingSlashes])
        print(String(data: d, encoding: .utf8)!)
    } else if all.isEmpty {
        print("no islands")
    } else {
        for (id, o) in all { print(summary(o, id: id) + stateLine(id)) }
    }

case "responses":
    let urls = ((try? FileManager.default.contentsOfDirectory(at: Paths.responses, includingPropertiesForKeys: nil)) ?? [])
        .filter { $0.pathExtension == "json" && !$0.lastPathComponent.hasPrefix(".") }
        .sorted { $0.lastPathComponent < $1.lastPathComponent }
    let items = urls.compactMap { IslandFile.read($0) }
    if args.contains("--json") {
        let d = try! JSONSerialization.data(withJSONObject: items, options: [.sortedKeys, .withoutEscapingSlashes])
        print(String(data: d, encoding: .utf8)!)
    } else if items.isEmpty {
        print("no responses")
    } else {
        for o in items {
            print("\(o["island"] ?? "?") · \(o["kind"] ?? "?") · \(o["action"] ?? o["value"] ?? "")")
        }
    }
    if args.contains("--consume") { for u in urls { try? FileManager.default.removeItem(at: u) } }

case "todo", "plan":
    // `islands plan [todo|scheduled|settled] VERB …`; `islands todo VERB …` is the To do list (with --id, another one).
    if args.contains("--help") || args.contains("-h") { print(cmd == "plan" ? planUsage : todoUsage); exit(0) }
    var rest = args
    func take(_ flag: String) -> String? {
        guard let k = rest.firstIndex(of: flag) else { return nil }
        guard k + 1 < rest.count else { fail("islands \(cmd): \(flag) needs a value") }
        let v = rest[k + 1]
        rest.removeSubrange(k...k + 1)
        return v
    }
    let tid = take("--id"), when = take("--when"), detail = take("--detail"), proposal = take("--proposal"), itemID = take("--item-id"), toList = take("--to"), dueText = take("--due")
    let due: Double? = dueText.map { t -> Double in
        guard let d = Due.parse(t) else { fail("islands \(cmd): --due \"\(t)\" is not a time (\"2026-10-24 10:00\", \"tomorrow 10:00\", +2h)") }
        return d
    }
    if let toList, !PlanList.all.contains(toList) { fail("islands \(cmd): --to takes todo, scheduled or settled") }
    let groupFlag = rest.first { ["--running", "--waiting", "--discussed"].contains($0) }.map { String($0.dropFirst(2)) }
    rest.removeAll { $0.hasPrefix("--") }
    var list = "todo"
    if cmd == "plan", let first = rest.first, PlanList.all.contains(first) { list = first; rest.removeFirst() }
    let verb = rest.first ?? "list"
    let operands = Array(rest.dropFirst())
    let id: String = {
        if let tid { return tid }
        if cmd == "plan" { return list }
        // The old To do island (any preset todo file that is the To do list), else "todo".
        return allIslands().first { $0.1["preset"] as? String == "todo" && ($0.1["list"] as? String ?? "todo") == "todo" }?.0 ?? "todo"
    }()
    func objects(_ o: JSONObject?) -> [JSONObject] {
        (o?["items"] as? [Any] ?? []).compactMap { v in (v as? JSONObject) ?? (v as? String).map { ["title": $0] } }
    }
    /// N (from the listing), an item id, the exact title, or a unique part of it → index in the file's items.
    func resolve(_ ref: String, _ o: JSONObject) -> Int {
        let l = Island(id: id, raw: o).list ?? "todo"
        let shown = Plan.displayOrder(o, list: l)
        if let n = Int(ref) { guard n >= 1 && n <= shown.count else { fail("islands \(cmd): no item \(n)") }; return shown[n - 1].index }
        let all = Plan.items(o)
        if let it = all.first(where: { $0.id == ref || $0.key == ref }) { return it.index }
        if let it = all.first(where: { $0.title == ref }) { return it.index }
        let hits = all.filter { $0.title.localizedCaseInsensitiveContains(ref) }
        guard hits.count == 1 else { fail("islands \(cmd): \(hits.isEmpty ? "no item matches" : "several items match") \"\(ref)\"") }
        return hits[0].index
    }
    func fresh(_ id: String, _ list: String) -> JSONObject {
        if cmd == "todo" && tid != nil { return ["id": id, "title": "To do", "preset": "todo", "icon": "checklist"] }
        var o = PlanList.newIsland(list); o["id"] = id
        return o
    }
    func printList(_ id: String) {
        let o = IslandFile.read(Paths.file(id)) ?? [:]
        let l = Island(id: id, raw: o).list ?? "todo"
        let shown = Plan.displayOrder(o, list: l)
        print("\(PlanList.title(l)) (\(id))" + (shown.isEmpty ? " · nothing here" : ""))
        var last: String?
        for (n, it) in shown.enumerated() {
            let g = l == "todo" ? (it.group ?? "discussed") : ""
            if l == "todo", g != last { print("  " + TodoGroup.label(g)); last = g }
            var line = "  \(n + 1). \(it.done ? "[x]" : "[ ]") \(it.title)"
            if let d = it.due { line += " · \(Due.label(d, now: Date()).text)" } else if let w = it.when { line += " · \(w)" }
            if it.proposal != nil { line += it.chosen.map { " · chose \($0)" } ?? " · proposal" }
            if let i = it.id { line += "  (\(i))" }
            print(line)
        }
    }
    if verb == "list" {
        if cmd == "plan" && args.first.map({ !PlanList.all.contains($0) }) ?? true { PlanList.all.forEach(printList) } else { printList(id) }
        exit(0)
    }
    guard ["add", "done", "undo", "move", "rm", "prune"].contains(verb) else { fail(cmd == "plan" ? planUsage : todoUsage) }
    if verb != "prune" && operands.isEmpty { fail(cmd == "plan" ? planUsage : todoUsage) }
    if verb == "move" && groupFlag == nil && toList == nil && when == nil && due == nil { fail("islands \(cmd) move N|ID|\"TEXT\" --running|--waiting|--discussed | --to todo|scheduled|settled [--when W]") }
    var said = ""
    let out: JSONObject = IslandFile.locked {
        let old = IslandFile.read(Paths.file(id))
        var o = old ?? fresh(id, list)
        var items = objects(old)
        let taken = Set(Plan.items(o).compactMap(\.id))
        switch verb {
        case "add":
            var ids = taken
            for t in operands where !t.isEmpty {
                var it: JSONObject = ["title": t]
                let iid = itemID ?? Plan.slug(t, taken: ids)
                ids.insert(iid)
                it["id"] = iid
                if (o["list"] as? String ?? "todo") == "todo" || groupFlag != nil { it["group"] = groupFlag ?? "discussed" }
                if let when { it["when"] = when }
                if let due { it["due"] = due; it.removeValue(forKey: "when") }
                if let detail { it["detail"] = detail }
                if let proposal { it["proposal"] = ["text": proposal]; if groupFlag == nil { it["group"] = "waiting" } }
                items.append(it)
            }
            said = "added \(operands.count)"
        case "done", "undo":
            for k in operands.map({ resolve($0, o) }) { if verb == "done" { items[k]["done"] = true } else { items[k].removeValue(forKey: "done") } }
            said = verb == "done" ? "ticked \(operands.count)" : "unticked \(operands.count)"
        case "move":
            let ks = operands.map { resolve($0, o) }
            for k in ks {
                if let groupFlag { items[k]["group"] = groupFlag }
                // A new word without a date drops an old due time (it would win on screen or move the item back).
                if let when { items[k]["when"] = when; if due == nil { items[k].removeValue(forKey: "due") } }
                if let due { items[k]["due"] = due; items[k].removeValue(forKey: "when") }
            }
            if let toList, toList != (o["list"] as? String ?? "todo") || tid != nil {
                // To another list: it leaves this island and arrives at the end of the other (same id, same keys).
                let dest = toList
                guard dest != id else { break }
                var d = IslandFile.read(Paths.file(dest)) ?? { var n = PlanList.newIsland(dest); n["id"] = dest; return n }()
                var dItems = objects(d)
                for k in ks {
                    var it = items[k]
                    if dest == "todo" { it["group"] = groupFlag ?? (it["group"] as? String ?? "discussed") }
                    dItems.append(it)
                }
                d["items"] = dItems
                let dOld = IslandFile.read(Paths.file(dest))
                stamp(&d, old: dOld)
                do { try IslandFile.writeAtomic(d, to: Paths.file(dest)) } catch { fail("islands \(cmd): \(error.localizedDescription)") }
                let set = Set(ks)
                items = items.enumerated().filter { !set.contains($0.offset) }.map(\.element)
                said = "moved \(ks.count) to \(PlanList.title(dest))"
            } else {
                said = "moved \(ks.count)" + (groupFlag.map { " to \(TodoGroup.label($0))" } ?? "") + (when.map { " (\($0))" } ?? "")
            }
        case "rm":
            let ks = Set(operands.map { resolve($0, o) })
            items = items.enumerated().filter { !ks.contains($0.offset) }.map(\.element)
            said = "removed \(ks.count)"
        default:
            let before = items.count
            items.removeAll { $0["done"] as? Bool == true }
            said = "pruned \(before - items.count)"
        }
        o["items"] = items
        stamp(&o, old: old)
        do { try IslandFile.writeAtomic(o, to: Paths.file(id)) } catch { fail("islands \(cmd): \(error.localizedDescription)") }
        return o
    }
    let open = objects(out).filter { $0["done"] as? Bool != true }.count
    print("\(cmd) \(id) · \(said) · \(open) open")

case "dir":
    print(Paths.root.path)

case "-h", "--help", "help":
    print(usage)

default:
    fail("islands: unknown command \(cmd)\n\n\(usage)")
}
