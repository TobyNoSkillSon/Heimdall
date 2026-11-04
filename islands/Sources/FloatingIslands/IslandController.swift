// One island: a borderless, non-activating panel holding one black shape. The shape and everything in it move on
// Core Animation springs in the render server; the app's main thread only sets targets. At rest nothing runs: no
// loops, no breathing; things move only when they change, are carried, open or close.
import AppKit
import IslandModel
import QuartzCore

final class IslandPanel: NSPanel {
    init() {
        super.init(contentRect: NSRect(x: 0, y: 0, width: 28, height: 28), styleMask: [.borderless, .nonactivatingPanel],
                   backing: .buffered, defer: false)
        isFloatingPanel = true
        // Out of screen shares and streams unless Heimdall's Show Islands on Streams is on (Streams.swift).
        sharingType = Streams.sharing
        level = .statusBar
        collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary, .ignoresCycle]
        isOpaque = false
        backgroundColor = .clear
        hasShadow = false
        hidesOnDeactivate = false
        becomesKeyOnlyIfNeeded = true
        isReleasedWhenClosed = false
        animationBehavior = .none
        isMovable = false
    }
    var acceptsKey = false
    var onCancel: (() -> Void)?
    override var canBecomeKey: Bool { acceptsKey }
    override var canBecomeMain: Bool { false }
    override func cancelOperation(_ sender: Any?) { onCancel?() }
}

final class IslandView: NSView {
    weak var controller: IslandController?
    override var isFlipped: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
    override func mouseDown(with e: NSEvent) {
        if e.modifierFlags.contains(.control) { controller?.showMenu(e, in: self) } else { controller?.mouseDown(e) }
    }
    override func rightMouseDown(with e: NSEvent) { controller?.showMenu(e, in: self) }
    override func keyDown(with e: NSEvent) { if e.keyCode == 53 { controller?.closeViewer() } else { super.keyDown(with: e) } }
    override var acceptsFirstResponder: Bool { controller?.isViewer == true }
    override func mouseDragged(with e: NSEvent) { controller?.mouseDragged(e) }
    override func mouseUp(with e: NSEvent) { controller?.mouseUp(e) }
    override func scrollWheel(with e: NSEvent) { if controller?.scrollWheel(e) != true { super.scrollWheel(with: e) } }
}

final class IslandController: NSObject {
    let id: String
    private(set) var island: Island
    let panel = IslandPanel()
    let view = IslandView()
    private let container = FlippedView()
    private var fileViewer: FileViewer?   // viewer content (Viewer.swift)
    private var outsideMonitor: Any?
    /// The open viewer's file watch (only while it is open).
    fileprivate var fileWatch: FileWatch?
    private var closeLayer: CALayer?
    var isViewer: Bool { island.look == "viewer" }
    weak var app: AppController?

    private let root = CALayer()
    let shape = CALayer()
    private let header = CALayer()
    private var icon: CALayer?
    private var ring: CAShapeLayer?
    private var titleLayer: CATextLayer?
    private var trailingLayer: CATextLayer?
    private var trailingInPill = false
    private let bodyHost = CALayer()
    /// Holds the open content; scaled about the shape's top centre on the shape's own spring, so the content always
    /// fits the growing or shrinking shape (never cut, never an empty black slab).
    private let bodyGroup = CALayer()
    private var body: Body?
    private var clockTimer: DispatchSourceTimer?

    private(set) var anchor = CGPoint.zero        // the island's top centre, screen coordinates
    private var screenFrame = CGRect.zero
    private var rowTop = CGFloat.greatestFiniteMagnitude   // highest an island's top may be: under the menu bar
    private(set) var shapeRect = CGRect.zero      // where the shape is heading, screen coordinates
    private(set) var expanded = false
    private var generation = 0
    private var placed = false
    private(set) var removing = false
    let seq: Int

    // Pointer state.
    private var downScreen: CGPoint?
    private var downAnchor = CGPoint.zero
    private var dragging = false
    private var downPillAnchor = CGPoint.zero
    /// Recent pointer samples while carried (event time, screen point), for the throw's velocity on release.
    private var samples: [(t: TimeInterval, p: CGPoint)] = []
    /// The release velocity (points per second, y up) the next move starts with: a thrown island keeps its momentum.
    private var pendingVelocity: CGVector?
    static let closeRoom: CGFloat = 30
    private var sliderDrag: String?
    private var pressed: Control?

    /// Plan rows the user opened to read or answer (app-side: the agent's rewrites keep them open).
    private var openItems = Set<String>()
    /// Attention (bell or question mark): what the pill shows now, the signature the user has seen by opening it, and the
    /// one timer that plays a pulse every 6 s while it is set and the island is closed (none otherwise).
    private(set) var attentionShown: String?
    private var iconAttention: String?
    private var seenAttention: String?
    private var attentionTimer: DispatchSourceTimer?
    /// While running: one tiny sign of work every few seconds, still in between.
    private var workTimer: DispatchSourceTimer?
    static let workInterval: TimeInterval = 4
    private var glow: CALayer?
    /// Pulses played so far (lab readback).
    private(set) var pulseCount = 0
    /// Where a notch-born island started (lab readback).
    private(set) var bornFrom: CGRect?
    /// Set by the app for an island that appears while it runs (any writer: the agent's tools, the CLI, a refresher):
    /// it drops out of the notch like one carrying `origin: notch`. Islands already there at launch just appear.
    var fromNotch = false

    /// The tallest the island may be on its display: a little less than the room between the menu bar and the Dock
    ///. Taller content scrolls inside it. Set by the app before each layout.
    var heightCap: CGFloat = .greatestFiniteMagnitude {
        didSet { if heightCap != oldValue, body != nil, !isViewer { layoutViewport() } }
    }
    /// How far the open body is scrolled (points from its top; beyond the ends while rubber-banding).
    private(set) var scrollY: CGFloat = 0
    private var scrollIndicator: CALayer?
    private var scrollFade: CAGradientLayer?
    private var scrollEndWork: DispatchWorkItem?
    private var indicatorWork: DispatchWorkItem?
    /// Momentum that ran into an end is spent: its remaining events are ignored until the next gesture.
    private var momentumSpent = false

    /// An answer waiting for its second step (Confirm.swift): the control, its row over the body, the timer that
    /// puts the row back.
    private var confirming: (control: Control, row: Confirm.Row, expiry: DispatchWorkItem)?


    var scale: CGFloat { panel.screen?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 2 }

    init(_ island: Island, seq: Int) {
        self.id = island.id
        self.island = island
        self.seq = seq
        super.init()
        view.controller = self
        root.isGeometryFlipped = true
        root.noImplicitAnimations()
        view.layer = root
        view.wantsLayer = true
        container.wantsLayer = true
        view.autoresizingMask = [.width, .height]
        container.addSubview(view)
        panel.contentView = container
        panel.onCancel = { [weak self] in self?.closeViewer() }

        shape.backgroundColor = NSColor.black.cgColor
        shape.borderColor = Look.hairline
        shape.borderWidth = Look.hairlineWidth
        shape.cornerCurve = .continuous
        shape.masksToBounds = true
        shape.noImplicitAnimations()
        root.addSublayer(shape)
        header.anchorPoint = .zero
        header.noImplicitAnimations()
        shape.addSublayer(header)
        bodyHost.anchorPoint = .zero
        bodyHost.opacity = 0
        bodyHost.noImplicitAnimations()
        bodyGroup.anchorPoint = CGPoint(x: 0.5, y: 0)
        bodyGroup.noImplicitAnimations()
        bodyGroup.addSublayer(bodyHost)
        shape.addSublayer(bodyGroup)
        buildHeader(previous: nil)
    }

    deinit { clockTimer?.cancel(); attentionTimer?.cancel(); workTimer?.cancel(); confirming?.expiry.cancel() }

    // MARK: Sizes

    private var headerWidth: CGFloat { Look.iconSize + Look.headerGap + (titleLayer?.bounds.width ?? 0) }
    private var trailingWidth: CGFloat { trailingLayer?.bounds.width ?? 0 }

    var pillSize: CGSize {
        let w = headerWidth + (trailingInPill ? trailingWidth + 7 : 0) + 2 * Look.pillPad
        return CGSize(width: ceil(max(Look.pillH, w)), height: Look.pillH)
    }

    var expandedWidth: CGFloat {
        let headerNeed = headerWidth + 2 * Look.pad + Self.closeRoom
        var want = CGFloat(island.width ?? Double(Look.expandedWidth))
        if island.width == nil, ["note", "list"].contains(island.look), island.summary == nil, island.steps.isEmpty, island.items.isEmpty, island.facts.isEmpty,
           island.image == nil, island.open == nil, !island.parts.isEmpty, island.parts.allSatisfy({ ($0["type"] as? String ?? "text") == "text" }) {
            // A few lines kept in view: as wide as the longest line, not a fixed panel.
            let widest = island.parts.flatMap { (($0["value"] ?? $0["text"]) as? String ?? "").components(separatedBy: "\n") }
                .map { ceil(($0 as NSString).size(withAttributes: [.font: Look.body]).width) }.max() ?? 0
            want = min(want, max(200, widest + 2 * Look.pad + 4))
        }
        return ceil(max(want, headerNeed, pillSize.width))
    }

    /// The pill's trailing word (stage, %, value) is in the open island's status line, so it is not repeated in its header.
    private let trailingOpen = false

    var expandedSize: CGSize {
        if isViewer { return viewerSize() }
        let b = ensureBody()
        return CGSize(width: expandedWidth, height: min(ceil(Look.bodyTop + b.height + Look.bottomPad), floor(heightCap)))
    }

    /// The open body's visible height and how far it can scroll.
    private var viewportHeight: CGFloat { max(0, expandedSize.height - Look.bodyTop - Look.bottomPad) }
    private var maxScroll: CGFloat { max(0, ceil(body?.height ?? 0) - viewportHeight) }
    var scrolls: Bool { !isViewer && maxScroll > 0.5 }

    var size: CGSize { expanded ? expandedSize : pillSize }
    /// What the automatic column reserves: the island as it is now, so an open island pushes the ones below it down
    /// instead of covering them. An open viewer leaves the column for the middle of the screen; its slot stays a pill.
    var slotSize: CGSize { isViewer ? pillSize : size }

    private func rect(for size: CGSize) -> CGRect {
        if isViewer && expanded && !screenFrame.isEmpty {
            // An open viewer sits in the middle of the screen (or where it was carried); closed, it is a pill.
            if let p = island.raw["openPosition"] as? JSONObject, let x = (p["x"] as? NSNumber)?.doubleValue, let y = (p["y"] as? NSNumber)?.doubleValue {
                let r = CGRect(x: screenFrame.midX + CGFloat(x) - size.width / 2, y: screenFrame.maxY - CGFloat(y) - size.height, width: size.width, height: size.height)
                return CGRect(x: round(min(max(r.minX, screenFrame.minX + 8), screenFrame.maxX - 8 - r.width)),
                              y: round(min(max(r.minY, screenFrame.minY + 8), min(screenFrame.maxY, rowTop) - r.height)), width: r.width, height: r.height)
            }
            return CGRect(x: round(screenFrame.midX - size.width / 2), y: round(screenFrame.midY - size.height / 2 + 20), width: size.width, height: size.height)
        }
        var r = CGRect(x: anchor.x - size.width / 2, y: anchor.y - size.height, width: size.width, height: size.height)
        if !screenFrame.isEmpty {
            r.origin.x = min(max(r.minX, screenFrame.minX + 8), screenFrame.maxX - 8 - r.width)
            r.origin.y = min(max(r.minY, screenFrame.minY + 8), min(screenFrame.maxY, rowTop) - r.height)
        }
        return CGRect(x: round(r.minX), y: round(r.minY), width: r.width, height: r.height)
    }

    // MARK: Header (icon · title · trailing value)

    private func buildHeader(previous: Island?) {
        let s = scale
        let look = island.look
        let keepRing = previous?.look == "task" && look == "task" && ring != nil && (previous?.progress == nil) == (island.progress == nil)
        if keepRing, let ring, let p = island.progress {
            ring.spring("strokeEnd", to: CGFloat(p), .grow)
        } else if previous == nil || previous?.look != look || previous?.icon != island.icon || (previous?.progress == nil) != (island.progress == nil)
                    || (look == "list" && !((previous?.parts ?? []) as NSArray).isEqual(to: island.parts))
                    || (look == "todo" && previous?.waitsOnOwner != island.waitsOnOwner) || iconAttention != attentionShown {
            if let old = icon {
                // Replace: the old symbol shrinks away while the new one bounces in.
                CATransaction.begin()
                CATransaction.setCompletionBlock { old.removeFromSuperlayer() }
                let g = CAAnimationGroup()
                let sc = CABasicAnimation(keyPath: "transform.scale"); sc.toValue = 0.3
                let op = CABasicAnimation(keyPath: "opacity"); op.toValue = 0
                g.animations = [sc, op]; g.duration = 0.14; g.fillMode = .forwards; g.isRemovedOnCompletion = false
                old.add(g, forKey: "replaceOut")
                CATransaction.commit()
            }
            pulse = nil
            spinner = nil
            ring = nil
            // Only the attention changed (bell or question mark on or off): the waiting dot's arrival pulse is not replayed,
            // nothing new has arrived.
            let attentionOnly = previous != nil && previous?.look == look && previous?.icon == island.icon
                && (previous?.progress == nil) == (island.progress == nil) && iconAttention != attentionShown
                && !(look == "list" && !((previous?.parts ?? []) as NSArray).isEqual(to: island.parts))
                && !(look == "todo" && previous?.waitsOnOwner != island.waitsOnOwner)
            iconAttention = attentionShown
            let l = makeIcon(look, scale: s)
            if attentionOnly, let (halo, _) = pulse { halo.removeAnimation(forKey: "pulse") }
            l.position = CGPoint(x: Look.iconSize / 2, y: Look.pillH / 2)
            header.addSublayer(l)
            icon = l
            if previous != nil || placed { arrive(l, look: look) }
        }

        let title = island.title
        if titleLayer.map({ ($0.string as? NSAttributedString)?.string != title }) ?? true {
            let t = textLayer(title, Look.title, .white, maxWidth: Look.maxPillTitle, scale: s)
            t.position = CGPoint(x: Look.iconSize + Look.headerGap, y: (Look.pillH - t.bounds.height) / 2)
            if let old = titleLayer { crossfade(old, t, in: header) } else { header.addSublayer(t) }
            titleLayer = t
        }
        header.bounds = CGRect(x: 0, y: 0, width: headerWidth, height: Look.pillH)
        refreshWorking()

        // Trailing: the percentage while running (open only), the headline value for a result (also in the pill).
        var trailing: String?
        trailingInPill = false
        switch look {
        case "task":
            trailing = island.progress.map { "\(Int(($0 * 100).rounded()))%" } ?? island.stage
            trailingInPill = trailing != nil
        case "todo":
            // How many are still open.
            let open = island.todoItems.filter { !$0.done }.count
            trailing = open > 0 ? "\(open)" : nil
            trailingInPill = trailing != nil
        case "result":
            trailing = island.value ?? island.parts.first { $0["type"] as? String == "stat" }.flatMap { p in
                (p["value"] as? String) ?? (p["value"] as? NSNumber).map { Island.format($0.doubleValue) }
            }
            trailingInPill = trailing != nil
        default: break
        }
        let oldText = (trailingLayer?.string as? NSAttributedString)?.string
        if oldText != trailing {
            let old = trailingLayer
            if let trailing {
                let t = textLayer(trailing, island.progress == nil && look == "task" ? Look.title : Look.trailing,
                                  look == "result" ? .white : Look.ink2, scale: s)
                if let old { t.position = old.position; t.opacity = old.opacity; crossfade(old, t, in: shape) } else { t.opacity = 0; shape.addSublayer(t) }
                trailingLayer = t
            } else {
                old?.removeFromSuperlayer()
                trailingLayer = nil
            }
        }
    }

    private func crossfade(_ old: CALayer, _ new: CALayer, in parent: CALayer) {
        parent.addSublayer(new)
        new.fade(to: new.opacity, duration: 0.2, delay: 0.04)
        let a = CABasicAnimation(keyPath: "opacity")
        a.fromValue = old.presentation()?.opacity ?? old.opacity
        a.toValue = 0
        a.duration = 0.14
        CATransaction.begin()
        CATransaction.setCompletionBlock { old.removeFromSuperlayer() }
        old.opacity = 0
        old.add(a, forKey: "out")
        CATransaction.commit()
    }

    private func makeIcon(_ look: String, scale s: CGFloat) -> CALayer {
        // State language: running = the task's own symbol, white and still; waiting on the user = orange; done = green
        // check; failed = red cross. A live progress value replaces the symbol with a ring. Nothing loops: a running
        // island shows life only when the agent changes it (one breath, see `nudge`).
        // Attention while closed: a bell (orange) when it waits on the user, a question mark (blue) for a question.
        if attentionShown == "question" { return symbolLayer("questionmark.circle.fill", size: Look.iconSize + 1, colors: [.white, Look.blue], scale: s) }
        if attentionShown == "waiting" { return symbolLayer("bell.fill", size: Look.iconSize, colors: [Look.orange], scale: s) }
        // The plan lists keep their fixed icons; Settled's is green.
        if look == "todo", let list = island.list {
            let color = list == "settled" ? Look.green : (island.waitsOnOwner ? Look.orange : .white)
            return symbolLayer(PlanList.icon(list), size: Look.iconSize, colors: [color], scale: s)
        }
        // Every other island may draw its own icon (an SF Symbol or a tiny SVG), in its state's colour: white while
        // running, orange while waiting on the user. Done and failed show the green check and red cross.
        if let name = island.icon {
            let color: NSColor? = switch look {
            // Its own icon also over a progress value: the percentage is beside it and the bar is in the body.
            case "task": .white
            case "waiting": Look.orange
            case "list": island.chosen != nil || island.parts.contains { $0["type"] as? String == "list" && $0["selected"] != nil } ? .white : Look.orange
            case "note", "viewer": .white
            default: nil
            }
            if let color, let l = iconLayer(name, size: Look.iconSize, color: color, scale: s) { return l }
        }
        switch look {
        case "task":
            let c = CALayer()
            c.bounds = CGRect(x: 0, y: 0, width: Look.iconSize, height: Look.iconSize)
            c.noImplicitAnimations()
            let path = CGMutablePath()
            path.addArc(center: CGPoint(x: Look.iconSize / 2, y: Look.iconSize / 2), radius: Look.iconSize / 2 - 1.6, startAngle: -.pi / 2, endAngle: 1.5 * .pi, clockwise: false)
            let track = CAShapeLayer()
            track.path = path
            track.lineWidth = 2.8
            track.strokeColor = Look.white(0.22).cgColor
            track.fillColor = nil
            track.frame = c.bounds
            track.noImplicitAnimations()
            c.addSublayer(track)
            let arc = CAShapeLayer()
            arc.path = path
            arc.lineWidth = 2.8
            arc.lineCap = .round
            arc.strokeColor = NSColor.white.cgColor
            arc.fillColor = nil
            arc.frame = c.bounds
            arc.noImplicitAnimations()
            c.addSublayer(arc)
            if let p = island.progress {
                arc.strokeEnd = CGFloat(p)
                ring = arc
            } else {
                // No measurable progress: a still quarter arc that turns once whenever the agent reports a change.
                arc.strokeEnd = 0.26
                spinner = arc
            }
            return c
        case "waiting":
            let c = CALayer()
            c.bounds = CGRect(x: 0, y: 0, width: Look.iconSize, height: Look.iconSize)
            c.noImplicitAnimations()
            let halo = CALayer()
            let d: CGFloat = 11, o = (Look.iconSize - d) / 2
            halo.frame = CGRect(x: o, y: o, width: d, height: d)
            halo.cornerRadius = d / 2
            halo.backgroundColor = Look.orange.cgColor
            halo.opacity = 0
            halo.noImplicitAnimations()
            c.addSublayer(halo)
            let dot = CALayer()
            dot.frame = CGRect(x: o, y: o, width: d, height: d)
            dot.cornerRadius = d / 2
            dot.backgroundColor = Look.orange.cgColor
            dot.noImplicitAnimations()
            c.addSublayer(dot)
            // Soft pulse when it starts waiting (or the question changes): a ring of the same orange widening and fading
            // twice, then a still dot.
            let grow = CABasicAnimation(keyPath: "transform.scale")
            grow.fromValue = 1
            grow.toValue = 2.3
            let fade = CABasicAnimation(keyPath: "opacity")
            fade.fromValue = 0.55
            fade.toValue = 0
            let g = CAAnimationGroup()
            g.animations = [grow, fade]
            g.duration = 1.8
            g.repeatCount = 2
            g.timingFunction = CAMediaTimingFunction(name: .easeOut)
            halo.add(g, forKey: "pulse")
            pulse = (halo, g)
            return c
        case "result": return symbolLayer("checkmark.circle.fill", size: Look.iconSize + 1, colors: [.white, Look.green], scale: s)
        case "todo": return symbolLayer("checklist", size: Look.iconSize, colors: [island.waitsOnOwner ? Look.orange : .white], scale: s)
        case "failed": return symbolLayer("xmark.circle.fill", size: Look.iconSize + 1, colors: [.white, Look.red], scale: s)
        case "viewer":
            let name = FileViewer.symbol(fileViewer?.kind ?? ViewerKind.detect(ViewerKind.url(for: island.path ?? "")))   // Viewer.swift
            return symbolLayer(name, size: Look.iconSize, colors: [.white], scale: s)
        case "list":
            // Orange until the user picks: it is waiting on him.
            let picked = island.chosen != nil || island.parts.contains { $0["type"] as? String == "list" && $0["selected"] != nil }
            return symbolLayer("list.bullet", size: Look.iconSize, colors: [picked ? .white : Look.orange], scale: s)
        default: return symbolLayer("pin.fill", size: Look.iconSize - 1, colors: [.white], scale: s)
        }
    }

    /// Running: every `workInterval` seconds the arc turns once (or the island's own icon, or its progress ring,
    /// takes one short breath); nothing moves in between, and nothing at all once it stops running.
    private func refreshWorking() {
        guard island.look == "task", attentionShown == nil, !removing else {
            workTimer?.cancel()
            workTimer = nil
            return
        }
        guard workTimer == nil else { return }
        let t = DispatchSource.makeTimerSource(queue: .main)
        t.schedule(deadline: .now() + Self.workInterval, repeating: Self.workInterval, leeway: .milliseconds(400))
        t.setEventHandler { [weak self] in self?.workPulse() }
        t.resume()
        workTimer = t
    }

    func workPulse() {
        guard island.look == "task", placed, !removing else { return }
        if let arc = spinner, arc.superlayer != nil {
            let spin = CABasicAnimation(keyPath: "transform.rotation.z")
            spin.fromValue = 0
            spin.toValue = 2 * Double.pi
            spin.duration = 0.9
            spin.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
            arc.add(spin, forKey: "work")
        } else if let target = ring ?? icon {
            let a = CABasicAnimation(keyPath: "opacity")
            a.fromValue = 1
            a.toValue = 0.45
            a.duration = 0.45
            a.autoreverses = true
            a.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
            target.add(a, forKey: "work")
        }
    }

    /// One sign of life when the agent changes a running or waiting island: the arc turns once, the symbol takes
    /// one breath, the waiting dot pulses. Then everything is still again.
    private func nudge() {
        if let arc = spinner, arc.superlayer != nil {
            let spin = CABasicAnimation(keyPath: "transform.rotation.z")
            spin.fromValue = 0
            spin.toValue = 2 * Double.pi
            spin.duration = 0.9
            spin.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
            arc.add(spin, forKey: "spin")
        } else if let (halo, g) = pulse, halo.superlayer != nil {
            halo.add(g, forKey: "pulse")
        } else if let icon, ring == nil {
            let a = CABasicAnimation(keyPath: "opacity")
            a.fromValue = 1
            a.toValue = 0.4
            a.duration = 0.45
            a.autoreverses = true
            a.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
            icon.add(a, forKey: "breath")
        }
    }

    private func arrive(_ l: CALayer, look: String) {
        if look == "failed" {
            let k = CAKeyframeAnimation(keyPath: "transform.rotation.z")
            k.values = [0, 0.22, -0.18, 0.12, -0.06, 0]
            k.duration = 0.5
            l.add(k, forKey: "wiggle")
        } else {
            let b = Spring(response: 0.3, damping: 0.5).animation("transform.scale", from: 0.3, to: 1)
            b.beginTime = CACurrentMediaTime() + 0.08
            b.fillMode = .backwards
            l.add(b, forKey: "bounce")
        }
    }

    private var spinner: CAShapeLayer?
    private var pulse: (CALayer, CAAnimation)?

    // MARK: Body

    @discardableResult
    private func ensureBody() -> Body {
        if let body { return body }
        if isViewer {
            let b = Body(layer: CALayer(), height: 0, regions: [])
            body = b
            return b
        }
        let attrs = try? FileManager.default.attributesOfItem(atPath: Paths.file(id).path)
        let b = buildBody(island, width: expandedWidth - 2 * Look.pad, scale: scale,
                          born: attrs?[.creationDate] as? Date, written: attrs?[.modificationDate] as? Date, openItems: openItems)
        bodyHost.sublayers?.forEach { $0.removeFromSuperlayer() }
        bodyHost.addSublayer(b.layer)
        bodyHost.position = CGPoint(x: Look.pad, y: Look.bodyTop)
        body = b
        layoutViewport()
        reattachConfirm()
        return b
    }

    // MARK: Scrolling: an island taller than its display scrolls inside (trackpad momentum and rubber band from the
    // system's own scroll events, a wheel in short eased steps, an overlay indicator that fades once it stops).

    /// The body's window onto its content: clipped to the visible height and offset by the scroll, with soft edges
    /// where more content continues.
    private func layoutViewport() {
        guard let body else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        if scrolls {
            scrollY = min(max(scrollY, 0), maxScroll)
            bodyHost.masksToBounds = true
            bodyHost.bounds = CGRect(x: 0, y: scrollY, width: body.layer.bounds.width, height: viewportHeight)
        } else {
            scrollY = 0
            bodyHost.masksToBounds = false
            bodyHost.bounds = body.layer.bounds
        }
        CATransaction.commit()
        updateScrollEdges()
    }

    /// Soft fades at the edges where content continues past them; none at an end. Redrawn only while scrolling.
    private func updateScrollEdges() {
        guard scrolls else { bodyHost.mask = nil; scrollFade = nil; return }
        let f = scrollFade ?? CAGradientLayer()
        f.noImplicitAnimations()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        let b = bodyHost.bounds
        f.frame = b
        let h = max(b.height, 1), edge = min(18 / h, 0.2)
        let top: CGFloat = scrollY > 1 ? 0 : 1, bottom: CGFloat = scrollY < maxScroll - 1 ? 0 : 1
        let c = { (a: CGFloat) in NSColor(white: 0, alpha: a).cgColor }
        f.colors = [c(top), c(1), c(1), c(bottom)]
        f.locations = [0, NSNumber(value: Double(edge)), NSNumber(value: Double(1 - edge)), 1]
        CATransaction.commit()
        if scrollFade == nil { scrollFade = f; bodyHost.mask = f }
    }

    /// Lab and pointer: put the body at `y` (no animation).
    func setScroll(_ y: CGFloat) {
        scrollY = y
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        bodyHost.removeAnimation(forKey: "scroll")
        scrollFade?.removeAnimation(forKey: "scroll")
        bodyHost.bounds.origin.y = y
        CATransaction.commit()
        updateScrollEdges()
        showIndicator()
    }

    /// Glide to `y` (a wheel step, a spring back from the rubber band, a row brought into view).
    private func animateScroll(to y: CGFloat, _ s: Spring = .calm) {
        let from = bodyHost.presentation()?.bounds.origin.y ?? bodyHost.bounds.origin.y
        scrollY = y
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        bodyHost.bounds.origin.y = y
        CATransaction.commit()
        bodyHost.add(Self.timed(s.animation("bounds.origin.y", from: from, to: y), bodyHost), forKey: "scroll")
        updateScrollEdges()
        // The edge fades ride with the content on the same spring (the mask lives in the scrolled coordinates).
        if let f = scrollFade {
            let h = f.bounds.height / 2
            f.add(Self.timed(s.animation("position.y", from: from + h, to: y + h), f), forKey: "scroll")
        }
        showIndicator()
    }

    /// A scroll event over the island: true when the open body took it.
    func scrollWheel(_ e: NSEvent) -> Bool {
        guard expanded, scrolls, !dragging else { return false }
        let maxY = maxScroll
        if e.hasPreciseScrollingDeltas {
            // Trackpad or Magic Mouse: follow the fingers, then the system's own momentum events; past an end the
            // content gives way with resistance and springs back when the gesture and its momentum end.
            if e.phase == .began || e.phase == .mayBegin { scrollEndWork?.cancel(); momentumSpent = false }
            if !e.momentumPhase.isEmpty, momentumSpent { return true }
            let d = e.scrollingDeltaY
            let over = scrollY < 0 || scrollY > maxY
            if !e.momentumPhase.isEmpty, over || (scrollY - d < 0 || scrollY - d > maxY) {
                // Momentum running into an end: a short overshoot, then back on the spring; the rest of it is spent.
                momentumSpent = true
                let raw = scrollY - d, end = min(max(raw, 0), maxY)
                setScroll(end + max(-24, min(24, (raw - end) * 0.4)))
                settleScroll()
                return true
            }
            var y = scrollY - (over ? d * 0.4 : d)
            let limit = viewportHeight * 0.18
            y = min(max(y, -limit), maxY + limit)
            if d != 0 { setScroll(y) }
            let ended = e.phase == .ended || e.phase == .cancelled || e.momentumPhase == .ended || e.momentumPhase == .cancelled
            if ended {
                // Wait a moment: momentum may follow the lifted fingers.
                scrollEndWork?.cancel()
                let w = DispatchWorkItem { [weak self] in self?.settleScroll() }
                scrollEndWork = w
                DispatchQueue.main.asyncAfter(deadline: .now() + (e.momentumPhase == .ended ? 0 : 0.06), execute: w)
            } else if e.momentumPhase == .began || e.momentumPhase == .changed {
                scrollEndWork?.cancel()
            }
        } else {
            // A wheel: line steps, eased.
            let step = e.scrollingDeltaY * 24
            guard step != 0 else { return true }
            animateScroll(to: min(max(scrollY - step, 0), maxY))
        }
        return true
    }

    /// Back inside the ends on the grow spring after a rubber band; nothing to do when already inside.
    private func settleScroll() {
        let y = min(max(scrollY, 0), maxScroll)
        if y != scrollY { animateScroll(to: y, .grow) }
    }

    /// Keep `r` (body coordinates) in view: used when a proposal opens below the fold.
    private func scrollToShow(_ r: CGRect) {
        guard scrolls else { return }
        let vh = viewportHeight
        var y = scrollY
        if r.maxY > y + vh { y = r.maxY - vh + 4 }
        if r.minY < y { y = r.minY - 4 }
        y = min(max(y, 0), maxScroll)
        if y != scrollY { animateScroll(to: y, .grow) }
    }

    /// The overlay indicator: a thin capsule at the right edge, shown while scrolling, faded 0.8 s after the last.
    private func showIndicator() {
        guard scrolls, let body else { return }
        let i: CALayer
        if let s = scrollIndicator { i = s } else {
            i = CALayer()
            i.backgroundColor = Look.white(0.42).cgColor
            i.cornerCurve = .continuous
            i.opacity = 0
            i.noImplicitAnimations()
            scrollIndicator = i
        }
        if i.superlayer !== shape { shape.addSublayer(i) }
        let vh = viewportHeight, total = max(body.height, 1)
        let visible = min(max(scrollY, 0), maxScroll) - scrollY   // shrinks while past an end
        let len = max(28, vh * vh / total - abs(visible))
        let travel = vh - len
        let k = maxScroll > 0 ? min(max(scrollY / maxScroll, 0), 1) : 0
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        i.frame = CGRect(x: expandedSize.width - 6, y: Look.bodyTop + travel * k, width: 3, height: len)
        i.cornerRadius = 1.5
        CATransaction.commit()
        if i.opacity < 1 { i.fade(to: 1, duration: 0.1) }
        indicatorWork?.cancel()
        let w = DispatchWorkItem { [weak self] in self?.scrollIndicator?.fade(to: 0, duration: 0.3) }
        indicatorWork = w
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.8, execute: w)
    }

    private func resetScroll() {
        scrollEndWork?.cancel()
        indicatorWork?.cancel()
        scrollIndicator?.removeFromSuperlayer()
        scrollIndicator = nil
        scrollY = 0
        bodyHost.removeAnimation(forKey: "scroll")
    }

    private func showBody(_ on: Bool) {
        if on {
            let b = ensureBody()
            _ = b
            bodyHost.position = CGPoint(x: Look.pad, y: Look.bodyTop)
            scaleBody(open: true)
            bodyHost.fade(to: 1, duration: 0.14, delay: 0.02)
        } else {
            scaleBody(open: false)
            bodyHost.fade(to: 0, duration: 0.09)
        }
        runClock()
    }

    /// While open, the "started 12 min ago" line is redrawn once a minute (one timer; none while it is a pill).
    private func runClock() {
        clockTimer?.cancel()
        clockTimer = nil
        guard expanded, !removing, body?.clock != nil else { return }
        let t = DispatchSource.makeTimerSource(queue: .main)
        t.schedule(deadline: .now() + 60, repeating: 60, leeway: .seconds(5))
        t.setEventHandler { [weak self] in
            guard let self, let c = self.body?.clock, let attr = c.layer.string as? NSAttributedString else { return }
            let text = c.text()
            guard text != attr.string else { return }
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            c.layer.string = NSAttributedString(string: text, attributes: attr.attributes(at: 0, effectiveRange: nil))
            CATransaction.commit()
        }
        t.resume()
        clockTimer = t
    }

    /// The open content grows from the pill's width to its own on the shape's spring (scale = shape width / open width).
    private func scaleBody(open: Bool) {
        let open0 = expandedSize, pill = pillSize
        let k = min(1, pill.width / max(open0.width, 1))
        bodyGroup.bounds = CGRect(origin: .zero, size: open0)
        let s: Spring = open ? .grow : .calm
        // Mid-flight: continue from where it is on screen; otherwise start from the other state.
        let inFlight = bodyGroup.animation(forKey: "scale") != nil
        let fromPos = inFlight ? (bodyGroup.presentation()?.position ?? .zero) : CGPoint(x: (open ? pill.width : open0.width) / 2, y: 0)
        let fromScale = inFlight ? ((bodyGroup.presentation()?.value(forKeyPath: "transform.scale") as? NSNumber)?.doubleValue ?? 1) : Double(open ? k : 1)
        let toPos = CGPoint(x: (open ? open0.width : pill.width) / 2, y: 0)
        let toScale: CGFloat = open ? 1 : k
        bodyGroup.position = toPos
        bodyGroup.setValue(toScale, forKeyPath: "transform.scale")
        bodyGroup.add(s.animation("position", from: NSValue(point: fromPos), to: NSValue(point: toPos)), forKey: "position")
        let sa = s.animation("transform.scale", from: fromScale, to: Double(toScale))
        sa.isRemovedOnCompletion = true
        bodyGroup.add(sa, forKey: "scale")
    }

    // MARK: Placement and motion

    /// Put the island at `anchor` (its top centre) on the given screen.
    func place(anchor a: CGPoint, screen: CGRect, rowTop top: CGFloat, _ spring: Spring = .grow) {
        guard !dragging else { return }
        anchor = CGPoint(x: a.x, y: min(a.y, top))
        screenFrame = screen
        rowTop = top
        let target = rect(for: size)
        if !placed {
            placed = true
            let from: CGRect
            let notch = island.bornFromNotch || fromNotch
            if notch {
                // Out of the notch into its slot on the open spring; then the key goes (it is a birth, not a state).
                from = Self.notchRect(NSScreen.screens.first { $0.frame == screen } ?? NSScreen.main)
                bornFrom = from
                if island.bornFromNotch {
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.7) { [id] in
                        IslandFile.patch(id) { $0.removeValue(forKey: "origin") }
                    }
                }
            } else {
                from = CGRect(x: target.midX - Look.pillH / 2, y: target.maxY - Look.pillH, width: Look.pillH, height: Look.pillH)
                shape.opacity = 0
                shape.fade(to: 1, duration: 0.14)
            }
            header.opacity = 0
            layoutContent(animated: false)
            header.fade(to: 1, duration: 0.22, delay: notch ? 0.18 : 0.1)
            applyLevel()
            if !Self.offscreen {
                panel.orderFrontRegardless()
                OverlaySpace.shared.adopt(panel)
            }
            if isViewer && expanded { viewerOpened(true) }
            morph(to: target, .grow, from: from)
            if expanded { showBody(true) }
            refreshAttention()
            return
        }
        let v = pendingVelocity
        pendingVelocity = nil
        if target != shapeRect { morph(to: target, v == nil ? spring : .toss, velocity: v) }
    }

    /// The one motion: the window becomes the union of where the shape is and where it goes, the shape springs
    /// inside it (render server), and the window shrinks to the shape when the spring settles.
    private func morph(to target: CGRect, _ s: Spring, from explicit: CGRect? = nil, velocity: CGVector? = nil) {
        let from = explicit ?? visibleShapeRect()
        // Whatever motion was running (a throw's per-axis springs or a plain move) gives way to this one, from
        // where the shape is on screen now.
        for k in ["position", "position.x", "position.y"] { shape.removeAnimation(forKey: k) }
        shapeRect = target
        app?.stateChanged()
        generation += 1
        let g = generation
        // A throw overshoots its target a little: the window leaves room for it.
        let room = velocity.map { max(60, 0.015 * hypot($0.dx, $0.dy)) } ?? 0
        let union = from.union(target).insetBy(dx: -room, dy: -room)
        // Safety net, seen once and not reproduced: if a motion is still attached well after it should have
        // settled, land it.
        DispatchQueue.main.asyncAfter(deadline: .now() + (velocity == nil ? 1.0 : 1.8)) { [weak self] in
            guard let self, g == self.generation, !self.removing, self.shape.animationKeys()?.isEmpty == false else { return }
            self.shape.removeAllAnimations()
            self.bodyGroup.removeAllAnimations()
            self.settle(g)
        }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        CATransaction.setCompletionBlock { [weak self] in self?.settle(g) }
        if panel.frame != union { panel.setFrame(union, display: false) }
        view.frame = CGRect(origin: .zero, size: union.size)
        let f = local(from, in: union), t = local(target, in: union)
        shape.bounds = CGRect(origin: .zero, size: t.size)
        shape.position = CGPoint(x: t.midX, y: t.midY)
        shape.cornerRadius = Look.radius(t.height)
        if from.size != target.size {
            shape.add(s.animation("bounds", from: NSValue(rect: CGRect(origin: .zero, size: f.size)), to: NSValue(rect: shape.bounds)), forKey: "bounds")
            shape.add(s.animation("cornerRadius", from: Look.radius(f.height), to: shape.cornerRadius), forKey: "cornerRadius")
        }
        if let v = velocity {
            // Each axis on its own spring, starting at the release velocity (in the layer's y-down space), so the
            // island carries on the way it was thrown and curves into its slot.
            for (key, a, b, speed) in [("position.x", f.midX, t.midX, v.dx), ("position.y", f.midY, t.midY, -v.dy)] where abs(b - a) >= 0.5 {
                shape.add(s.animation(key, from: a, to: b, velocity: abs(b - a) < 2 ? 0 : max(-40, min(40, Double(speed / (b - a))))), forKey: key)
            }
        } else if from.origin != target.origin || from.size != target.size {
            shape.add(s.animation("position", from: NSValue(point: CGPoint(x: f.midX, y: f.midY)), to: NSValue(point: shape.position)), forKey: "position")
        }
        CATransaction.commit()
    }

    private func settle(_ g: Int) {
        guard g == generation, !removing else { return }
        app?.stateChanged()
        defer { if isViewer && expanded { showScroll() } }
        guard panel.frame != shapeRect else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        panel.setFrame(shapeRect, display: false)
        view.frame = CGRect(origin: .zero, size: shapeRect.size)
        shape.bounds = CGRect(origin: .zero, size: shapeRect.size)
        shape.position = CGPoint(x: shapeRect.width / 2, y: shapeRect.height / 2)
        CATransaction.commit()
    }

    private func local(_ r: CGRect, in w: CGRect) -> CGRect {
        CGRect(x: r.minX - w.minX, y: w.maxY - r.maxY, width: r.width, height: r.height)
    }

    func visibleShapeRect() -> CGRect {
        guard placed else { return shapeRect }
        let p = shape.presentation() ?? shape
        let b = p.bounds.size, c = p.position
        let wf = panel.frame
        return CGRect(x: wf.minX + c.x - b.width / 2, y: wf.maxY - c.y - b.height / 2, width: b.width, height: b.height)
    }

    /// Header and trailing positions for the current state; the header glides from centred (pill) to the leading
    /// edge (open) on the shape's own spring.
    private func layoutContent(animated: Bool, _ s: Spring = .grow) {
        let w = size.width
        let total = headerWidth + (trailingInPill ? trailingWidth + 7 : 0)
        let hx = expanded ? Look.pad : (w - total) / 2
        let tx = expanded && trailingOpen ? w - Look.pad - trailingWidth - Self.closeRoom : (w - total) / 2 + headerWidth + 7
        let ty = (Look.pillH - (trailingLayer?.bounds.height ?? 0)) / 2
        let tOpacity: Float = (expanded ? trailingOpen : trailingInPill) ? 1 : 0
        if animated {
            // Dynamic Island style: the pill's content dissolves, the shape springs, the open content resolves in
            // its place. Nothing slides sideways across the island.
            hop(header, to: CGPoint(x: hx, y: 0), opacity: 1)
            if let tl = trailingLayer { hop(tl, to: CGPoint(x: tx, y: ty), opacity: tOpacity) }
        } else {
            header.position = CGPoint(x: hx, y: 0)
            trailingLayer?.position = CGPoint(x: tx, y: ty)
            trailingLayer?.opacity = tOpacity
        }
        layoutClose(animated: animated)
    }

    /// Move a layer while it is invisible: out quickly, jump, back in once the shape has room.
    private func hop(_ l: CALayer, to p: CGPoint, opacity target: Float) {
        let from = l.presentation()?.position ?? l.position
        let o0 = l.presentation()?.opacity ?? l.opacity
        l.position = p
        l.opacity = target
        guard from != p || o0 != target else { return }
        let pos = CAKeyframeAnimation(keyPath: "position")
        pos.values = [NSValue(point: from), NSValue(point: p)]
        pos.keyTimes = [0, 0.3, 1]
        pos.calculationMode = .discrete
        let op = CAKeyframeAnimation(keyPath: "opacity")
        op.values = from == p ? [o0, target] : [o0, 0, 0, target]
        // Opening waits for the shape to make room; closing brings the pill's words back as soon as the body has gone.
        op.keyTimes = from == p ? [0, 1] : (expanded ? [0, 0.14, 0.36, 1] : [0, 0.12, 0.24, 0.7])
        let sc = CAKeyframeAnimation(keyPath: "transform.scale")
        sc.values = from == p ? [1, 1] : [1, 0.94, 0.94, 1]
        sc.keyTimes = op.keyTimes
        let g = CAAnimationGroup()
        g.animations = [pos, op, sc]
        g.duration = expanded ? 0.3 : 0.26
        g.timingFunction = CAMediaTimingFunction(name: .easeOut)
        l.add(g, forKey: "hop")
    }

    /// Open islands sit above pills (a viewer above both), whatever order they were created in.
    private func applyLevel() {
        let base = NSWindow.Level.statusBar.rawValue
        panel.level = NSWindow.Level(rawValue: expanded ? base + (isViewer ? 2 : 1) : base)
    }

    func setInitialExpanded() { expanded = true }

    /// Every island opens: a bare one shows its state, stage and times.
    func setExpanded(_ e: Bool) {
        guard e != expanded else { return }
        expanded = e
        if !e { cancelConfirm(); resetScroll(); CATransaction.begin(); CATransaction.setDisableActions(true); layoutViewport(); CATransaction.commit() }
        guard placed else { return }
        if isViewer { viewerOpened(e) }
        applyLevel()
        let s: Spring = e ? .grow : .calm
        CATransaction.begin()
        layoutContent(animated: true, s)
        showBody(e)
        // In the automatic column the whole column moves at once on the same spring: the islands below slide down
        // as it opens and back up as it closes.
        if !isViewer, let app { app.layoutNow(s) } else { morph(to: rect(for: size), s) }
        CATransaction.commit()
        if e && !Self.offscreen { panel.orderFrontRegardless() }
        refreshAttention()
    }

    // MARK: Updates from the file

    func update(_ new: Island) {
        guard !(new.raw as NSDictionary).isEqual(to: island.raw) else { return }
        let old = island
        // Dropping `origin` after a notch birth changes nothing on screen.
        var a = old.raw, b = new.raw
        a.removeValue(forKey: "origin"); b.removeValue(forKey: "origin")
        if (a as NSDictionary).isEqual(to: b) { island = new; return }
        island = new
        if old.path != new.path || !((old.raw["actions"] as AnyObject?)?.isEqual(new.raw["actions"]) ?? (new.raw["actions"] == nil)) {
            resetViewer()
            if isViewer && expanded && placed { viewerOpened(true); morph(to: rect(for: size), .grow) }
        } else if isViewer, fileViewer?.isStale == true {
            // Shown again (or rewritten) with the same path while the file changed: read it again (bug 22 Oct 20:46).
            reloadViewer()
        }
        CATransaction.begin()
        buildHeader(previous: isViewer && old.path != new.path ? nil : old)   // a viewer's new file: its symbol follows the kind
        let shown = ["parts", "chosen", "progress", "width", "stage", "value", "summary", "steps", "items", "kv", "image", "open", "state", "preset", "updated", "started", "ended", "title"]
        let bodyChanged = old.look != new.look || shown.contains { k in
            switch (old.raw[k], new.raw[k]) {
            case (nil, nil): return false
            case let (a?, b?): return !(a as AnyObject).isEqual(b)
            default: return true
            }
        }
        if ["task", "waiting"].contains(new.look), old.look == new.look, bodyChanged { nudge() }
        // A plan list changed while it is a pill: one breath of its symbol (the open list animates its rows instead).
        if new.look == "todo", !expanded, !((old.raw["items"] as AnyObject?)?.isEqual(new.raw["items"]) ?? (new.raw["items"] == nil)) { nudge() }
        if bodyChanged {
            // The agent changed what it asks: a pending "Sure?" belongs to the old question.
            cancelConfirm()
            let wasShown = body != nil && expanded
            let planBefore = Self.isPlan(old) ? Plan.displayOrder(old.raw, list: old.list ?? "todo") : nil
            let captured = wasShown ? captureRows() : [:]
            body = nil
            if wasShown {
                let b = ensureBody()
                pinBodyOpen()
                if let planBefore, Self.isPlan(new), old.look == new.look {
                    // A plan list: each row animates by its id (added, ticked, moved, removed); the shape resizes on
                    // the same spring through the relayout below.
                    transitionRows(captured, to: b, diff: ItemDiff(old: planBefore, new: Plan.displayOrder(new.raw, list: new.list ?? "todo")))
                } else {
                    let t = CATransition()
                    t.type = .fade
                    t.duration = 0.18
                    bodyHost.add(t, forKey: "swap")
                }
                if old.look != new.look { b.arrivals.forEach { $0() } }
            } else {
                bodyHost.sublayers?.forEach { $0.removeFromSuperlayer() }
            }
            runClock()
        }
        CATransaction.commit()
        // Open or closed is changed only by a click or by the file (the agent), never by a timer or the pointer passing.
        if new.expanded != expanded { setExpanded(new.expanded) } else {
            CATransaction.begin()
            layoutContent(animated: true)
            CATransaction.commit()
        }
        refreshAttention()
        app?.relayout()
    }

    // MARK: Plan rows: keyed layers animated by id

    static func isPlan(_ i: Island) -> Bool { i.list != nil || i.groupedItems != nil }

    /// The open body at full size, no scale animation (after a rebuild while open).
    private func pinBodyOpen() {
        bodyHost.opacity = 1
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        bodyGroup.removeAllAnimations()
        bodyGroup.bounds = CGRect(origin: .zero, size: expandedSize)
        bodyGroup.position = CGPoint(x: expandedSize.width / 2, y: 0)
        bodyGroup.setValue(1, forKeyPath: "transform.scale")
        CATransaction.commit()
    }

    /// The current body's keyed layers with where they are on screen now (mid-slide included).
    private func captureRows() -> [String: (CALayer, CGPoint)] {
        var out: [String: (CALayer, CGPoint)] = [:]
        for (k, l) in body?.rows ?? [:] { out[k] = (l, l.presentation()?.position ?? l.position) }
        return out
    }

    /// From the old rows to the new, on Heimdall's grow spring: rows that stay slide from where they were (a ticked
    /// one sinks to the bottom of its group, a moved one to its new group); a row whose look changed cross-fades
    /// while it slides; a new row slides in with a soft highlight that fades; a removed row fades where it was while
    /// the rest close up. Every animation ends; nothing is left running.
    private func transitionRows(_ old: [String: (CALayer, CGPoint)], to b: Body, diff: ItemDiff) {
        let s = Spring.grow
        let added = Set(diff.added.map { "row:" + $0 }), moved = Set(diff.moved.map { "row:" + $0 })
        // Rows travelling against the flow (ticked, moved) pass over the rows they cross.
        let crossing = moved.union(diff.ticked.map { "row:" + $0 })
        func adopt(_ l: CALayer, at p: CGPoint) {
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            l.removeFromSuperlayer()
            l.removeAnimation(forKey: "slide")
            l.position = p
            b.layer.insertSublayer(l, at: 0)
            CATransaction.commit()
        }
        func fadeAway(_ l: CALayer, _ d: Double) {
            CATransaction.begin()
            CATransaction.setCompletionBlock { l.removeFromSuperlayer() }
            l.fade(to: 0, duration: d)
            CATransaction.commit()
        }
        func slide(_ l: CALayer, from: CGPoint, to: CGPoint) {
            guard from != to else { return }
            l.add(Self.timed(s.animation("position", from: NSValue(point: from), to: NSValue(point: to)), l), forKey: "slide")
        }
        CATransaction.begin()
        for (key, layer) in b.rows {
            let target = layer.position
            layer.zPosition = crossing.contains(key) ? 1 : 0
            if let (o, from) = old[key] {
                slide(layer, from: from, to: target)
                if (o.value(forKey: "fiSig") as? String) != (layer.value(forKey: "fiSig") as? String) {
                    adopt(o, at: from)
                    o.position = target
                    slide(o, from: from, to: target)
                    fadeAway(o, 0.16)
                    layer.opacity = 0
                    layer.fade(to: 1, duration: 0.26, delay: 0.02)
                }
                if moved.contains(key) { highlight(layer, strength: 0.09) }
            } else {
                layer.opacity = 0
                layer.fade(to: 1, duration: 0.24, delay: 0.04)
                if added.contains(key) {
                    slide(layer, from: CGPoint(x: target.x - 18, y: target.y), to: target)
                    highlight(layer, strength: 0.14)
                } else if key.hasPrefix("prop:") {
                    slide(layer, from: CGPoint(x: target.x, y: target.y - 8), to: target)
                }
            }
        }
        for (key, (o, from)) in old where b.rows[key] == nil {
            adopt(o, at: from)
            fadeAway(o, 0.2)
        }
        CATransaction.commit()
    }

    /// Starts now on the layer's own clock (an explicit begin time, so the lab's at-rest check sees it while it runs).
    static func timed<A: CAAnimation>(_ a: A, _ l: CALayer) -> A {
        a.beginTime = l.convertTime(CACurrentMediaTime(), from: nil)
        a.fillMode = .backwards
        return a
    }

    /// A soft light behind a row that fades out (about 1.4 s), then goes.
    private func highlight(_ row: CALayer, strength: CGFloat) {
        let h = CALayer()
        h.frame = row.bounds.insetBy(dx: -6, dy: 0)
        h.cornerRadius = 8
        h.cornerCurve = .continuous
        h.backgroundColor = Look.white(strength).cgColor
        h.opacity = 0
        h.noImplicitAnimations()
        row.insertSublayer(h, at: 0)
        let a = CAKeyframeAnimation(keyPath: "opacity")
        a.values = [0, 1, 1, 0]
        a.keyTimes = [0, 0.12, 0.4, 1]
        a.duration = 1.4
        a.timingFunction = CAMediaTimingFunction(name: .easeOut)
        CATransaction.begin()
        CATransaction.setCompletionBlock { h.removeFromSuperlayer() }
        h.add(Self.timed(a, h), forKey: "highlight")
        CATransaction.commit()
    }

    /// The clock moved a Scheduled label on (due times count down): an open list redraws its rows in place (changed
    /// rows cross-fade); a pill shows no labels and does nothing.
    func refreshPlanLabels() {
        guard expanded, body != nil, Self.isPlan(island) else { return }
        let captured = captureRows()
        body = nil
        let b = ensureBody()
        pinBodyOpen()
        transitionRows(captured, to: b, diff: ItemDiff())
    }

    /// Rebuild the open plan body after an app-side change (a proposal opened or closed), animating the rows.
    private func rebuildPlan() {
        guard expanded, body != nil, Self.isPlan(island) else { return }
        let captured = captureRows()
        body = nil
        let b = ensureBody()
        pinBodyOpen()
        transitionRows(captured, to: b, diff: ItemDiff())
        app?.layoutNow(.grow)
    }

    // MARK: Attention

    /// Bell or question mark while closed, until the user opens it or the key goes. A new attention (another waiting
    /// item, a new question) shows again after he has seen the last one.
    func refreshAttention() {
        guard !removing else { return }
        let sig = Attention.signature(island), kind = Attention.of(island)
        if sig == nil || expanded { seenAttention = sig }
        let show = (kind != nil && !expanded && sig != seenAttention) ? kind : nil
        if show != attentionShown {
            attentionShown = show
            if placed {
                CATransaction.begin()
                buildHeader(previous: island)
                CATransaction.commit()
                app?.relayout()
            }
        }
        guard show != nil else {
            attentionTimer?.cancel()
            attentionTimer = nil
            return
        }
        guard attentionTimer == nil else { return }
        let t = DispatchSource.makeTimerSource(queue: .main)
        t.schedule(deadline: .now() + 0.4, repeating: Attention.interval, leeway: .milliseconds(250))
        t.setEventHandler { [weak self] in self?.pulseAttention() }
        t.resume()
        attentionTimer = t
    }

    /// One pulse: a soft glow of the attention colour across the pill and its edge, and the symbol rings (bell) or
    /// bounces (question mark). About a second, then still until the next one.
    func pulseAttention() {
        guard let kind = attentionShown, !expanded, placed, !removing else { return }
        let color = kind == "question" ? Look.blue : Look.orange
        pulseCount += 1
        let g: CALayer
        if let glow { g = glow } else {
            g = CALayer()
            g.noImplicitAnimations()
            g.cornerCurve = .continuous
            g.borderWidth = 1.5
            g.opacity = 0
            let grad = CAGradientLayer()
            grad.type = .radial
            grad.noImplicitAnimations()
            g.addSublayer(grad)
            glow = g
        }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        let b = shape.bounds
        g.frame = b
        g.cornerRadius = shape.cornerRadius
        g.borderColor = color.withAlphaComponent(0.9).cgColor
        if let grad = g.sublayers?.first as? CAGradientLayer {
            grad.frame = b
            grad.colors = [color.withAlphaComponent(0.45).cgColor, color.withAlphaComponent(0).cgColor]
            let cx = (header.position.x + Look.iconSize / 2) / max(b.width, 1)
            grad.startPoint = CGPoint(x: cx, y: 0.5)
            grad.endPoint = CGPoint(x: cx + 90 / max(b.width, 1), y: 0.5 + 90 / max(b.height, 1))
        }
        if g.superlayer !== shape { shape.insertSublayer(g, at: 0) }
        CATransaction.commit()
        let a = CAKeyframeAnimation(keyPath: "opacity")
        a.values = [0, 1, 0]
        a.keyTimes = [0, 0.22, 1]
        a.duration = 1.3
        a.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
        g.add(Self.timed(a, g), forKey: "attnGlow")
        guard let icon else { Self.labPulse?(self); return }
        if kind == "question" {
            let y = CAKeyframeAnimation(keyPath: "transform.translation.y")
            y.values = [0, -5, 0, -2, 0]
            let sc = CAKeyframeAnimation(keyPath: "transform.scale")
            sc.values = [1, 1.14, 0.97, 1.04, 1]
            y.keyTimes = [0, 0.3, 0.55, 0.78, 1]
            sc.keyTimes = y.keyTimes
            let grp = CAAnimationGroup()
            grp.animations = [y, sc]
            grp.duration = 0.7
            grp.timingFunction = CAMediaTimingFunction(name: .easeOut)
            icon.add(Self.timed(grp, icon), forKey: "attnRing")
        } else {
            let r = CAKeyframeAnimation(keyPath: "transform.rotation.z")
            r.values = [0, 0.42, -0.36, 0.24, -0.13, 0.05, 0]
            r.duration = 0.8
            r.timingFunction = CAMediaTimingFunction(name: .easeOut)
            icon.add(Self.timed(r, icon), forKey: "attnRing")
        }
        Self.labPulse?(self)
    }

    /// Lab: called right after each attention pulse starts (the self-test records it with what is animating).
    static var labPulse: ((IslandController) -> Void)?

    /// The notch in screen coordinates: the gap between the screen's two unobscured top areas; on a display without
    /// one, a small rect at the top centre.
    static func notchRect(_ screen: NSScreen?) -> CGRect {
        guard let screen else { return .zero }
        let f = screen.frame
        if let l = screen.auxiliaryTopLeftArea, let r = screen.auxiliaryTopRightArea, r.minX > l.maxX {
            let h = max(screen.safeAreaInsets.top, l.height)
            return CGRect(x: f.minX + l.maxX, y: f.maxY - h, width: r.minX - l.maxX, height: h)
        }
        return CGRect(x: f.midX - 60, y: f.maxY - 24, width: 120, height: 24)
    }


    func remove(_ done: @escaping () -> Void) {
        let r = visibleShapeRect()
        let dot = CGRect(x: r.midX - Look.pillH / 2, y: r.maxY - Look.pillH, width: Look.pillH, height: Look.pillH)
        header.fade(to: 0, duration: 0.1)
        bodyHost.fade(to: 0, duration: 0.1)
        trailingLayer?.fade(to: 0, duration: 0.1)
        morph(to: dot, .calm)
        removing = true
        resetViewer()
        clockTimer?.cancel()
        clockTimer = nil
        attentionTimer?.cancel()
        attentionTimer = nil
        workTimer?.cancel()
        workTimer = nil
        CATransaction.begin()
        CATransaction.setCompletionBlock { [weak self] in
            self?.panel.orderOut(nil)
            done()
        }
        shape.fade(to: 0, duration: 0.22, delay: 0.1)
        CATransaction.commit()
    }

    // MARK: Pointer

    /// The control under a point in the view (flipped, window-local).
    private func control(at p: CGPoint) -> (Control, CGPoint)? {
        guard expanded, let body, bodyHost.opacity > 0, let q = bodyPoint(p) else { return nil }
        if let (_, row, _) = confirming, Confirm.hit(row, q) != .outside { return nil }
        return body.regions.first { $0.rect.contains(q) }.map { ($0.control, q) }
    }

    /// A view point in body coordinates (scroll included); nil outside the visible body.
    private func bodyPoint(_ p: CGPoint) -> CGPoint? {
        let origin = CGPoint(x: shape.frame.minX + Look.pad, y: shape.frame.minY + Look.bodyTop)
        let v = CGPoint(x: p.x - origin.x, y: p.y - origin.y)
        if scrolls, v.y < -4 || v.y > viewportHeight + 4 { return nil }
        return CGPoint(x: v.x, y: v.y + scrollY)
    }

    /// The pointer in screen coordinates at the event's time. The event's own global location is used, not the window-relative one:
    /// the window moves under the pointer during a drag, and a window-relative point read against the moved window jitters.
    private func screenPoint(_ e: NSEvent) -> CGPoint {
        if let cg = e.cgEvent, let h = NSScreen.screens.first?.frame.height { return CGPoint(x: cg.location.x, y: h - cg.location.y) }
        return panel.convertPoint(toScreen: e.locationInWindow)
    }

    func mouseDown(_ e: NSEvent) {
        let p = view.convert(e.locationInWindow, from: nil)
        if expanded, let c = closeLayer, c.opacity > 0,
           c.frame.insetBy(dx: -6, dy: -6).offsetBy(dx: shape.frame.minX, dy: shape.frame.minY).contains(p) {
            downScreen = nil
            closeIsland()
            return
        }
        // An open viewer is carried by its title row (its text area scrolls and selects).
        if isViewer && expanded && p.y - shape.frame.minY > Look.pillH + 8 { downScreen = nil; return }
        downScreen = screenPoint(e)
        samples = [(e.timestamp, downScreen!)]
        downAnchor = anchor
        dragging = false
        pressed = nil
        sliderDrag = nil
        if let (c, q) = control(at: view.convert(e.locationInWindow, from: nil)) {
            if case let .slider(id, _, _, _) = c { sliderDrag = id; moveSlider(id, x: q.x, commit: false) } else { pressed = c; showPress(c) }
        }
    }

    func mouseDragged(_ e: NSEvent) {
        guard let start = downScreen else { return }
        let p = screenPoint(e)
        if let id = sliderDrag, let (_, q) = controlPoint(e) { moveSlider(id, x: q.x, commit: false); return }
        if !dragging && hypot(p.x - start.x, p.y - start.y) < 3 { return }
        showPress(nil)
        if !dragging {
            // Picked up: open or closed, it stays exactly as it is; only its position follows the pointer.
            dragging = true
            downPillAnchor = anchor
            downAnchor = CGPoint(x: shapeRect.midX, y: shapeRect.maxY)
        }
        pressed = nil
        samples.append((e.timestamp, p))
        if samples.count > 12 { samples.removeFirst(samples.count - 12) }
        anchor = CGPoint(x: downAnchor.x + p.x - start.x, y: downAnchor.y + p.y - start.y)
        // Direct manipulation: the island sits under the pointer, no spring.
        generation += 1
        shape.removeAllAnimations()
        let r = CGRect(x: round(anchor.x - size.width / 2), y: round(anchor.y - size.height), width: size.width, height: size.height)
        shapeRect = r
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        panel.setFrame(r, display: false)
        view.frame = CGRect(origin: .zero, size: r.size)
        shape.bounds = CGRect(origin: .zero, size: r.size)
        shape.position = CGPoint(x: r.width / 2, y: r.height / 2)
        CATransaction.commit()
    }

    /// Points per second (y up) over the last 80 ms of the carry; a hand that stopped before letting go throws nothing.
    private func releaseVelocity(at t: TimeInterval, _ p: CGPoint) -> CGVector {
        defer { samples = [] }
        guard let last = samples.last, t - last.t < 0.06 else { return .zero }
        let pts = samples.filter { t - $0.t <= 0.08 } + [(t, p)]
        guard let first = pts.first, let end = pts.last, end.t - first.t > 0.008 else { return .zero }
        let dt = end.t - first.t
        return CGVector(dx: (end.p.x - first.p.x) / dt, dy: (end.p.y - first.p.y) / dt)
    }

    private func controlPoint(_ e: NSEvent) -> (Control, CGPoint)? {
        let p = view.convert(e.locationInWindow, from: nil)
        let origin = CGPoint(x: shape.frame.minX + Look.pad, y: shape.frame.minY + Look.bodyTop)
        guard let id = sliderDrag else { return nil }
        return (.slider(id: id, min: 0, max: 1, step: 0), CGPoint(x: p.x - origin.x, y: p.y - origin.y))
    }

    func mouseUp(_ e: NSEvent) {
        defer { downScreen = nil; dragging = false; pressed = nil; sliderDrag = nil; showPress(nil) }
        guard downScreen != nil else { return }
        if let id = sliderDrag, let (_, q) = controlPoint(e) {
            moveSlider(id, x: q.x, commit: true)
            return
        }
        if dragging && isViewer && expanded {
            // An open viewer stays where it was put (its own place; the pill keeps its own).
            dragging = false
            anchor = downPillAnchor
            let scr = NSScreen.screens.first { $0.frame.intersects(shapeRect) }?.frame ?? screenFrame
            let pos: JSONObject = ["x": Double(shapeRect.midX - scr.midX), "y": Double(scr.maxY - shapeRect.maxY)]
            if let o = IslandFile.patch(id, { $0["openPosition"] = pos }) { update(Island(id: id, raw: o)) }
            app?.stateChanged()
            return
        }
        if dragging {
            // Thrown or let go near an edge: it docks to that stack and springs there with the throw's momentum.
            // Otherwise it stays where it was dropped (snapped within 14 pt). Either way the file keeps the place.
            dragging = false
            let v = releaseVelocity(at: e.timestamp, screenPoint(e))
            pendingVelocity = v
            if let o = app?.drop(self, shapeRect, velocity: v) { update(Island(id: id, raw: o)) }
            app?.layoutNow(.snap)   // now, on the release frame: the throw continues without a pause
            CATransaction.flush()
            return
        }
        if let (_, row, _) = confirming, let q = bodyPoint(view.convert(e.locationInWindow, from: nil)) {
            switch Confirm.hit(row, q) {
            case .confirm: finishConfirm(); return
            case .cancel: cancelConfirm(); return
            case .ignored: return
            case .outside: cancelConfirm()
            }
        }
        if let c = pressed, let (now, _) = control(at: view.convert(e.locationInWindow, from: nil)), sameControl(c, now) {
            perform(c)
            return
        }
        // A click on a pill opens it; on an open island, a click on its header row minimises it. The rest of an open
        // island does only what its controls do (the open target is its own button).
        let local = view.convert(e.locationInWindow, from: nil)
        if expanded, local.y - shape.frame.minY > Look.pillH { return }
        let keep = !expanded
        if let o = IslandFile.patch(id, { $0["expanded"] = keep }) { update(Island(id: id, raw: o)) }
    }

    private var pressLayer: CALayer?

    /// A pressed row or button lights up until the pointer is released.
    private func showPress(_ c: Control?) {
        pressLayer?.removeFromSuperlayer()
        pressLayer = nil
        guard let c, let body, let r = body.regions.first(where: { sameControl($0.control, c) })?.rect else { return }
        let l = CALayer()
        let button: Bool
        if case .button = c { button = true } else { button = false }
        l.frame = button ? r : r.insetBy(dx: -6, dy: 0)
        l.cornerRadius = button ? r.height / 2 : 8
        l.cornerCurve = .continuous
        l.backgroundColor = (button ? NSColor.black.withAlphaComponent(0.18) : Look.white(0.1)).cgColor
        l.noImplicitAnimations()
        body.layer.addSublayer(l)
        pressLayer = l
    }

    /// Open a page or file in the default browser or app. Lab runs (FI_OPEN_LOG) write what would open instead.
    func openTarget(_ target: String) {
        guard let url = OpenTarget.url(target) else { return }
        IslandFile.respond(island: id, kind: "open", action: "open", value: target)
        let env = ProcessInfo.processInfo.environment
        if let log = env["FI_OPEN_LOG"], env["FLOATING_ISLANDS_DIR"] != nil {
            let app = NSWorkspace.shared.urlForApplication(toOpen: url)?.lastPathComponent ?? "none"
            let line = "\(url.absoluteString)\t\(app)\n"
            if let h = FileHandle(forWritingAtPath: log) { h.seekToEndOfFile(); h.write(line.data(using: .utf8)!); h.closeFile() }
            else { try? line.write(toFile: log, atomically: true, encoding: .utf8) }
            return
        }
        NSWorkspace.shared.open(url)
    }

    private func sameControl(_ a: Control, _ b: Control) -> Bool {
        switch (a, b) {
        case let (.open(x), .open(y)): return x == y
        case let (.button(x, _), .button(y, _)), let (.listRow(x, _), .listRow(y, _)), let (.toggle(x, _), .toggle(y, _)): return x == y
        case let (.tick(x, _), .tick(y, _)), let (.disclose(x), .disclose(y)): return x == y
        case let (.approve(x, a), .approve(y, b)): return x == y && a == b
        default: return false
        }
    }

    /// A click on a control. An answer to the agent (a button, a proposal's action) asks "Sure?" first.
    private func perform(_ c: Control) {
        if let action = Self.answer(c), Confirm.needed(action) { askConfirm(c) } else { commit(c) }
    }

    private static func answer(_ c: Control) -> String? {
        switch c {
        case let .button(id, _): return id
        case let .approve(_, action): return action
        case let .listRow(id, _): return id
        default: return nil
        }
    }

    // MARK: Confirm (Confirm.swift): the row over the clicked button's row, kept across rebuilds while it is up.

    private func askConfirm(_ c: Control) {
        cancelConfirm()
        guard let body, let row = confirmRow(c, in: body) else { commit(c); return }
        body.layer.addSublayer(row.layer)
        let w = DispatchWorkItem { [weak self] in self?.cancelConfirm() }
        confirming = (c, row, w)
        DispatchQueue.main.asyncAfter(deadline: .now() + Confirm.window, execute: w)
        if let r = body.regions.first(where: { sameControl($0.control, c) })?.rect { scrollToShow(r) }
    }

    /// The clicked button's row: every button beside it (for a proposal, its own actions).
    private func confirmRow(_ c: Control, in body: Body) -> Confirm.Row? {
        guard let clicked = body.regions.first(where: { sameControl($0.control, c) })?.rect else { return nil }
        let row = body.regions.filter { r in
            switch (c, r.control) {
            case (.button, .button): return abs(r.rect.minY - clicked.minY) < 1
            case let (.approve(k, _), .approve(k2, _)): return k == k2
            default: return false
            }
        }.map(\.rect).reduce(clicked) { $0.union($1) }
        // Painted the colour under it so it covers the buttons: black, or the proposal's lifted panel.
        let bg: NSColor
        if case .approve = c { bg = NSColor(white: 0.08, alpha: 1) } else { bg = .black }
        return Confirm.row(over: row, clicked: clicked, background: bg, scale: scale)
    }

    private func finishConfirm() {
        guard let (c, row, w) = confirming else { return }
        w.cancel()
        confirming = nil
        Confirm.dismiss(row)
        commit(c)
    }

    private func cancelConfirm() {
        guard let (_, row, w) = confirming else { return }
        w.cancel()
        confirming = nil
        Confirm.dismiss(row)
    }

    /// After a rebuild (the agent rewrote the island) the row comes back over the same button, or goes with it.
    private func reattachConfirm() {
        guard let (c, old, w) = confirming, let body else { return }
        guard let row = confirmRow(c, in: body) else { w.cancel(); confirming = nil; return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        row.layer.removeAllAnimations()
        CATransaction.commit()
        body.layer.addSublayer(row.layer)
        confirming = (c, Confirm.Row(layer: row.layer, cancel: row.cancel, confirm: row.confirm, clicked: row.clicked, started: old.started), w)
    }

    /// Lab: is a "Sure?" row up, where its Cancel or Confirm is (window coordinates), and answer it.
    var isConfirming: Bool { confirming != nil }
    var labMaxScroll: CGFloat { maxScroll }
    func labConfirmCentre(cancel: Bool) -> CGPoint? {
        guard let (_, row, _) = confirming else { return nil }
        let r = cancel ? row.cancel : row.confirm
        return view.convert(CGPoint(x: shape.frame.minX + Look.pad + r.midX, y: shape.frame.minY + Look.bodyTop - scrollY + r.midY), to: nil)
    }
    func labConfirm(_ yes: Bool) { if yes { finishConfirm() } else { cancelConfirm() } }

    private func commit(_ c: Control) {
        var new: JSONObject?
        switch c {
        case let .button(action, label):
            IslandFile.respond(island: id, kind: "button", action: action, value: label)
            new = IslandFile.patch(id) { $0["chosen"] = action }
            if island.look == "waiting" {
                // Answered: the choice shows for a moment, then the question leaves.
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { [id] in
                    if IslandFile.read(Paths.file(id))?["chosen"] as? String == action { try? FileManager.default.removeItem(at: Paths.file(id)) }
                }
            }
        case let .listRow(item, title):
            IslandFile.respond(island: id, kind: "list", action: item, value: title)
            new = IslandFile.patch(id) { o in o["parts"] = Self.mapParts(o, type: "list", id: nil) { $0["selected"] = item } }
        case let .toggle(tid, on):
            IslandFile.respond(island: id, kind: "toggle", action: tid, value: !on)
            new = IslandFile.patch(id) { o in o["parts"] = Self.mapParts(o, type: "toggle", id: tid) { $0["on"] = !on } }
        case let .open(target):
            openTarget(target)
        case let .tick(key, _):
            // Tick off (or back on) one plan item, found by its id (the title when it has none), wherever the agent
            // has moved it meanwhile. The agent hears `tick` with the item's id.
            var result: (item: String, done: Bool)?
            new = IslandFile.patch(id) { o in
                guard var items = o["items"] as? [Any], let it = Plan.items(o).first(where: { $0.key == key }) else { return }
                var obj: JSONObject = (items[it.index] as? JSONObject) ?? ["title": it.title]
                let d = !it.done
                if d { obj["done"] = true } else { obj.removeValue(forKey: "done") }
                items[it.index] = obj
                o["items"] = items
                result = (it.id ?? it.title, d)
            }
            if let result { IslandFile.respond(Responses.tick(island: id, item: result.item, done: result.done)) }
        case let .disclose(key):
            if openItems.contains(key) { openItems.remove(key) } else { openItems.insert(key) }
            rebuildPlan()
            if openItems.contains(key), let row = body?.rows["row:" + key]?.frame { scrollToShow(body?.rows["prop:" + key].map { row.union($0.frame) } ?? row) }
        case let .approve(key, action):
            // The answer goes to the agent; the row shows the choice for a moment, then closes (Rust moves the item).
            var itemID: String?
            var label: String?
            new = IslandFile.patch(id) { o in
                guard var items = o["items"] as? [Any], let it = Plan.items(o).first(where: { $0.key == key }),
                      var obj = items[it.index] as? JSONObject else { return }
                obj["chosen"] = action
                items[it.index] = obj
                o["items"] = items
                itemID = it.id ?? it.title
                label = it.proposal?.actions.first(where: { $0.action == action })?.label
            }
            // The label he saw goes along: Heimdall hears his words even if the buttons change meanwhile.
            if let itemID {
                var r = Responses.approval(island: id, item: itemID, action: action)
                if let label { r["label"] = label }
                IslandFile.respond(r)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.4) { [weak self] in
                guard let self, self.openItems.remove(key) != nil else { return }
                self.rebuildPlan()
            }
        case .slider: break
        }
        if let new { update(Island(id: id, raw: new)) }
    }

    private static func mapParts(_ o: JSONObject, type: String, id: String?, _ f: (inout JSONObject) -> Void) -> [JSONObject] {
        (o["parts"] as? [JSONObject] ?? []).map { p in
            var p = p
            if p["type"] as? String == type, id == nil || (p["id"] as? String ?? type) == id { f(&p) }
            return p
        }
    }

    private func moveSlider(_ sid: String, x: CGFloat, commit: Bool) {
        guard let s = body?.sliders[sid],
              let part = island.parts.first(where: { $0["type"] as? String == "slider" && ($0["id"] as? String ?? "slider") == sid }) else { return }
        let lo = (part["min"] as? NSNumber)?.doubleValue ?? 0, hi = (part["max"] as? NSNumber)?.doubleValue ?? 1
        let step = (part["step"] as? NSNumber)?.doubleValue ?? 0
        var v = lo + Double(min(1, max(0, x / s.track.width))) * (hi - lo)
        if step > 0 { v = (v / step).rounded() * step }
        let frac = CGFloat((v - lo) / max(hi - lo, 1e-9))
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        s.fill.bounds.size.width = max(4, s.track.width * frac)
        s.knob.position.x = s.track.width * frac - 9
        s.label?.string = NSAttributedString(string: Island.format(v) + (s.unit.isEmpty ? "" : " " + s.unit),
                                             attributes: [.font: Look.smallMono, .foregroundColor: NSColor.white])
        CATransaction.commit()
        guard commit else { return }
        IslandFile.respond(island: id, kind: "slider", action: sid, value: v)
        if let o = IslandFile.patch(id, { o in o["parts"] = Self.mapParts(o, type: "slider", id: sid) { $0["value"] = v } }) {
            island = Island(id: id, raw: o)   // the knob is already there; no rebuild
        }
    }

    // MARK: Lab hooks (self-test drives the real event path through the window)

    /// The close button's centre in window coordinates (nil while it is hidden).
    func closeCentre() -> CGPoint? {
        guard expanded, let c = closeLayer, c.opacity > 0 else { return nil }
        return view.convert(CGPoint(x: shape.frame.minX + c.position.x, y: shape.frame.minY + c.position.y), to: nil)
    }

    /// The strings the open body shows.
    func bodyTexts() -> [String] {
        func walk(_ l: CALayer) -> [String] {
            ((l as? CATextLayer)?.string as? NSAttributedString).map { [$0.string] } ?? [] + (l.sublayers ?? []).flatMap(walk)
        }
        return walk(bodyHost)
    }

    /// Layers with an animation still running or looping (empty at rest; finished ones Core Animation has not pruned
    /// yet do not count, the render server no longer draws them).
    func animatedLayers() -> [String] {
        func walk(_ l: CALayer) -> [String] {
            let now = l.convertTime(CACurrentMediaTime(), from: nil)
            let live = (l.animationKeys() ?? []).filter { k in
                guard let a = l.animation(forKey: k) else { return false }
                if a.repeatCount == .infinity || a.repeatDuration == .infinity { return true }
                let end = a.beginTime + a.duration * Double(max(1, a.repeatCount)) * (a.autoreverses ? 2 : 1)
                return end > now
            }
            return live.map { "\(type(of: l)).\($0)" } + (l.sublayers ?? []).flatMap(walk)
        }
        return walk(root)
    }

    /// Lab renders: windows are built but never ordered on screen.
    static var offscreen = false
    /// Lab: what a click on a control does, without a pointer.
    func labPerform(_ c: Control) { commit(c) }
    /// Lab: a click on a control as the user makes it (answers ask first).
    func labClick(_ c: Control) { perform(c) }

    /// A plan row's place in the body (model value), by key: `row:<key>`, `hdr:<group>`, `prop:<key>`.
    func rowFrame(_ key: String) -> CGRect? { body?.rows[key]?.frame }
    /// Keys of the open body's plan rows, top to bottom.
    func rowOrder() -> [String] {
        (body?.rows ?? [:]).filter { $0.key.hasPrefix("row:") }.sorted { $0.value.frame.minY < $1.value.frame.minY }.map { String($0.key.dropFirst(4)) }
    }

    func controlCentre(_ match: (Control) -> Bool) -> CGPoint? {
        guard let body, let r = body.regions.first(where: { match($0.control) }) else { return nil }
        let origin = CGPoint(x: shape.frame.minX + Look.pad, y: shape.frame.minY + Look.bodyTop - scrollY)
        let v = CGPoint(x: origin.x + r.rect.midX, y: origin.y + r.rect.midY)
        return view.convert(v, to: nil)   // window coordinates
    }
}

final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

// MARK: Viewer: a file shown large in the middle of the screen, closed by Esc, a click outside, its X or the CLI.
// The content (any file type, proposed actions) is a FileViewer (Viewer.swift); this block only hosts it.

extension IslandController {
    fileprivate func loadViewer() -> FileViewer {
        if let v = fileViewer { return v }
        let v = FileViewer(path: island.path ?? "", actions: island.viewerActions)
        let id = self.id
        v.onAction = { a in IslandFile.respond(island: id, kind: "button", action: a.action, value: nil) }
        v.onResize = { [weak self] in
            guard let self, self.isViewer, self.expanded, self.placed else { return }
            self.morph(to: self.rect(for: self.size), .grow)
        }
        fileViewer = v
        return v
    }

    private func viewerBodyRect(_ size: CGSize) -> CGRect { FileViewer.bodyRect(size) }

    func viewerSize() -> CGSize {
        loadViewer().islandSize(screen: screenFrame.isEmpty ? CGSize(width: 1728, height: 1117) : screenFrame.size)
    }

    /// The file changed under an open viewer: rebuild it from the file, keeping the scroll position.
    func reloadViewer() {
        guard isViewer, let v = fileViewer, v.isStale else { return }
        // A reload still waiting to restore its place (an async load) keeps the place it was given.
        let offset = v.restoreOffset ?? v.scrollOffset()
        resetViewer()
        guard expanded, placed else { return }
        loadViewer().restoreOffset = offset
        viewerOpened(true)
        morph(to: rect(for: size), .grow)
        showScroll()
    }

    fileprivate func viewerOpened(_ open: Bool) {
        if open {
            // Opened again after the file changed: what it shows is read now.
            if fileViewer?.isStale == true { resetViewer() }
            if let p = island.path, fileWatch?.url != ViewerKind.url(for: p) {
                fileWatch?.cancel()
                fileWatch = FileWatch(ViewerKind.url(for: p)) { [weak self] in self?.reloadViewer() }
            }
            // While the shape flies open the body is a still of the viewer, so the shape is never empty; the live
            // view takes over once it has landed.
            let sz = viewerSize()
            let body = viewerBodyRect(sz)
            bodyHost.sublayers?.forEach { $0.removeFromSuperlayer() }
            let l = CALayer()
            l.anchorPoint = .zero
            l.frame = CGRect(origin: .zero, size: body.size)
            l.contents = loadViewer().snapshot(size: body.size)?.cgImage
            l.contentsGravity = .topLeft
            l.contentsScale = scale
            l.noImplicitAnimations()
            bodyHost.addSublayer(l)
            bodyHost.bounds = l.bounds
            bodyHost.position = CGPoint(x: Look.pad, y: Look.bodyTop)
            // A click anywhere else closes it (the monitor exists only while a viewer is open).
            if outsideMonitor == nil {
                outsideMonitor = NSEvent.addGlobalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { [weak self] _ in self?.closeViewer() }
            }
            panel.acceptsKey = true
        } else {
            hideScroll()
            fileWatch?.cancel()
            fileWatch = nil
            if let m = outsideMonitor { NSEvent.removeMonitor(m); outsideMonitor = nil }
            panel.acceptsKey = false
            if panel.isKeyWindow { panel.resignKey() }
        }
    }

    /// The live viewer (scrolling, selection, links, players), shown once the shape has opened.
    fileprivate func showScroll() {
        let v = loadViewer()
        let body = viewerBodyRect(shapeRect.size)
        let r = v.makeView(size: body.size)
        if r.superview !== container { container.addSubview(r) }
        r.frame = body
        v.layout(body.size)
        bodyHost.opacity = 0
        guard r.isHidden || r.alphaValue < 1 else { return }
        r.isHidden = false
        r.alphaValue = 0
        NSAnimationContext.runAnimationGroup { ctx in
            ctx.duration = 0.2
            r.animator().alphaValue = 1
        }
    }

    fileprivate func hideScroll() {
        fileViewer?.pause()
        fileViewer?.root?.isHidden = true
        fileViewer?.root?.alphaValue = 0
        bodyHost.opacity = 1
    }

    /// The file or its actions changed, or the island left: drop the viewer (players stop).
    func resetViewer() {
        fileViewer?.teardown()
        fileViewer = nil
        // Open, viewerOpened arms the watch again for the current path; closed or leaving, nothing watches.
        if removing || !expanded || !isViewer { fileWatch?.cancel(); fileWatch = nil }
    }

    func closeViewer() {
        guard isViewer, expanded else { return }
        if let o = IslandFile.patch(id, { $0["expanded"] = false }) { update(Island(id: id, raw: o)) }
    }

    /// Right-click (or control-click): open or minimise, and close.
    func showMenu(_ e: NSEvent, in v: NSView) {
        let m = NSMenu()
        let toggle = NSMenuItem(title: expanded ? "Minimise" : "Open", action: #selector(menuToggle), keyEquivalent: "")
        toggle.target = self
        let close = NSMenuItem(title: "Close", action: #selector(menuClose), keyEquivalent: "")
        close.target = self
        m.addItem(toggle)
        if let t = island.open, !OpenTarget.missing(t) {
            let o = NSMenuItem(title: "Open " + OpenTarget.label(t), action: #selector(menuOpenTarget), keyEquivalent: "")
            o.target = self
            m.addItem(o)
        }
        m.addItem(close)
        NSMenu.popUpContextMenu(m, with: e, for: v)
    }

    @objc func menuToggle() {
        let keep = !expanded
        if let o = IslandFile.patch(id, { $0["expanded"] = keep }) { update(Island(id: id, raw: o)) }
    }

    @objc func menuClose() { closeIsland() }
    @objc func menuOpenTarget() { if let t = island.open { openTarget(t) } }

    /// Close for good: the island goes and the agent hears about it.
    func closeIsland() {
        IslandFile.respond(island: id, kind: "closed", action: "close", value: nil)
        try? FileManager.default.removeItem(at: Paths.file(id))
    }

    fileprivate func layoutClose(animated: Bool) {
        guard expanded || closeLayer != nil else { return }
        if closeLayer == nil {
            let c = symbolLayer("xmark.circle.fill", size: 22, colors: [Look.white(0.9), Look.white(0.18)], scale: scale)
            c.opacity = 0
            shape.addSublayer(c)
            closeLayer = c
        }
        guard let c = closeLayer else { return }
        let target = CGPoint(x: size.width - Look.pad - 4, y: Look.pillH / 2)
        shape.addSublayer(c)   // keep it above the content
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        c.position = target
        CATransaction.commit()
        if animated { c.fade(to: expanded ? 1 : 0, duration: expanded ? 0.2 : 0.08, delay: expanded ? 0.15 : 0) } else { c.opacity = expanded ? 1 : 0 }
    }
}
