import XCTest
@testable import IslandModel

final class DueTests: XCTestCase {
    var cal: Calendar = {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "Europe/Warsaw")!
        return c
    }()
    /// Local wall-clock time in Warsaw.
    func at(_ y: Int, _ mo: Int, _ d: Int, _ h: Int, _ mi: Int, _ s: Int = 0) -> Date {
        cal.date(from: DateComponents(year: y, month: mo, day: d, hour: h, minute: mi, second: s))!
    }
    func label(_ due: Date, _ now: Date) -> String { Due.label(due.timeIntervalSince1970, now: now, calendar: cal, locale: Locale(identifier: "en_GB")).text }

    func testLabelsCountDownInTheReadersUnits() {
        let now = at(2026, 10, 23, 9, 0)
        XCTAssertEqual(label(at(2026, 10, 23, 9, 12), now), "in 12 min")
        XCTAssertEqual(label(at(2026, 10, 23, 9, 0, 20), now), "in 1 min")
        XCTAssertEqual(label(at(2026, 10, 23, 9, 59, 30), now), "in 1 h", "59.5 min rounds up to the hour")
        XCTAssertEqual(label(at(2026, 10, 23, 12, 10), now), "in 3 h")
        XCTAssertEqual(label(at(2026, 10, 23, 23, 0), now), "in 14 h")
        XCTAssertEqual(label(at(2026, 10, 24, 10, 0), now), "tomorrow 10:00")
        XCTAssertEqual(label(at(2026, 10, 25, 8, 0), now), "in 2 days")
        XCTAssertEqual(label(at(2026, 10, 29, 8, 0), now), "in 6 days")
        XCTAssertEqual(label(at(2026, 10, 30, 8, 0), now), "30 Oct")
        XCTAssertEqual(label(at(2026, 10, 23, 8, 0), now), "now")
        XCTAssertTrue(Due.label(at(2026, 10, 23, 9, 30).timeIntervalSince1970, now: now, calendar: cal).soon)
        XCTAssertFalse(Due.label(at(2026, 10, 23, 12, 0).timeIntervalSince1970, now: now, calendar: cal).soon)
        // Just before midnight, an item due early tomorrow is "tomorrow 06:00", and under an hour counts minutes.
        XCTAssertEqual(label(at(2026, 10, 24, 6, 0), at(2026, 10, 23, 23, 50)), "tomorrow 06:00")
        XCTAssertEqual(label(at(2026, 10, 24, 0, 20), at(2026, 10, 23, 23, 50)), "in 30 min")
    }

    func testNextChangeIsExactlyWhenTheLabelChanges() {
        // Walk a day in one-minute steps: the label never changes before nextChange, and does change at it.
        let due = at(2026, 10, 25, 7, 30).timeIntervalSince1970
        var now = at(2026, 10, 23, 18, 0, 17)
        var steps = 0
        while now.timeIntervalSince1970 < due {
            steps += 1
            guard steps < 500 else { return XCTFail("no progress at \(now)") }
            let next = Due.nextChange(due, now: now, calendar: cal)
            XCTAssertGreaterThan(next, now)
            let before = next.addingTimeInterval(-0.5)
            if before > now { XCTAssertEqual(label(Date(timeIntervalSince1970: due), before), label(Date(timeIntervalSince1970: due), now), "\(now) → \(next)") }
            XCTAssertNotEqual(label(Date(timeIntervalSince1970: due), next.addingTimeInterval(0.01)), label(Date(timeIntervalSince1970: due), now), "\(now) → \(next)")
            now = next.addingTimeInterval(0.01)
        }
        XCTAssertEqual(Due.nextChange(due, now: Date(timeIntervalSince1970: due + 1), calendar: cal), .distantFuture)
    }

    func testTheTimerSleepsLongWhenNothingIsNear() {
        let now = at(2026, 10, 23, 9, 0)
        // Due next week: next change at midnight, not every minute.
        XCTAssertEqual(Due.nextChange(at(2026, 10, 30, 9, 0).timeIntervalSince1970, now: now, calendar: cal), at(2026, 10, 24, 0, 0))
        // Due in 5 h: next change when the rounded hour changes (in 4.5 h → 4 h).
        XCTAssertEqual(Due.nextChange(at(2026, 10, 23, 14, 0).timeIntervalSince1970, now: now, calendar: cal), at(2026, 10, 23, 9, 30))
    }

    func testDueItemsMoveToToDoWaitingKeepingTheirKeys() {
        let now = at(2026, 10, 23, 10, 0).timeIntervalSince1970
        let sched: JSONObject = ["id": "scheduled", "items": [
            ["id": "a", "title": "Arsenal: push", "due": now - 60, "when": "Fri", "detail": "d", "thread": "t1"],
            ["id": "b", "title": "Later", "due": now + 3600],
            ["id": "c", "title": "Ticked", "due": now - 60, "done": true],
            ["title": "No id", "due": now],
            ["id": "dup", "title": "Already there", "due": now - 1],
            "Plain"]]
        let todo: JSONObject = ["id": "todo", "items": [["id": "dup", "title": "Already there", "group": "waiting"]]]
        let r = Due.move(scheduled: sched, todo: todo, now: now)!
        XCTAssertEqual(r.moved.map(\.id), ["a", "no-id", "dup"])
        let left = (r.scheduled["items"] as! [Any]).map { ($0 as? JSONObject)?["id"] as? String ?? ($0 as! String) }
        XCTAssertEqual(left, ["b", "c", "Plain"])
        let t = r.todo["items"] as! [JSONObject]
        XCTAssertEqual(t.map { $0["id"] as! String }, ["dup", "a", "no-id"])
        XCTAssertEqual(t[1]["group"] as? String, "waiting")
        XCTAssertNil(t[1]["when"])
        XCTAssertEqual(t[1]["detail"] as? String, "d")
        XCTAssertEqual(t[1]["thread"] as? String, "t1")
        XCTAssertEqual(t[1]["due"] as? Double, now - 60)
        XCTAssertTrue(r.todoChanged)
        XCTAssertNil(Due.move(scheduled: ["items": [["id": "b", "title": "Later", "due": now + 1]]], todo: todo, now: now))
        // Only already-moved items: To do stays as it is.
        XCTAssertEqual(Due.move(scheduled: ["items": [["id": "dup", "title": "Already there", "due": now - 1]]], todo: todo, now: now)?.todoChanged, false)
    }

    func testPlanItemsReadDue() {
        let items = Plan.items(["items": [["id": "a", "title": "A", "due": 1792700000], ["id": "b", "title": "B", "due": "1792700000.5"], ["id": "c", "title": "C", "due": true], ["id": "d", "title": "D", "due": "Fri"]]])
        XCTAssertEqual(items.map(\.due), [1792700000, 1792700000.5, nil, nil])
    }

    func testParseHandWrittenTimes() {
        let now = at(2026, 10, 23, 9, 0)
        let p = { (s: String) in Due.parse(s, now: now, calendar: self.cal) }
        XCTAssertEqual(p("2026-10-24 10:00"), at(2026, 10, 24, 10, 0).timeIntervalSince1970)
        XCTAssertEqual(p("2026-10-24T10:00"), at(2026, 10, 24, 10, 0).timeIntervalSince1970)
        XCTAssertEqual(p("2026-10-24T08:00:00Z"), at(2026, 10, 24, 10, 0).timeIntervalSince1970)
        XCTAssertEqual(p("+2h"), now.timeIntervalSince1970 + 7200)
        XCTAssertEqual(p("+30m"), now.timeIntervalSince1970 + 1800)
        XCTAssertEqual(p("+3d"), now.timeIntervalSince1970 + 3 * 86400)
        XCTAssertEqual(p("tomorrow 10:00"), at(2026, 10, 24, 10, 0).timeIntervalSince1970)
        XCTAssertEqual(p("today 22:00"), at(2026, 10, 23, 22, 0).timeIntervalSince1970)
        XCTAssertEqual(p("Tonight"), at(2026, 10, 23, 23, 0).timeIntervalSince1970)
        XCTAssertNil(p("Fri"))
        XCTAssertNil(p("tomorrow 25:00"))
    }
}
