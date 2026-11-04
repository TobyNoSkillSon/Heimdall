import XCTest
@testable import IslandModel

final class PlanTests: XCTestCase {
    func item(_ id: String?, _ title: String, _ group: String? = nil, done: Bool = false, extra: JSONObject = [:]) -> JSONObject {
        var o: JSONObject = ["title": title]
        if let id { o["id"] = id }
        if let group { o["group"] = group }
        if done { o["done"] = true }
        return o.merging(extra) { a, _ in a }
    }

    func testPlanIslandsDockLeftInOrderWithTitlesAndIcons() {
        let islands = PlanList.all.map { Island(id: $0, raw: PlanList.newIsland($0)) }
        XCTAssertEqual(islands.map(\.list), ["todo", "scheduled", "settled"])
        XCTAssertEqual(islands.map(\.side), ["left", "left", "left"])
        XCTAssertEqual(islands.map(\.order), [0, 1, 2])
        XCTAssertEqual(islands.map(\.title), ["To do", "Scheduled", "Settled"])
        XCTAssertEqual(islands.map(\.icon), ["checklist", "calendar", "checkmark.seal"])
        XCTAssertFalse(islands[2].expanded, "Settled is a pill by default")
        // An explicit order or side still wins; a plain To do island without `list` is the To do list.
        XCTAssertEqual(Island(id: "x", raw: ["preset": "todo", "list": "settled", "order": 7]).order, 7)
        XCTAssertEqual(Island(id: "x", raw: ["preset": "todo"]).list, "todo")
        XCTAssertNil(Island(id: "x", raw: ["preset": "task"]).list)
        XCTAssertNil(Island(id: "x", raw: ["preset": "task"]).order)
    }

    func testItemKeysUseIdsAndFallBackToUniqueTitles() {
        let raw: JSONObject = ["items": [item("a", "Arsenal: push branch"), item(nil, "Echo bug"), item(nil, "Echo bug"), "Plain", item("", "Empty id")]]
        XCTAssertEqual(Plan.items(raw).map(\.key), ["a", "Echo bug", "Echo bug#2", "Plain", "Empty id"])
        XCTAssertEqual(Plan.items(raw).map(\.id), ["a", nil, nil, nil, nil])
    }

    func testTickedItemsSinkToTheBottomOfTheirGroup() {
        let raw: JSONObject = ["items": [item("1", "One", "running", done: true), item("2", "Two", "running"), item("3", "Three", "waiting"),
                                         item("4", "Four", "running", done: true), item("5", "Five", "running"), item("6", "Six")]]
        let g = Plan.grouped(Plan.items(raw), list: "todo")
        XCTAssertEqual(g.map(\.group), ["running", "waiting", "discussed"])
        XCTAssertEqual(g[0].items.map(\.key), ["2", "5", "1", "4"])
        XCTAssertEqual(Plan.displayOrder(raw, list: "todo").map(\.key), ["2", "5", "1", "4", "3", "6"])
        // Scheduled and Settled: one group without a header, ticked items last.
        let s = Plan.grouped(Plan.items(raw), list: "scheduled")
        XCTAssertEqual(s.map(\.group), [""])
        XCTAssertEqual(s[0].items.map(\.key), ["2", "3", "5", "6", "1", "4"])
    }

    func testScheduledWhenDetailAndProposalParse() {
        let raw: JSONObject = ["items": [item("w", "Delete old caches", "waiting", extra: ["proposal": ["text": "~/.cache 12 GB\n~/Library/Caches 4 GB"], "when": "Tonight", "detail": "Freed 16 GB"]),
                                         item("c", "Custom", "waiting", extra: ["proposal": ["text": "Pick", "actions": [["label": "Do it", "action": "do"]]]])]]
        let it = Plan.items(raw)
        XCTAssertEqual(it[0].when, "Tonight")
        XCTAssertEqual(it[0].detail, "Freed 16 GB")
        XCTAssertEqual(it[0].proposal?.text, "~/.cache 12 GB\n~/Library/Caches 4 GB")
        XCTAssertEqual(it[0].proposal?.actions.map(\.action), ["yes", "no", "later"], "actions default to Yes / No / Later")
        XCTAssertEqual(it[0].proposal?.actions.map(\.label), ["Yes", "No", "Later"])
        XCTAssertEqual(it[1].proposal?.actions.map(\.action), ["do"])
        XCTAssertNil(Proposal(["nothing": 1]))
        XCTAssertEqual(Proposal("Plain text")?.actions.count, 3)
    }

    func testDiffByIdFindsAddsRemovesMovesAndTicks() {
        let old: JSONObject = ["items": [item("a", "A", "running"), item("b", "B", "running"), item("c", "C", "discussed"), item("d", "D", "discussed")]]
        // b renamed (same id: kept, not re-added), a ticked (sinks), c moved to waiting, d removed, e added.
        let new: JSONObject = ["items": [item("a", "A", "running", done: true), item("b", "B renamed", "running"), item("c", "C", "waiting"), item("e", "E", "discussed")]]
        let d = ItemDiff(old: Plan.displayOrder(old, list: "todo"), new: Plan.displayOrder(new, list: "todo"))
        XCTAssertEqual(d.added, ["e"])
        XCTAssertEqual(d.removed, ["d"])
        XCTAssertEqual(d.ticked, ["a"])
        XCTAssertEqual(d.moved, ["c"])
        XCTAssertEqual(d.untouched, ["b"])
        // Without ids the title is the key: a rename is a remove plus an add.
        let o2 = Plan.items(["items": ["X", "Y"]]), n2 = Plan.items(["items": ["X", "Z"]])
        let d2 = ItemDiff(old: o2, new: n2)
        XCTAssertEqual(d2.added, ["Z"]); XCTAssertEqual(d2.removed, ["Y"]); XCTAssertEqual(d2.untouched, ["X"])
        // Same content, nothing to animate.
        XCTAssertEqual(ItemDiff(old: o2, new: o2).untouched, ["X", "Y"])
    }

    func testAttentionKeyAndDerivedWaiting() {
        XCTAssertEqual(Attention.of(Island(id: "j", raw: ["attention": "question"])), "question")
        XCTAssertEqual(Attention.of(Island(id: "j", raw: ["attention": "waiting"])), "waiting")
        XCTAssertNil(Attention.of(Island(id: "j", raw: ["attention": "loud"])))
        XCTAssertNil(Attention.of(Island(id: "j", raw: [:])))
        let plan: JSONObject = ["preset": "todo", "items": [item("w", "Approve", "waiting")]]
        XCTAssertEqual(Attention.of(Island(id: "todo", raw: plan)), "waiting", "an open waiting item asks for attention")
        let ticked: JSONObject = ["preset": "todo", "items": [item("w", "Approve", "waiting", done: true)]]
        XCTAssertNil(Attention.of(Island(id: "todo", raw: ticked)))
        // A new waiting item changes the signature (pulses again after the user opened it); the same one does not.
        let more: JSONObject = ["preset": "todo", "items": [item("w", "Approve", "waiting"), item("x", "Another", "waiting")]]
        XCTAssertNotEqual(Attention.signature(Island(id: "todo", raw: plan)), Attention.signature(Island(id: "todo", raw: more)))
        XCTAssertEqual(Attention.signature(Island(id: "todo", raw: plan)), Attention.signature(Island(id: "todo", raw: plan.merging(["title": "Renamed"]) { _, b in b })))
        // A new question (new buttons) does too.
        let q1: JSONObject = ["attention": "question", "parts": [["type": "button", "label": "A", "action": "t3q:1:0"]]]
        let q2: JSONObject = ["attention": "question", "parts": [["type": "button", "label": "A", "action": "t3q:2:0"]]]
        XCTAssertNotEqual(Attention.signature(Island(id: "j", raw: q1)), Attention.signature(Island(id: "j", raw: q2)))
    }

    func testResponseFormats() throws {
        XCTAssertEqual(Responses.tick(island: "todo", item: "a", done: true) as NSDictionary,
                       ["island": "todo", "kind": "tick", "action": "done", "value": "a"] as NSDictionary)
        XCTAssertEqual(Responses.tick(island: "todo", item: "a", done: false)["action"] as? String, "undo")
        XCTAssertEqual(Responses.approval(island: "todo", item: "caches", action: "yes") as NSDictionary,
                       ["island": "todo", "kind": "approval", "action": "yes", "value": "caches"] as NSDictionary)
        // Written as a response file the Rust reader consumes.
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("fi-plan-\(getpid())")
        setenv("FLOATING_ISLANDS_DIR", dir.path, 1)
        defer { unsetenv("FLOATING_ISLANDS_DIR"); try? FileManager.default.removeItem(at: dir) }
        let url = try XCTUnwrap(IslandFile.respond(Responses.approval(island: "todo", item: "caches", action: "later")))
        XCTAssertTrue(url.lastPathComponent.hasPrefix("todo-"))
        let o = try XCTUnwrap(IslandFile.read(url))
        XCTAssertEqual(o["kind"] as? String, "approval")
        XCTAssertEqual(o["action"] as? String, "later")
        XCTAssertEqual(o["value"] as? String, "caches")
        XCTAssertNotNil(o["at"])
    }

    /// The user's real settled.json (22 Oct): no `preset`, no `list`, nine plain strings. It is the Settled list.
    func testOldShapePlanIslandsAreKnownById() throws {
        let json = #"{"edgeY":157,"expanded":false,"icon":"checkmark.seal","id":"settled","items":["Islands into Heimdall, bound to threads","Drop replies you talked over","Island closes when message is sent","To do: animations, approvals, Settled group","Arsenal note in my prompt","Arsenal polish list, models, peak memory","Repair OCR and translation models","Simple add-a-model, model folders","Homebrew tap, prepared not published"],"order":9,"side":"left","stage":"Running","state":"running","title":"Scheduled"}"#
        let raw = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(json.utf8)) as? JSONObject)
        let settled = Island(id: "settled", raw: raw)
        XCTAssertEqual(settled.preset, "todo")
        XCTAssertEqual(settled.list, "settled")
        XCTAssertEqual(settled.planItems.count, 9)
        XCTAssertEqual(settled.planItems.first?.title, "Islands into Heimdall, bound to threads")
        XCTAssertEqual(settled.title, "Scheduled", "its own title is kept")
        XCTAssertEqual(Island(id: "todo", raw: [:]).list, "todo")
        XCTAssertEqual(Island(id: "scheduled", raw: ["preset": "todo"]).list, "scheduled")
        XCTAssertEqual(Island(id: "scheduled", raw: ["items": ["A"]]).title, "Scheduled")
        XCTAssertEqual(Island(id: "x", raw: ["preset": "todo", "list": "settled"]).list, "settled", "an explicit list still wins")
    }

    func testParentPID() {
        XCTAssertEqual(Lifetime.parentPID(["--max-hours", "0", "--parent-pid", "4242"]), 4242)
        XCTAssertNil(Lifetime.parentPID([]))
        XCTAssertNil(Lifetime.parentPID(["--parent-pid"]))
        XCTAssertNil(Lifetime.parentPID(["--parent-pid", "soon"]))
        XCTAssertNil(Lifetime.parentPID(["--parent-pid", "1"]), "launchd is never a parent to follow")
        XCTAssertNil(Lifetime.parentPID(["--parent-pid", "-5"]))
    }

    func testMaxHours() {
        XCTAssertEqual(Lifetime.maxHours([]), 24)
        XCTAssertEqual(Lifetime.maxHours(["--max-hours", "2.5"]), 2.5)
        XCTAssertNil(Lifetime.maxHours(["--max-hours", "0"]), "0 means no limit")
        XCTAssertNil(Lifetime.maxHours(["--selftest", "--max-hours", "-1"]))
        XCTAssertEqual(Lifetime.maxHours(["--max-hours", "soon"]), 24)
        XCTAssertEqual(Lifetime.maxHours(["--max-hours"]), 24)
    }

    func testSlugIdsAreStableAndUnique() {
        XCTAssertEqual(Plan.slug("Arsenal: push branch", taken: []), "arsenal-push-branch")
        XCTAssertEqual(Plan.slug("Arsenal: push branch", taken: ["arsenal-push-branch"]), "arsenal-push-branch-2")
        XCTAssertEqual(Plan.slug("!!!", taken: []), "item")
        XCTAssertLessThanOrEqual(Plan.slug(String(repeating: "word ", count: 30), taken: []).count, 40)
    }

    func testARowSymbolComesFromItsOwnIconThenItsProjectThenItsKind() {
        let raw: JSONObject = ["items": [item("a", "Echo guard: keep raw audio in archive", "waiting"), item("b", "Voices: pick one by ear", "waiting"),
                                         item("c", "Stream the Arsenal builds", "discussed"),
                                         item("d", "Mac: empty the Trash", "waiting", extra: ["icon": "trash.circle"]),
                                         item("e", "Pick a release date", "waiting"), item("f", "Something new")]]
        let i = Plan.items(raw)
        // The project prefix wins over words later in the title; plurals match; the item's own icon wins over all.
        XCTAssertEqual(i.map { $0.symbol(list: "todo") }, ["speaker.wave.2", "waveform", "shippingbox", "trash.circle", "flag", "bubble.left.and.bubble.right"])
        XCTAssertEqual(i[5].symbol(list: "scheduled"), "clock")
        XCTAssertEqual(i[5].symbol(list: "settled"), "checkmark")
        XCTAssertEqual(Plan.items(["items": [item("g", "Nothing known", "waiting")]])[0].symbol(list: "todo"), "hand.raised")
    }

    func testARowOpensOnlyWhenItHasMoreThanItsTitle() {
        let raw: JSONObject = ["items": [item("a", "Plain"), item("b", "Explained", extra: ["detail": "Why and what next"]),
                                         item("c", "Asks", "waiting", extra: ["proposal": ["text": "Delete?"]]),
                                         item("d", "Answered and ticked", "waiting", done: true, extra: ["proposal": ["text": "Delete?"]])]]
        XCTAssertEqual(Plan.items(raw).map(\.discloses), [false, true, true, false])
    }
}
