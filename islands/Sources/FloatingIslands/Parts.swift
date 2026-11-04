// The nine parts (Ideas/generative-ui.md §3): bars, line, stat, list, image, slider, toggle, button, text.
// Each renders into a layer of a given width and registers its click targets.
import AppKit
import ImageIO
import IslandModel
import QuartzCore

enum Control {
    case button(id: String, label: String)
    case listRow(id: String, title: String)
    case toggle(id: String, on: Bool)
    case slider(id: String, min: Double, max: Double, step: Double)
    case open(target: String)
    /// Tick a plan item off or back on (`key`: its stable id, see `PlanItem.key`).
    case tick(key: String, title: String)
    /// Open or close a waiting item's proposal.
    case disclose(key: String)
    /// Answer a proposal: Yes / No / Later (or the file's own actions).
    case approve(key: String, action: String)
}

struct HitRegion { let rect: CGRect; let control: Control }   // rect in body coordinates

struct Body {
    let layer: CALayer
    let height: CGFloat
    let regions: [HitRegion]
    var sliders: [String: (track: CGRect, fill: CALayer, knob: CALayer, label: CATextLayer?, unit: String)] = [:]
    var arrivals: [() -> Void] = []        // small arrival animations, played when the body is shown
    /// The "started 12 min ago" line, refreshed once a minute while the island is open.
    var clock: (layer: CATextLayer, text: () -> String)?
    /// Plan rows, group headers and open proposals, each in its own layer keyed `row:<key>`, `hdr:<group>`,
    /// `prop:<key>`: an update diffs them by key and animates each (slide, fade, highlight). Positions in body coordinates.
    var rows: [String: CALayer] = [:]
}

private func num(_ v: Any?) -> Double? { (v as? NSNumber)?.doubleValue ?? (v as? String).flatMap(Double.init) }
private func str(_ v: Any?) -> String? {
    if let s = v as? String { return s }
    if let n = v as? NSNumber { return Island.format(n.doubleValue) }
    return nil
}

private func box(_ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat, _ color: NSColor, radius: CGFloat = 0) -> CALayer {
    let l = CALayer()
    l.anchorPoint = .zero
    l.frame = CGRect(x: x, y: y, width: w, height: h)
    l.backgroundColor = color.cgColor
    l.cornerRadius = radius
    l.cornerCurve = .continuous
    l.noImplicitAnimations()
    return l
}

/// What an open island shows, in order: its state, stage and times; the agent's summary, steps, key values and
/// image; its own parts; the open target as a button. A bare island (title and stage only) still says something.
func bodyParts(_ i: Island) -> [JSONObject] {
    var p: [JSONObject] = []
    let custom = !i.parts.isEmpty || i.summary != nil || !i.steps.isEmpty || !i.items.isEmpty || !i.facts.isEmpty || i.image != nil
    if ["task", "waiting", "result", "failed"].contains(i.look) || !custom { p.append(["type": "status"]) }
    if let s = i.summary { p.append(["type": "text", "value": s]) }
    if i.look == "task", let pr = i.progress, i.parts.isEmpty { p.append(["type": "progress", "value": pr]) }
    if !i.steps.isEmpty { p.append(["type": "steps"]) }
    if i.list != nil || i.groupedItems != nil { p.append(["type": "todo"]) } else if !i.items.isEmpty { p.append(["type": "items"]) }
    if !i.facts.isEmpty { p.append(["type": "kv"]) }
    if let img = i.image { p.append(["type": "image", "path": img, "open": img]) }
    p += i.parts
    if let o = i.open { p.append(["type": "openButton", "target": o]) }
    return p
}

/// "just now", "12 min", "1 h 5 min", "3 d".
func spoken(_ t: TimeInterval) -> String {
    let m = Int(t / 60)
    if m < 1 { return "under a minute" }
    if m < 60 { return "\(m) min" }
    if m < 24 * 60 { return m % 60 == 0 ? "\(m / 60) h" : "\(m / 60) h \(m % 60) min" }
    return "\(m / (24 * 60)) d"
}
func ago(_ d: Date, _ now: Date) -> String { now.timeIntervalSince(d) < 60 ? "just now" : spoken(now.timeIntervalSince(d)) + " ago" }

/// The status line pair: what state and stage it is in, and how long since it started or last changed.
/// Times come from the CLI's stamps; a writer that left them out falls back to the file's own dates.
func statusLines(_ i: Island, born: Date?, written: Date? = nil) -> (String, () -> String) {
    let word: String
    switch i.look {
    case "failed": word = "Failed"
    case "waiting": word = "Waiting on you"
    case "result": word = "Done"
    case "task": word = "Running"
    default: word = i.preset == "list" ? "List" : "Note"
    }
    var first = [word]
    if let p = i.progress, i.look == "task" { first.append("\(Int((p * 100).rounded()))%") }
    if let s = i.stage, i.look != "result" { first.append(s) }
    if let v = i.value, i.look == "result" { first.append(v) }
    let started = i.started ?? born, updated = i.updated, ended = i.ended ?? (["result", "failed"].contains(i.look) ? (updated ?? written) : nil)
    let look = i.look
    let times: () -> String = {
        let now = Date()
        if ["result", "failed"].contains(look), let e = ended {
            var t = (look == "failed" ? "Failed " : "Finished ") + ago(e, now)
            if let s = started, e.timeIntervalSince(s) >= 1 { t += " · took " + spoken(max(60, e.timeIntervalSince(s))) }
            return t
        }
        var bits: [String] = []
        if let s = started { bits.append((look == "waiting" ? "Asked " : look == "task" ? "Started " : "Added ") + ago(s, now)) }
        if let u = updated, started.map({ u.timeIntervalSince($0) >= 60 }) ?? true { bits.append((bits.isEmpty ? "Updated " : "updated ") + ago(u, now)) }
        return bits.joined(separator: " · ")
    }
    return (first.joined(separator: " · "), times)
}

/// Build the expanded body of an island from its parts. Consecutive buttons form one row.
func buildBody(_ island: Island, width: CGFloat, scale: CGFloat, born: Date? = nil, written: Date? = nil, openItems: Set<String> = []) -> Body {
    let root = CALayer()
    root.anchorPoint = .zero
    root.noImplicitAnimations()
    var y: CGFloat = 0
    var regions: [HitRegion] = []
    var body = Body(layer: root, height: 0, regions: [])
    let parts = bodyParts(island)

    var i = 0
    while i < parts.count {
        if y > 0 { y += Look.partGap }
        let part = parts[i]
        let type = part["type"] as? String ?? "text"
        switch type {
        case "button":
            var row: [JSONObject] = []
            while i < parts.count, (parts[i]["type"] as? String) == "button", row.count < 3 { row.append(parts[i]); i += 1 }
            i -= 1
            let h: CGFloat = 34, gap: CGFloat = 8
            let w = (width - gap * CGFloat(row.count - 1)) / CGFloat(row.count)
            for (k, b) in row.enumerated() {
                let label = str(b["label"]) ?? "OK"
                let id = (b["action"] as? String) ?? (b["id"] as? String) ?? label
                let chosen = island.chosen
                let dim = chosen != nil && chosen != id
                let x = CGFloat(k) * (w + gap)
                let cap = box(x, y, w, h, Look.white(dim ? 0.16 : 1), radius: h / 2)
                root.addSublayer(cap)
                let t = textLayer(label, Look.button, dim ? Look.ink2 : .black, maxWidth: w - 16, scale: scale)
                t.position = CGPoint(x: x + (w - t.bounds.width) / 2, y: y + (h - t.bounds.height) / 2)
                root.addSublayer(t)
                if chosen == nil { regions.append(HitRegion(rect: cap.frame, control: .button(id: id, label: label))) }
            }
            y += h

        case "text":
            let s = str(part["value"]) ?? str(part["text"]) ?? ""
            let lines = (part["lines"] as? Int) ?? (island.preset == "note" ? 6 : 3)
            let t = textLayer(s, Look.body, Look.ink, maxWidth: width, lines: lines, scale: scale)
            t.position = CGPoint(x: 0, y: y)
            root.addSublayer(t)
            y += t.bounds.height

        case "stat":
            let v = str(part["value"]) ?? "–"
            let t = textLayer(v, Look.stat, .white, maxWidth: width * 0.7, scale: scale)
            t.position = CGPoint(x: 0, y: y)
            root.addSublayer(t)
            if let unit = str(part["unit"]) {
                let u = textLayer(unit, Look.small, Look.ink2, maxWidth: width - t.bounds.width - 6, scale: scale)
                u.position = CGPoint(x: t.bounds.width + 6, y: y + t.bounds.height - u.bounds.height - 4)
                root.addSublayer(u)
            }
            y += t.bounds.height
            // The baseline, shown as two bars so the comparison reads without words.
            if let b = part["baseline"] as? JSONObject, let bv = num(b["value"]), let vv = num(part["value"]), max(bv, vv) > 0 {
                let label = str(b["label"]) ?? "before"
                let bunit = str(b["unit"]) ?? str(part["unit"]).map { " " + $0 } ?? ""
                let lt = textLayer("\(label) \(str(b["value"]) ?? "")\(bunit)", Look.small, Look.ink2, maxWidth: width * 0.4, scale: scale)
                let barW = width - lt.bounds.width - 10
                let m = max(bv, vv)
                y += 4
                let a = box(0, y, max(4, barW * vv / m), 5, .white, radius: 2.5)
                root.addSublayer(a)
                body.arrivals.append(growBar(a))
                y += 9
                let bb = box(0, y, max(4, barW * bv / m), 5, Look.white(0.25), radius: 2.5)
                root.addSublayer(bb)
                body.arrivals.append(growBar(bb))
                lt.position = CGPoint(x: barW + 10, y: y + 2.5 - lt.bounds.height / 2)
                root.addSublayer(lt)
                y += 5
            }

        case "bars":
            let items = (part["items"] as? [JSONObject] ?? []).prefix(6)
            let vals = items.map { num($0["value"]) ?? 0 }
            let m = max(num(part["max"]) ?? 0, vals.max() ?? 1, 1e-9)
            let unit = str(part["unit"]).map { " " + $0 } ?? ""
            let labels = items.map { textLayer(str($0["label"]) ?? "", Look.small, Look.ink, maxWidth: width * 0.36, scale: scale) }
            let values = items.map { textLayer((str($0["value"]) ?? "") + unit, Look.smallMono, .white, maxWidth: width * 0.3, scale: scale) }
            let lw = labels.map(\.bounds.width).max() ?? 0
            let vw = values.map(\.bounds.width).max() ?? 0
            let barX = lw + 10, barMax = max(20, width - barX - vw - 10)
            let anyHighlight = items.contains { $0["highlight"] as? Bool == true }
            let rowH: CGFloat = 22
            for (k, it) in items.enumerated() {
                let cy = y + rowH / 2
                labels[k].position = CGPoint(x: 0, y: cy - labels[k].bounds.height / 2)
                root.addSublayer(labels[k])
                let lit = !anyHighlight || it["highlight"] as? Bool == true
                let bar = box(barX, cy - 4, max(4, barMax * vals[k] / m), 8, Look.white(lit ? 1 : 0.32), radius: 4)
                root.addSublayer(bar)
                body.arrivals.append(growBar(bar))
                values[k].position = CGPoint(x: width - values[k].bounds.width, y: cy - values[k].bounds.height / 2)
                root.addSublayer(values[k])
                y += rowH
            }

        case "line":
            let pts = (part["points"] as? [Any] ?? []).compactMap(num)
            let h: CGFloat = 44
            let lastText = pts.last.map { Island.format($0) + (str(part["unit"]).map { " " + $0 } ?? "") }
            let lt = lastText.map { textLayer($0, Look.smallMono, .white, maxWidth: width * 0.3, scale: scale) }
            let plotW = width - (lt.map { $0.bounds.width + 10 } ?? 0)
            if pts.count >= 2, let lo = pts.min(), let hi = pts.max() {
                let span = max(hi - lo, 1e-9)
                let path = CGMutablePath()
                var last = CGPoint.zero
                for (k, v) in pts.enumerated() {
                    let p = CGPoint(x: 3 + (plotW - 6) * CGFloat(k) / CGFloat(pts.count - 1), y: y + 3 + (h - 6) * CGFloat(1 - (v - lo) / span))
                    if k == 0 { path.move(to: p) } else { path.addLine(to: p) }
                    last = p
                }
                let s = CAShapeLayer()
                s.path = path
                s.strokeColor = NSColor.white.cgColor
                s.fillColor = nil
                s.lineWidth = 2
                s.lineJoin = .round
                s.lineCap = .round
                s.noImplicitAnimations()
                root.addSublayer(s)
                body.arrivals.append { [weak s] in
                    let a = Spring.calm.animation("strokeEnd", from: 0, to: 1)
                    s?.add(a, forKey: "draw")
                }
                let dot = box(last.x - 3.5, last.y - 3.5, 7, 7, .white, radius: 3.5)
                root.addSublayer(dot)
                if let lt { lt.position = CGPoint(x: width - lt.bounds.width, y: last.y - lt.bounds.height / 2); root.addSublayer(lt) }
            }
            y += h

        case "progress":
            let p = CGFloat(num(part["value"]) ?? 0)
            root.addSublayer(box(0, y, width, 6, Look.white(0.18), radius: 3))
            let f = box(0, y, max(6, width * p), 6, .white, radius: 3)
            root.addSublayer(f)
            body.arrivals.append(growBar(f))
            y += 6

        case "list":
            let items = (part["items"] as? [JSONObject] ?? []).prefix(6)
            let selected = (part["selected"] as? String) ?? island.chosen
            let rowH: CGFloat = 34
            for (k, it) in items.enumerated() {
                let id = str(it["id"]) ?? str(it["title"]) ?? "\(k)"
                let title = str(it["title"]) ?? id
                if k > 0 { root.addSublayer(box(0, y, width, 0.5, Look.white(0.1))) }
                let isSel = selected == id
                let t = textLayer(title, Look.body, selected == nil || isSel ? Look.ink : Look.ink2, maxWidth: width * 0.62, scale: scale)
                t.position = CGPoint(x: 0, y: y + (rowH - t.bounds.height) / 2)
                root.addSublayer(t)
                var right = width
                if isSel {
                    let c = symbolLayer("checkmark.circle.fill", size: 16, colors: [.white, Look.green], scale: scale)
                    c.position = CGPoint(x: width - 8, y: y + rowH / 2)
                    root.addSublayer(c)
                    right -= 24
                }
                if let v = str(it["value"]) {
                    let vt = textLayer(v, Look.smallMono, Look.ink2, maxWidth: width * 0.34, scale: scale)
                    vt.position = CGPoint(x: right - vt.bounds.width, y: y + (rowH - vt.bounds.height) / 2)
                    root.addSublayer(vt)
                }
                regions.append(HitRegion(rect: CGRect(x: 0, y: y, width: width, height: rowH), control: .listRow(id: id, title: title)))
                y += rowH
            }

        case "image":
            if let path = str(part["path"]) ?? str(part["src"]), let img = loadThumbnail(path, maxPixel: Int(width * scale)) {
                let aspect = CGFloat(img.height) / CGFloat(max(img.width, 1))
                let h = min(160, width * aspect)
                let w = h / max(aspect, 0.01)
                let l = box((width - w) / 2, y, w, h, .clear, radius: 10)
                l.contents = img
                l.contentsGravity = .resizeAspectFill
                l.masksToBounds = true
                root.addSublayer(l)
                if let o = str(part["open"]) { regions.append(HitRegion(rect: l.frame, control: .open(target: o))) }
                y += h
            } else {
                // A picture that is not there (or not a picture) takes no room: never a broken-image placeholder.
                y -= y > 0 ? Look.partGap : 0
            }

        case "toggle":
            let id = str(part["id"]) ?? "toggle"
            let on = part["on"] as? Bool ?? false
            let rowH: CGFloat = 26
            let t = textLayer(str(part["label"]) ?? id, Look.body, .white, maxWidth: width - 50, scale: scale)
            t.position = CGPoint(x: 0, y: y + (rowH - t.bounds.height) / 2)
            root.addSublayer(t)
            let track = box(width - 38, y + 3, 38, 20, on ? Look.green : Look.white(0.22), radius: 10)
            root.addSublayer(track)
            let knob = box(on ? width - 19 : width - 37, y + 4, 18, 18, .white, radius: 9)
            root.addSublayer(knob)
            regions.append(HitRegion(rect: CGRect(x: 0, y: y, width: width, height: rowH), control: .toggle(id: id, on: on)))
            y += rowH

        case "slider":
            let id = str(part["id"]) ?? "slider"
            let lo = num(part["min"]) ?? 0, hi = max(num(part["max"]) ?? 1, lo + 1e-9)
            let v = min(hi, max(lo, num(part["value"]) ?? lo))
            let unit = str(part["unit"]) ?? ""
            let rowH: CGFloat = 26
            let vt = textLayer(Island.format(hi) + (unit.isEmpty ? "" : " " + unit), Look.smallMono, .white, maxWidth: width * 0.3, scale: scale)
            let trackW = width - vt.bounds.width - 14
            let frac = CGFloat((v - lo) / (hi - lo))
            root.addSublayer(box(0, y + rowH / 2 - 2, trackW, 4, Look.white(0.2), radius: 2))
            let fill = box(0, y + rowH / 2 - 2, max(4, trackW * frac), 4, .white, radius: 2)
            root.addSublayer(fill)
            let knob = box(trackW * frac - 9, y + rowH / 2 - 9, 18, 18, .white, radius: 9)
            root.addSublayer(knob)
            vt.string = NSAttributedString(string: Island.format(v) + (unit.isEmpty ? "" : " " + unit),
                                           attributes: [.font: Look.smallMono, .foregroundColor: NSColor.white])
            vt.alignmentMode = .right
            vt.position = CGPoint(x: width - vt.bounds.width, y: y + (rowH - vt.bounds.height) / 2)
            root.addSublayer(vt)
            let track = CGRect(x: 0, y: y, width: trackW, height: rowH)
            regions.append(HitRegion(rect: track.insetBy(dx: -9, dy: 0), control: .slider(id: id, min: lo, max: hi, step: num(part["step"]) ?? 0)))
            body.sliders[id] = (track, fill, knob, vt, unit)
            y += rowH

        case "status":
            let (first, times) = statusLines(island, born: born, written: written)
            let t = textLayer(first, Look.statusFont, Look.ink, maxWidth: width, scale: scale)
            t.position = CGPoint(x: 0, y: y)
            root.addSublayer(t)
            y += t.bounds.height
            let when = times()
            if !when.isEmpty {
                y += 2
                let c = textLayer(when, Look.small, Look.ink2, maxWidth: width, scale: scale)
                c.bounds.size.width = width
                c.position = CGPoint(x: 0, y: y)
                root.addSublayer(c)
                body.clock = (c, times)
                y += c.bounds.height
            }

        case "steps":
            // Every step stays readable: pending steps are white with a hollow ring, only done ones step back a little.
            let rowH: CGFloat = 26
            for st in island.steps.prefix(10) {
                let (sym, colors, ink): (String, [NSColor], NSColor) = switch st.mark {
                case "done": ("checkmark.circle.fill", [.white, Look.green], Look.inkDone)
                case "now": ("circle.inset.filled", [.white], .white)
                case "failed": ("xmark.circle.fill", [.white, Look.red], Look.ink)
                default: ("circle", [Look.ink2], Look.ink)
                }
                let ic = symbolLayer(sym, size: 16, colors: colors, scale: scale)
                ic.position = CGPoint(x: 8, y: y + rowH / 2)
                root.addSublayer(ic)
                let t = textLayer(st.title, st.mark == "now" ? Look.bodyStrong : Look.body, ink, maxWidth: width - 26, scale: scale)
                t.position = CGPoint(x: 26, y: y + (rowH - t.bounds.height) / 2)
                root.addSublayer(t)
                y += rowH
            }

        case "items":
            // A plain list: bullets, no status. Long items wrap to a second line.
            for (k, s) in island.items.prefix(12).enumerated() {
                if k > 0 { y += 4 }
                let t = textLayer(s, Look.body, Look.ink, maxWidth: width - 18, lines: 2, scale: scale)
                let dot = box(3, y + ceil(Look.body.ascender - Look.body.capHeight / 2) - 2.5, 5, 5, Look.ink2, radius: 2.5)
                root.addSublayer(dot)
                t.position = CGPoint(x: 18, y: y)
                root.addSublayer(t)
                y += t.bounds.height
            }

        case "todo":
            y = buildPlan(island, into: root, at: y, width: width, scale: scale, openItems: openItems, regions: &regions, rows: &body.rows)

        case "kv":
            let rowH: CGFloat = 22
            for f in island.facts.prefix(6) {
                let v = textLayer(f.value, Look.smallMono, .white, maxWidth: width * 0.6, scale: scale)
                let l = textLayer(f.label, Look.small, Look.ink2, maxWidth: width - v.bounds.width - 12, scale: scale)
                l.position = CGPoint(x: 0, y: y + (rowH - l.bounds.height) / 2)
                v.position = CGPoint(x: width - v.bounds.width, y: y + (rowH - v.bounds.height) / 2)
                root.addSublayer(l)
                root.addSublayer(v)
                y += rowH
            }

        case "openButton":
            // The open target as a compact chip sized to its label: "localhost ↗", "compare3.html ↗"; a file that
            // is gone says so instead and does nothing.
            let target = str(part["target"]) ?? ""
            let gone = OpenTarget.missing(target)
            let h: CGFloat = 26, padX: CGFloat = 11
            let label = (gone ? "Not found: " : "") + OpenTarget.label(target)
            let t = textLayer(label, Look.small, gone ? Look.ink2 : Look.ink, maxWidth: width - 2 * padX - 16, scale: scale)
            let arrow = gone ? nil : symbolLayer("arrow.up.right", size: 10, colors: [Look.ink], weight: .bold, scale: scale)
            let w = t.bounds.width + 2 * padX + (arrow == nil ? 0 : 14)
            let cap = box(0, y, w, h, Look.white(gone ? 0.08 : 0.14), radius: h / 2)
            root.addSublayer(cap)
            t.position = CGPoint(x: padX, y: y + (h - t.bounds.height) / 2)
            root.addSublayer(t)
            if let arrow { arrow.position = CGPoint(x: padX + t.bounds.width + 9, y: y + h / 2); root.addSublayer(arrow) }
            if !gone { regions.append(HitRegion(rect: cap.frame, control: .open(target: target))) }
            y += h

        default:
            y -= y > 0 ? Look.partGap : 0
        }
        i += 1
    }
    root.bounds = CGRect(x: 0, y: 0, width: width, height: y)
    return Body(layer: root, height: y, regions: regions, sliders: body.sliders, arrivals: body.arrivals, clock: body.clock, rows: body.rows)
}

private func growBar(_ bar: CALayer) -> () -> Void {
    { [weak bar] in
        guard let bar else { return }
        let w = bar.bounds.width
        let a = Spring.grow.animation("bounds.size.width", from: min(4, w), to: w)
        let p = Spring.grow.animation("position.x", from: bar.position.x - (w - min(4, w)) * bar.anchorPoint.x, to: bar.position.x)
        bar.add(a, forKey: "grow")
        if bar.anchorPoint.x != 0 { bar.add(p, forKey: "growPos") }
    }
}

private func loadThumbnail(_ path: String, maxPixel: Int) -> CGImage? {
    let url = path.hasPrefix("file://") ? URL(string: path) : URL(fileURLWithPath: (path as NSString).expandingTildeInPath)
    guard let url, let src = CGImageSourceCreateWithURL(url as CFURL, nil) else { return nil }
    let opts: [CFString: Any] = [kCGImageSourceCreateThumbnailFromImageAlways: true, kCGImageSourceThumbnailMaxPixelSize: maxPixel,
                                 kCGImageSourceCreateThumbnailWithTransform: true, kCGImageSourceShouldCacheImmediately: true]
    return CGImageSourceCreateThumbnailAtIndex(src, 0, opts as CFDictionary)
}

// MARK: Plan lists (To do, Scheduled, Settled)

/// What a plan row shows on its right: a Scheduled item's countdown to its `due` time, else its written `when`.
func whenLabel(_ it: PlanItem, list: String, now: Date = Date()) -> (text: String, soon: Bool)? {
    if list == "scheduled", let d = it.due { return Due.label(d, now: now) }
    return it.when.map { ($0, false) }
}

/// A layer holding one keyed piece of a plan list, placed at `y` in the body; `sig` says what it shows, so an update
/// can tell a row that only moved from one whose look changed.
private func keyed(_ y: CGFloat, _ width: CGFloat, _ h: CGFloat, sig: String) -> CALayer {
    let l = CALayer()
    l.anchorPoint = .zero
    l.frame = CGRect(x: 0, y: y, width: width, height: h)
    // Opaque on the island's black (invisible at rest): a row crossing others mid-slide covers them cleanly.
    l.backgroundColor = NSColor.black.cgColor
    l.noImplicitAnimations()
    l.setValue(sig, forKey: "fiSig")
    return l
}

/// A closed plan row's height, and where its symbol (centre) and title start.
let planRowH: CGFloat = 26, planMarkX: CGFloat = 35, planTextX: CGFloat = 48

/// To do: a header per group (Running, Waiting on you, Discussed) and its rows. Scheduled and Settled: rows only,
/// a Scheduled row with its `when` on the right. A row is its tick ring, a small symbol for its project and the title
/// on one line; the ring ticks (a green check once ticked; ticked rows sink to the bottom of their group, struck and
/// stepped back). A click on the rest of a row with an explanation or a proposal opens it: the title wraps, the
/// explanation follows in full, then the proposal's buttons. Only the ring ticks.
func buildPlan(_ island: Island, into root: CALayer, at y0: CGFloat, width: CGFloat, scale: CGFloat, openItems: Set<String>,
               regions: inout [HitRegion], rows: inout [String: CALayer]) -> CGFloat {
    var y = y0
    let list = island.list ?? "todo"
    let groups = Plan.grouped(island.planItems, list: list)
    if groups.isEmpty {
        let t = textLayer(list == "settled" ? "Nothing settled yet" : "Nothing here yet", Look.body, Look.ink2, maxWidth: width, scale: scale)
        let l = keyed(y, width, t.bounds.height, sig: "empty")
        l.addSublayer(t)
        root.addSublayer(l)
        rows["empty"] = l
        return y + t.bounds.height
    }
    for (gk, g) in groups.enumerated() {
        if gk > 0 { y += 10 }
        if !g.group.isEmpty {
            let ink: NSColor = g.group == "waiting" ? Look.orange : (g.group == "running" ? .white : Look.ink2)
            let h = textLayer(TodoGroup.label(g.group).uppercased(), Look.groupHeader, ink, maxWidth: width, scale: scale)
            let hl = keyed(y, width, h.bounds.height, sig: g.group)
            hl.addSublayer(h)
            root.addSublayer(hl)
            rows["hdr:" + g.group] = hl
            y += h.bounds.height + 4
        }
        // Every item: a list taller than the display scrolls inside the island (IslandController).
        for it in g.items.prefix(500) {
            let open = it.discloses && openItems.contains(it.key)
            let asks = it.proposal != nil && !it.done
            // Trailing: the approval the user picked, else when (a Scheduled countdown or the written `when`).
            var trailing: CALayer?
            var trailingW: CGFloat = 0
            let chosenLabel = it.chosen.flatMap { c in it.proposal?.actions.first { $0.action == c }?.label ?? c.capitalized }
            if asks, let chosenLabel, !open {
                let t = textLayer(chosenLabel, Look.small, Look.ink2, maxWidth: 90, scale: scale)
                trailing = t; trailingW = t.bounds.width
            } else if let w = whenLabel(it, list: list) {
                // A due time counts down (the app redraws it when it changes); otherwise the written `when`.
                let t = textLayer(w.text, Look.small, w.soon ? Look.ink : Look.ink2, maxWidth: 110, scale: scale)
                trailing = t; trailingW = t.bounds.width
            }
            let textW = width - planTextX - (trailingW > 0 ? trailingW + 10 : 0)
            let t = textLayer(it.title, Look.body, it.done ? Look.inkDone : Look.ink, maxWidth: textW, lines: open ? 8 : 1, strike: it.done, scale: scale)
            let detail = open ? it.detail.map { textBlock($0, Look.note, Look.ink2, maxWidth: width - planTextX, scale: scale) } : nil
            let top = floor(max(0, (planRowH - ceil(Look.body.ascender - Look.body.descender + Look.body.leading)) / 2))
            let rowH = detail.map { top + t.bounds.height + 3 + $0.bounds.height + 8 } ?? max(planRowH, t.bounds.height + 2 * top)
            // An `icon` that is no SF Symbol (a typo, an SVG) falls back to the picked one rather than a dot.
            var symbol = it.symbol(list: list)
            if NSImage(systemSymbolName: symbol, accessibilityDescription: nil) == nil { var plain = it; plain.icon = nil; symbol = plain.symbol(list: list) }
            let sig = [it.title, it.done ? "1" : "0", whenLabel(it, list: list)?.text ?? "", open ? it.detail ?? "" : "", it.chosen ?? "", open ? "o" : "", it.proposal == nil ? "" : "p", g.group, symbol].joined(separator: "\u{1}")
            let row = keyed(y, width, rowH, sig: sig)
            let ic = symbolLayer(it.done ? "checkmark.circle.fill" : "circle", size: 16,
                                 colors: it.done ? [.white, Look.green] : [Look.ink2], scale: scale)
            ic.position = CGPoint(x: 8, y: planRowH / 2)
            row.addSublayer(ic)
            let mark = symbolLayer(symbol, size: 13, colors: [Look.ink3], weight: .medium, scale: scale)
            mark.position = CGPoint(x: planMarkX, y: planRowH / 2)
            row.addSublayer(mark)
            t.position = CGPoint(x: planTextX, y: top)
            row.addSublayer(t)
            if let detail { detail.position = CGPoint(x: planTextX, y: top + t.bounds.height + 3); row.addSublayer(detail) }
            if let trailing {
                trailing.position = CGPoint(x: width - trailingW, y: (planRowH - trailing.bounds.height) / 2)
                row.addSublayer(trailing)
            }
            root.addSublayer(row)
            rows["row:" + it.key] = row
            // Only the ring ticks, so a click on a title never ticks by surprise; the rest of the row opens and closes it
            // (first region wins).
            regions.append(HitRegion(rect: CGRect(x: 0, y: y, width: 26, height: planRowH), control: .tick(key: it.key, title: it.title)))
            if it.discloses {
                regions.append(HitRegion(rect: CGRect(x: 0, y: y, width: width, height: rowH), control: .disclose(key: it.key)))
            }
            y += rowH
            if open, asks, let p = it.proposal {
                // The proposal: what exactly is proposed, then its buttons. Primary first (white), the rest quieter.
                // Under the item's own text, so the answer reads as part of it.
                y += 4
                let inset: CGFloat = 10, x0 = planTextX - 10
                let text = p.text.isEmpty ? nil : textBlock(p.text, Look.small, Look.ink, maxWidth: width - x0 - 2 * inset, scale: scale)
                let bh: CGFloat = 32, gap: CGFloat = 8
                let blockH = inset + (text.map { $0.bounds.height + 10 } ?? 0) + bh + inset
                let block = keyed(y, width, blockH, sig: [p.text, it.chosen ?? ""].joined(separator: "\u{1}"))
                let bg = box(x0, 0, width - x0, blockH, Look.white(0.08), radius: 14)
                block.addSublayer(bg)
                var by = inset
                if let text { text.position = CGPoint(x: x0 + inset, y: by); block.addSublayer(text); by += text.bounds.height + 10 }
                let acts = Array(p.actions.prefix(3))
                let bx0 = x0 + inset, bw = (width - x0 - 2 * inset - gap * CGFloat(acts.count - 1)) / CGFloat(max(1, acts.count))
                for (k, a) in acts.enumerated() {
                    let x = bx0 + CGFloat(k) * (bw + gap)
                    let picked = it.chosen == a.action
                    let primary = it.chosen == nil ? k == 0 : picked
                    let cap = box(x, by, bw, bh, primary ? .white : Look.white(it.chosen == nil ? 0.16 : 0.1), radius: bh / 2)
                    block.addSublayer(cap)
                    let label = textLayer(a.label, Look.button, primary ? .black : (it.chosen == nil ? .white : Look.ink2), maxWidth: bw - 16, scale: scale)
                    var lx = x + (bw - label.bounds.width) / 2
                    if picked {
                        let ck = symbolLayer("checkmark", size: 12, colors: [.black], weight: .bold, scale: scale)
                        lx += 9
                        ck.position = CGPoint(x: lx - 11, y: by + bh / 2)
                        block.addSublayer(ck)
                    }
                    label.position = CGPoint(x: lx, y: by + (bh - label.bounds.height) / 2)
                    block.addSublayer(label)
                    if it.chosen == nil {
                        regions.append(HitRegion(rect: CGRect(x: x, y: y + by, width: bw, height: bh), control: .approve(key: it.key, action: a.action)))
                    }
                }
                root.addSublayer(block)
                rows["prop:" + it.key] = block
                y += blockH + 4
            }
        }
    }
    return y
}
