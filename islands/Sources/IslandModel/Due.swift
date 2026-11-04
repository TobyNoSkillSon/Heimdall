// Scheduled items' due times ("proper integration"): writers store an absolute `due` (epoch
// seconds); the app draws the countdown from the local clock and moves a due item into To do (Waiting) by itself.
// Pure functions only (no AppKit), shared by the app, the CLI and the tests.
import Foundation

public enum Due {
    /// A `due` value as epoch seconds: a number, or a numeric string. Anything else is no due time.
    public static func seconds(_ v: Any?) -> Double? {
        if let n = v as? NSNumber, !(v is Bool), n.doubleValue.isFinite, n.doubleValue > 0 { return n.doubleValue }
        if let s = v as? String, let d = Double(s.trimmingCharacters(in: .whitespaces)), d.isFinite, d > 0 { return d }
        return nil
    }

    private static func days(from now: Date, to due: Date, _ cal: Calendar) -> Int {
        cal.dateComponents([.day], from: cal.startOfDay(for: now), to: cal.startOfDay(for: due)).day ?? 0
    }

    private static func clock(_ d: Date, _ cal: Calendar) -> String {
        let c = cal.dateComponents([.hour, .minute], from: d)
        return String(format: "%02d:%02d", c.hour ?? 0, c.minute ?? 0)
    }

    /// The label for an item due at `due`, and whether it is soon (within the hour: drawn at full white).
    /// Under an hour "in 12 min"; later today "in 3 h"; tomorrow "tomorrow 10:00"; 2–6 days "in 2 days"; then "26 Oct".
    public static func label(_ due: Double, now: Date, calendar cal: Calendar = .current, locale: Locale = .current) -> (text: String, soon: Bool) {
        let d = due - now.timeIntervalSince1970
        if d <= 0 { return ("now", true) }
        let mins = Int((d / 60).rounded(.up))
        if mins < 60 { return ("in \(max(1, mins)) min", true) }
        let date = Date(timeIntervalSince1970: due)
        switch days(from: now, to: date, cal) {
        case ...0: return ("in \(max(1, Int((d / 3600).rounded()))) h", false)
        case 1: return ("tomorrow \(clock(date, cal))", false)
        case 2...6: return ("in \(days(from: now, to: date, cal)) days", false)
        default:
            let f = DateFormatter()
            f.locale = locale
            f.calendar = cal
            f.timeZone = cal.timeZone
            f.setLocalizedDateFormatFromTemplate("dMMM")
            return (f.string(from: date), false)
        }
    }

    /// The next moment `label(due, …)` reads differently (or the item falls due); `.distantFuture` once it is due.
    public static func nextChange(_ due: Double, now: Date, calendar cal: Calendar = .current) -> Date {
        let t = now.timeIntervalSince1970, d = due - t
        if d <= 0 { return .distantFuture }
        let mins = Int((d / 60).rounded(.up))
        // Minute steps: the label changes when the remaining time drops to (mins - 1) minutes; at 1 min, when due.
        if mins < 60 { return Date(timeIntervalSince1970: due - Double(mins - 1) * 60) }
        let date = Date(timeIntervalSince1970: due)
        let dayDiff = days(from: now, to: date, cal)
        // Minutes take over once ceil(remaining minutes) drops below 60, i.e. at 59 minutes left.
        let intoHour = Date(timeIntervalSince1970: due - 59 * 60)
        let midnight = cal.date(byAdding: .day, value: 1, to: cal.startOfDay(for: now)) ?? intoHour
        if dayDiff <= 0 {
            // Rounded hours change as the remaining time crosses (h - 0.5) hours; under an hour, minutes take over.
            let h = max(1, Int((d / 3600).rounded()))
            return Date(timeIntervalSince1970: due - max(59 * 60, (Double(h) - 0.5) * 3600))
        }
        // Tomorrow and later change at midnight (or when the last hour begins, if that comes first).
        return min(midnight, intoHour)
    }

    /// Items of a Scheduled island that are due at `now` (not ticked) move to the To do island's Waiting group,
    /// keeping their id and every other key but `when`; an item whose id To do already has is only removed from
    /// Scheduled. nil when nothing is due. Returns both islands rewritten and the moved items (id, title).
    /// `todoChanged` is false when every moved item was already in To do (only Scheduled needs writing).
    public static func move(scheduled: JSONObject, todo: JSONObject, now: Double) -> (scheduled: JSONObject, todo: JSONObject, moved: [(id: String, title: String)], todoChanged: Bool)? {
        let items = scheduled["items"] as? [Any] ?? []
        var keep: [Any] = [], moved: [(String, String)] = [], added = 0
        var todoItems = todo["items"] as? [Any] ?? []
        let ids = { (v: Any) -> String? in
            guard let o = v as? JSONObject else { return nil }
            return (o["id"] as? String) ?? (o["id"] as? NSNumber).map { "\($0)" }
        }
        var taken = Set(todoItems.compactMap(ids))
        for v in items {
            guard var o = v as? JSONObject, let due = seconds(o["due"]), due <= now, (o["done"] as? Bool) != true,
                  let title = o["title"] as? String, !title.isEmpty else { keep.append(v); continue }
            let id = ids(o) ?? Plan.slug(title, taken: taken)
            moved.append((id, title))
            if taken.contains(id) { continue }
            taken.insert(id)
            o["id"] = id
            o["group"] = "waiting"
            o.removeValue(forKey: "when")
            todoItems.append(o)
            added += 1
        }
        guard !moved.isEmpty else { return nil }
        var s = scheduled, t = todo
        s["items"] = keep
        t["items"] = todoItems
        return (s, t, moved, added > 0)
    }

    /// A due time as written by hand or by the CLI, relative to `now` in `cal`'s time zone: "2026-10-24 10:00",
    /// "2026-10-24T10:00", an ISO 8601 date-time with an offset, "+30m" / "+2h" / "+3d", "today 22:00",
    /// "tomorrow 10:00", "tonight" (23:00 today). nil when unreadable.
    public static func parse(_ text: String, now: Date = Date(), calendar cal: Calendar = .current) -> Double? {
        let s = text.trimmingCharacters(in: .whitespaces).lowercased()
        if s.hasPrefix("+"), let unit = s.last, let n = Double(s.dropFirst().dropLast()), n >= 0 {
            let k: Double? = ["m": 60, "h": 3600, "d": 86400][unit]
            return k.map { now.timeIntervalSince1970 + n * $0 }
        }
        if s == "tonight" { return cal.date(bySettingHour: 23, minute: 0, second: 0, of: now)?.timeIntervalSince1970 }
        let parts = s.split(separator: " ").map(String.init)
        if parts.count == 2, ["today", "tomorrow", "tonight"].contains(parts[0]) {
            let hm = parts[1].split(separator: ":").compactMap { Int($0) }
            guard hm.count == 2, (0..<24).contains(hm[0]), (0..<60).contains(hm[1]) else { return nil }
            let day = parts[0] == "tomorrow" ? cal.date(byAdding: .day, value: 1, to: now) ?? now : now
            return cal.date(bySettingHour: hm[0], minute: hm[1], second: 0, of: day)?.timeIntervalSince1970
        }
        let iso = ISO8601DateFormatter()
        if let d = iso.date(from: text.trimmingCharacters(in: .whitespaces)) { return d.timeIntervalSince1970 }
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.calendar = cal
        f.timeZone = cal.timeZone
        for fmt in ["yyyy-MM-dd HH:mm", "yyyy-MM-dd'T'HH:mm", "yyyy-MM-dd'T'HH:mm:ss", "yyyy-MM-dd HH:mm:ss"] {
            f.dateFormat = fmt
            if let d = f.date(from: text.trimmingCharacters(in: .whitespaces)) { return d.timeIntervalSince1970 }
        }
        return nil
    }
}
