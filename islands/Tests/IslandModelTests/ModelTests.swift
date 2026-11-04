import XCTest
@testable import IslandModel

final class ModelTests: XCTestCase {
    func testColumnStacksDownTheRightEdge() {
        let r = autoColumn(sizes: [CGSize(width: 100, height: 36), CGSize(width: 140, height: 36)], width: 1000, top: 120, bottom: 900)
        XCTAssertEqual(r[0], CGRect(x: 892, y: 120, width: 100, height: 36))
        XCTAssertEqual(r[1], CGRect(x: 852, y: 164, width: 140, height: 36))   // right-aligned, 8 pt below
    }

    func testColumnContinuesToTheLeftWhenFull() {
        let s = Array(repeating: CGSize(width: 100, height: 36), count: 3)
        let r = autoColumn(sizes: s, width: 1000, top: 100, bottom: 190)
        XCTAssertEqual(r.map(\.minX), [892, 892, 784])
        XCTAssertEqual(r[2].minY, 100)
    }

    func testColumnStepsPastPlacedIslands() {
        // The user put something where the second slot would be: it goes under it.
        let placed = CGRect(x: 850, y: 160, width: 150, height: 120)
        let r = autoColumn(sizes: [CGSize(width: 100, height: 36), CGSize(width: 100, height: 36)], width: 1000, top: 120, bottom: 900, obstacles: [placed])
        XCTAssertEqual(r[0].minY, 120 - 0)
        XCTAssertFalse(r[0].intersects(placed))
        XCTAssertEqual(r[1].minY, 288)
    }

    func testStepsAndTargetsParse() {
        XCTAssertEqual(Step.parse("Download=done").title, "Download")
        XCTAssertEqual(Step.parse("Download=done").mark, "done")
        XCTAssertEqual(Step.parse("Convert = now").mark, "now")
        XCTAssertEqual(Step.parse("a=b").title, "a=b")
        XCTAssertEqual(Step.parse("Install").mark, "todo")
        XCTAssertEqual(OpenTarget.url("https://example.com/x")?.host, "example.com")
        XCTAssertTrue(OpenTarget.url("/tmp/compare3.html")?.isFileURL ?? false)
        XCTAssertEqual(OpenTarget.url("example.com")?.absoluteString, "https://example.com")
        XCTAssertEqual(OpenTarget.label("/tmp/compare3.html"), "compare3.html")
        XCTAssertEqual(OpenTarget.label("https://www.example.com/a"), "example.com")
        XCTAssertNil(Island(id: "x", raw: ["open": ""]).open)
        XCTAssertTrue(OpenTarget.missing("/nonexistent/file.html"))
    }

    let screen = CGRect(x: 0, y: 0, width: 1728, height: 1117)
    let visible = CGRect(x: 0, y: 70, width: 1728, height: 1015)

    func testSnapNeverEntersTheMenuBar() {
        let r = snap(CGRect(x: 600, y: 1100, width: 120, height: 28), screen: screen, visible: visible, rowTop: 1079, neighbours: [])
        XCTAssertEqual(r.maxY, 1079)
    }

    func testSnapToCornerAndNeighbour() {
        let corner = snap(CGRect(x: 1590, y: 90, width: 120, height: 28), screen: screen, visible: visible, rowTop: 1079, neighbours: [])
        XCTAssertEqual(corner.origin, CGPoint(x: 1600, y: 78))
        let n = CGRect(x: 300, y: 1051, width: 100, height: 28)
        let beside = snap(CGRect(x: 420, y: 1040, width: 120, height: 28), screen: screen, visible: visible, rowTop: 1079, neighbours: [n])
        XCTAssertEqual(beside.origin, CGPoint(x: 408, y: 1051))
        let below = snap(CGRect(x: 310, y: 1005, width: 80, height: 28), screen: screen, visible: visible, rowTop: 1079, neighbours: [n])
        XCTAssertEqual(below.origin, CGPoint(x: 310, y: 1015))   // centred under it
    }

    func testSnapNeverDropsOntoANeighbour() {
        let n = CGRect(x: 600, y: 600, width: 300, height: 28)
        let r = snap(CGRect(x: 700, y: 600, width: 100, height: 28), screen: screen, visible: visible, rowTop: 1079, neighbours: [n])
        XCTAssertFalse(r.intersects(n.insetBy(dx: 1, dy: 1)))
        XCTAssertEqual(r.minX, 908)
    }

    func testSnapLeavesFreePlacementsAlone() {
        let r = CGRect(x: 700, y: 500, width: 120, height: 28)
        XCTAssertEqual(snap(r, screen: screen, visible: visible, rowTop: 1079, neighbours: []), r)
    }

    func testLookFollowsState() {
        XCTAssertEqual(Island(id: "a", raw: ["preset": "task", "state": "done"]).look, "result")
        XCTAssertEqual(Island(id: "a", raw: ["preset": "task", "state": "waiting"]).look, "waiting")
        XCTAssertEqual(Island(id: "a", raw: ["preset": "list", "state": "failed"]).look, "failed")
        XCTAssertEqual(Island(id: "a", raw: ["preset": "waiting", "state": "done"]).look, "result")
        XCTAssertEqual(Island(id: "a", raw: ["preset": "bogus"]).look, "task")
        XCTAssertEqual(Island(id: "a", raw: ["preset": "note"]).look, "note")
    }

    func testMergeRemovesNullAndKeepsPosition() {
        let base: JSONObject = ["title": "A", "position": ["x": 1, "y": 2], "progress": 0.5]
        let m = IslandFile.merge(base, ["progress": NSNull(), "title": "B"])
        XCTAssertNil(m["progress"])
        XCTAssertEqual(m["title"] as? String, "B")
        XCTAssertNotNil(Island(id: "a", raw: m).position)
    }

    func testMalformedValuesAreClamped() {
        let i = Island(id: "a", raw: ["progress": 7, "position": ["x": "no"], "parts": "nope"])
        XCTAssertEqual(i.progress, 1)
        XCTAssertNil(i.position)
        XCTAssertTrue(i.parts.isEmpty)
        XCTAssertEqual(Island.format(2.10), "2.1")
        XCTAssertEqual(Island.format(412), "412")
    }

    func testSafeIDCannotEscapeTheFolder() {
        XCTAssertFalse(Paths.safeID("../../etc/passwd").contains("/"))
    }

    func testPlainItemsKeepOrderAndSkipBlanks() {
        let i = Island(id: "a", raw: ["items": ["One", "", ["title": "Two"], 3, "Three"]])
        XCTAssertEqual(i.items, ["One", "Two", "Three"])
    }

    func testOpenIslandPushesTheColumnDown() {
        let pill = CGSize(width: 120, height: 36), open = CGSize(width: 360, height: 240)
        let closed = autoColumn(sizes: [pill, pill, pill, pill], width: 1500, top: 100, bottom: 900)
        let opened = autoColumn(sizes: [pill, open, pill, pill], width: 1500, top: 100, bottom: 900)
        XCTAssertEqual(opened[2].minY - closed[2].minY, open.height - pill.height)
        for k in 0..<3 { XCTAssertFalse(opened[k].intersects(opened[k + 1])) }
    }

    // Docks and throws (y-up screen rects for dropDock; display-local y-down for the stacks).

    func testLeftColumnMirrorsTheRight() {
        let r = leftColumn(sizes: [CGSize(width: 100, height: 36), CGSize(width: 140, height: 36)], width: 1000, top: 120, bottom: 900)
        XCTAssertEqual(r[0], CGRect(x: 8, y: 120, width: 100, height: 36))
        XCTAssertEqual(r[1], CGRect(x: 8, y: 164, width: 140, height: 36))   // left-aligned, 8 pt below
    }

    func testLeftColumnPushesDownWhenOneOpens() {
        let closed = leftColumn(sizes: [CGSize(width: 100, height: 36), CGSize(width: 100, height: 36)], width: 1000, top: 120, bottom: 900)
        let open = leftColumn(sizes: [CGSize(width: 360, height: 300), CGSize(width: 100, height: 36)], width: 1000, top: 120, bottom: 900)
        XCTAssertEqual(closed[1].minY, 164)
        XCTAssertEqual(open[1].minY, 428)
        XCTAssertFalse(open[0].intersects(open[1]))
    }

    func testTopRowIsCentredOnTheNotchAndNeverOverlaps() {
        let r = topRow(sizes: [CGSize(width: 100, height: 36), CGSize(width: 140, height: 36)], width: 1728, top: 44)
        XCTAssertEqual(r[0], CGRect(x: 740, y: 44, width: 100, height: 36))
        XCTAssertEqual(r[1], CGRect(x: 848, y: 44, width: 140, height: 36))
        let wide = topRow(sizes: Array(repeating: CGSize(width: 500, height: 36), count: 4), width: 1728, top: 44)
        XCTAssertEqual(wide[3].minY, 88)   // a full row continues below
        for a in wide.indices { for b in wide.indices where a < b { XCTAssertFalse(wide[a].intersects(wide[b])) } }
    }

    let scr = CGRect(x: 0, y: 0, width: 1728, height: 1117)
    let row: CGFloat = 1073

    func testTodoItemsGroupAndTick() {
        XCTAssertEqual(TodoGroup.parse("Echo bug=waiting").group, "waiting")
        XCTAssertTrue(TodoGroup.parse("Echo bug=done").done)
        XCTAssertEqual(TodoGroup.parse("Arsenal: a=b").title, "Arsenal: a=b")
        let i = Island(id: "open", raw: ["preset": "todo", "items": ["Plain", ["title": "Port", "group": "running"], ["title": "Test", "group": "waiting", "done": true]]])
        XCTAssertEqual(i.groupedItems?.map(\.group), ["running", "waiting", "discussed"])
        XCTAssertEqual(i.groupedItems?.last?.items.first?.title, "Plain")
        XCTAssertEqual(i.side, "left")
        XCTAssertFalse(i.waitsOnOwner)   // the only waiting item is ticked off
        XCTAssertNil(Island(id: "x", raw: ["items": ["a", "b"]]).groupedItems)   // a plain list stays plain
        XCTAssertNil(Island(id: "x", raw: ["position": ["x": 10, "y": 20], "side": "left"]).side)
    }

    func testEdgeColumnKeepsPinnedHeightsAndFlowsAround() {
        let s = Array(repeating: CGSize(width: 100, height: 36), count: 4)
        // Third island pinned at 140: the auto ones before it step past, the one after continues below.
        let r = edgeColumn(sizes: s, pins: [nil, nil, 140, nil], width: 1000, top: 120, highest: 40, bottom: 900)
        XCTAssertEqual(r.map(\.minY), [120, 208, 164, 252])   // the pin sits right under the island above it
        // A pin level with or above a slot: the stack steps past it and the pin keeps its height.
        let p = edgeColumn(sizes: s, pins: [nil, nil, 110, nil], width: 1000, top: 120, highest: 40, bottom: 900)
        XCTAssertEqual(p.map(\.minY), [154, 198, 110, 242])
        for a in 0..<4 { for b in 0..<4 where a < b { XCTAssertFalse(r[a].insetBy(dx: 1, dy: 1).intersects(r[b]), "\(a) \(b)") } }
        XCTAssertTrue(r.allSatisfy { $0.maxX == 992 })
        // Pins clamp to the screen: under the menu bar, above the bottom.
        let c = edgeColumn(sizes: [s[0], s[0]], pins: [0, 2000], width: 1000, top: 120, highest: 40, bottom: 900)
        XCTAssertEqual(c[0].minY, 40); XCTAssertEqual(c[1].maxY, 900)
        // Two pinned at the same height: no overlap.
        let d = edgeColumn(sizes: [s[0], s[0]], pins: [500, 500], width: 1000, top: 120, highest: 40, bottom: 900)
        XCTAssertEqual(d[0].minY, 500); XCTAssertEqual(d[1].minY, 544)
        // An open island grows over a pinned one below: it moves down; overflow at the bottom sweeps back up.
        let e = edgeColumn(sizes: [CGSize(width: 300, height: 400), s[0]], pins: [nil, 300], width: 1000, top: 120, highest: 40, bottom: 900)
        XCTAssertEqual(e[1].minY, 528)
        let f = edgeColumn(sizes: [s[0], s[0]], pins: [880, 880], width: 1000, top: 120, highest: 40, bottom: 900)
        XCTAssertEqual(f[1].maxY, 900); XCTAssertEqual(f[0].maxY, 856)
    }

    func testLeftEdgeColumnMirrors() {
        let r = leftEdgeColumn(sizes: [CGSize(width: 100, height: 36)], pins: [300], width: 1000, top: 120, highest: 40, bottom: 900)
        XCTAssertEqual(r[0], CGRect(x: 8, y: 300, width: 100, height: 36))
    }

    func testEdgeDropGoesToTheNearerSideAtItsHeight() {
        let r = CGRect(x: 700, y: 500, width: 100, height: 36)
        let slow = edgeDrop(r, velocity: .zero, screen: screen, highest: 1079, lowest: 78)
        XCTAssertEqual(slow.side, "left"); XCTAssertEqual(slow.top, 536)
        let right = edgeDrop(r.offsetBy(dx: 300, dy: 0), velocity: .zero, screen: screen, highest: 1079, lowest: 78)
        XCTAssertEqual(right.side, "right")
        // A flick carries on along its path: thrown right and up, it lands right and higher, never in the menu bar.
        let flick = edgeDrop(r, velocity: CGVector(dx: 2000, dy: 200), screen: screen, highest: 1079, lowest: 78)
        XCTAssertEqual(flick.side, "right"); XCTAssertEqual(flick.top, 636, accuracy: 1)
        XCTAssertEqual(edgeDrop(r, velocity: CGVector(dx: 0, dy: 5000), screen: screen, highest: 1079, lowest: 78).top, 1079)
        XCTAssertEqual(edgeDrop(r, velocity: CGVector(dx: 0, dy: -5000), screen: screen, highest: 1079, lowest: 78).top, 114)
    }
}
