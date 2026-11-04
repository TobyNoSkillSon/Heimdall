// A second step before an answer goes to the agent (after a stray click picked a website
// concept): the clicked button's row turns into "Sure?  [Cancel] [Confirm]" in place. Confirm, or a second click on
// the same button within `window` seconds, sends it; Cancel, or nothing for `window` seconds, puts the row back.
import AppKit
import QuartzCore

enum Confirm {
    static let window: TimeInterval = 4

    /// "Later" only puts a question off (it comes back): one click. Every other answer asks first.
    static func needed(_ action: String) -> Bool { !["later", "not-now", "notnow"].contains(action.lowercased()) }

    struct Row {
        let layer: CALayer
        let cancel: CGRect
        let confirm: CGRect
        /// The button that was clicked: a second click there confirms (unless it lands on Cancel).
        let clicked: CGRect
        let started: Date
    }

    /// The row over `row` (the clicked button's row, in the parent's coordinates, y down), painted `background` so it
    /// covers the buttons under it. Arrives with a quick fade and a small settle, like a native control state.
    static func row(over row: CGRect, clicked: CGRect, background: NSColor, scale: CGFloat) -> Row {
        // Capsules at button height, centred in a taller row (a list row).
        let h = min(row.height, 34), y0 = row.minY + (row.height - h) / 2
        let l = CALayer()
        l.anchorPoint = .zero
        l.frame = row.insetBy(dx: -2, dy: -2)
        l.backgroundColor = background.cgColor
        l.noImplicitAnimations()
        let pad: CGFloat = 2, gap: CGFloat = 8
        let ask = textLayer("Sure?", Look.button, Look.ink2, scale: scale)
        let askW = ceil(ask.bounds.width) + 6
        // Confirm takes the clicked button's own place (a second click there confirms), at least wide enough for its
        // word; Cancel takes the wider side beside it; "Sure?" leads the row when there is room on the left.
        var confirm = CGRect(x: clicked.midX - max(clicked.width, 104) / 2, y: y0, width: max(clicked.width, 104), height: h)
        confirm.origin.x = min(max(confirm.minX, row.minX), row.maxX - confirm.width)
        let leftW = confirm.minX - row.minX - gap, rightW = row.maxX - confirm.maxX - gap
        var cancel: CGRect
        var askX: CGFloat?
        if max(leftW, rightW) < 80 {
            // One button across the row: Sure? · Cancel · Confirm, Confirm still over the button's middle.
            let cw: CGFloat = 92
            askX = row.minX
            cancel = CGRect(x: row.minX + askW + gap, y: y0, width: cw, height: h)
            confirm = CGRect(x: cancel.maxX + gap, y: y0, width: row.maxX - cancel.maxX - gap, height: h)
        } else if rightW < 80 || (leftW > rightW && leftW >= askW + gap + 96) {
            // The last button: Sure? · Cancel · Confirm.
            let cw = leftW >= askW + gap + 96 ? leftW - askW - gap : leftW
            cancel = CGRect(x: confirm.minX - gap - cw, y: y0, width: cw, height: h)
            if cw < leftW { askX = confirm.minX - gap - cw - gap - askW }
        } else {
            // A first or middle button: Cancel on its right, "Sure?" just before it when there is room.
            cancel = CGRect(x: confirm.maxX + gap, y: y0, width: rightW, height: h)
            if leftW >= askW { askX = confirm.minX - gap - askW }
        }
        if let askX {
            ask.position = CGPoint(x: askX - row.minX + pad, y: pad + (row.height - ask.bounds.height) / 2)
            l.addSublayer(ask)
        }
        func cap(_ r: CGRect, _ fill: NSColor, _ text: String, _ ink: NSColor) {
            let local = r.offsetBy(dx: -l.frame.minX, dy: -l.frame.minY)
            let c = CALayer()
            c.anchorPoint = .zero
            c.frame = local
            c.cornerRadius = h / 2
            c.cornerCurve = .continuous
            c.backgroundColor = fill.cgColor
            c.noImplicitAnimations()
            l.addSublayer(c)
            let t = textLayer(text, Look.button, ink, maxWidth: r.width - 16, scale: scale)
            t.position = CGPoint(x: local.minX + (local.width - t.bounds.width) / 2, y: local.minY + (local.height - t.bounds.height) / 2)
            l.addSublayer(t)
        }
        cap(cancel, Look.white(0.16), "Cancel", .white)
        cap(confirm, .white, "Confirm", .black)
        // In: a quick fade with a slight scale settle (0.18 s), then still.
        let fade = CABasicAnimation(keyPath: "opacity")
        fade.fromValue = 0
        fade.toValue = 1
        let sc = CABasicAnimation(keyPath: "transform.scale")
        sc.fromValue = 0.97
        sc.toValue = 1
        let g = CAAnimationGroup()
        g.animations = [fade, sc]
        g.duration = 0.18
        g.timingFunction = CAMediaTimingFunction(name: .easeOut)
        l.add(IslandController.timed(g, l), forKey: "confirmIn")
        return Row(layer: l, cancel: cancel, confirm: confirm, clicked: clicked, started: Date())
    }

    enum Hit { case confirm, cancel, outside, ignored }

    /// What a click at `p` (the row's parent coordinates) means while the row is up.
    /// A double-click or a bounced tap is not a decision: Confirm takes clicks only this long after "Sure?" shows.
    static let settle: TimeInterval = 0.35

    static func hit(_ r: Row, _ p: CGPoint, now: Date = Date()) -> Hit {
        if r.cancel.insetBy(dx: -2, dy: -4).contains(p) { return .cancel }
        let age = now.timeIntervalSince(r.started)
        if r.confirm.insetBy(dx: -2, dy: -4).contains(p) || (r.clicked.contains(p) && age <= window) { return age >= settle ? .confirm : .ignored }
        return .outside
    }

    /// Out: a quick fade, then the layer goes.
    static func dismiss(_ r: Row) {
        CATransaction.begin()
        CATransaction.setCompletionBlock { r.layer.removeFromSuperlayer() }
        r.layer.fade(to: 0, duration: 0.12)
        CATransaction.commit()
    }
}
