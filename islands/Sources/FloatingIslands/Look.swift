// The visual language, taken from Heimdall's notch island (Heimdall lab/island-design/DESIGN.md §1, §4, §5):
// the notch's own black, radius r = clamp(10 + (h − 32) × 0.4, 10, 24) with continuous corners, grow spring
// 0.45 s / 0.82, retract spring 0.34 s / 1.0, SF Pro, white text, colour only in icons.
import AppKit
import CoreText
import QuartzCore

enum Look {
    static let pillH: CGFloat = 36
    static let pillPad: CGFloat = 15
    static let pad: CGFloat = 16          // expanded side padding
    static let headerGap: CGFloat = 8     // icon to title
    static let iconSize: CGFloat = 19
    static let bodyTop: CGFloat = 48
    static let bottomPad: CGFloat = 16
    static let partGap: CGFloat = 12
    static let expandedWidth: CGFloat = 360
    /// A title truncates only when it is truly out of room: the open island's header, beside its icon and close
    /// button ("Mistral vs Heimd…" with room to spare). The pill grows with it.
    static let maxPillTitle: CGFloat = expandedWidth - 2 * pad - 30 - iconSize - headerGap

    static func radius(_ h: CGFloat) -> CGFloat { min(24, max(10, 10 + (h - 32) * 0.4)) }

    static func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> NSColor {
        NSColor(srgbRed: CGFloat((hex >> 16) & 255) / 255, green: CGFloat((hex >> 8) & 255) / 255,
                blue: CGFloat(hex & 255) / 255, alpha: a)
    }
    static let green = rgb(0x30D158), red = rgb(0xFF453A), orange = rgb(0xFF9F0A), blue = rgb(0x0A84FF)
    static func white(_ a: CGFloat) -> NSColor { NSColor(white: 1, alpha: a) }
    /// The hairline that keeps a black island visible on black and dark backgrounds.
    static let hairline = NSColor(white: 1, alpha: 0.34).cgColor
    static let hairlineWidth: CGFloat = 1.25

    // Open islands are read at a glance on any background: body 15 pt near-white, secondary text never below 70 %.
    static let title = NSFont.systemFont(ofSize: 17, weight: .semibold)
    static let trailing = NSFont.monospacedDigitSystemFont(ofSize: 17, weight: .semibold)
    static let body = NSFont.systemFont(ofSize: 15, weight: .regular)
    static let bodyStrong = NSFont.systemFont(ofSize: 15, weight: .semibold)
    static let statusFont = NSFont.systemFont(ofSize: 15, weight: .semibold)
    static let small = NSFont.systemFont(ofSize: 13, weight: .medium)
    /// A plan item's explanation, under its title once the row is open: smaller and quieter than the title.
    static let note = NSFont.systemFont(ofSize: 13, weight: .regular)
    static let groupHeader = NSFont.systemFont(ofSize: 12, weight: .bold)
    static let smallMono = NSFont.monospacedDigitSystemFont(ofSize: 13, weight: .medium)
    static let button = NSFont.systemFont(ofSize: 15, weight: .semibold)
    /// Text colours: primary, secondary (labels, times, values beside a label), done steps.
    static let ink = white(0.96), ink2 = white(0.74), inkDone = white(0.7)
    /// A plan row's small project symbol: present but behind the title.
    static let ink3 = white(0.62)
    static let stat = NSFont.monospacedDigitSystemFont(ofSize: 28, weight: .semibold)
}

struct Spring {
    let response: Double, damping: Double
    // Dynamic Island pace: quick, with one small living overshoot on growth.
    static let grow = Spring(response: 0.3, damping: 0.74)
    static let calm = Spring(response: 0.24, damping: 1.0)
    /// Settling into a snap point: a little livelier than the grow spring, one small overshoot.
    static let snap = Spring(response: 0.3, damping: 0.7)
    /// A thrown island landing in its dock (iPhone picture in picture): carries the throw, settles with one soft overshoot.
    static let toss = Spring(response: 0.4, damping: 0.78)

    /// `velocity`: the starting speed as a fraction of the distance per second (positive toward the target).
    func animation(_ keyPath: String, from: Any?, to: Any?, velocity: Double = 0) -> CASpringAnimation {
        let a = CASpringAnimation(keyPath: keyPath)
        let w0 = 2 * Double.pi / response
        a.mass = 1
        a.stiffness = w0 * w0
        a.damping = 2 * damping * w0
        a.initialVelocity = velocity
        a.fromValue = from
        a.toValue = to
        a.duration = a.settlingDuration
        return a
    }
}

extension CALayer {
    /// Set a property and spring it from where it is on screen now.
    func spring(_ keyPath: String, to target: Any, _ s: Spring) {
        let from = presentation()?.value(forKeyPath: keyPath) ?? self.value(forKeyPath: keyPath)
        setValue(target, forKeyPath: keyPath)
        add(s.animation(keyPath, from: from, to: target), forKey: keyPath)
    }

    /// Opacity fade (content follows the shape, so fades are short and start slightly after the shape moves).
    func fade(to value: Float, duration: Double, delay: Double = 0) {
        let from = presentation()?.opacity ?? opacity
        opacity = value
        let a = CABasicAnimation(keyPath: "opacity")
        a.fromValue = from
        a.toValue = value
        a.duration = duration
        a.beginTime = delay > 0 ? CACurrentMediaTime() + delay : 0
        a.fillMode = .backwards
        a.timingFunction = CAMediaTimingFunction(name: .easeOut)
        add(a, forKey: "opacity")
    }
}

/// Text measured with the same attributes it is drawn with.
func textLayer(_ s: String, _ font: NSFont, _ color: NSColor, maxWidth: CGFloat = 10_000, lines: Int = 1,
               align: NSTextAlignment = .left, strike: Bool = false, scale: CGFloat) -> CATextLayer {
    let para = NSMutableParagraphStyle()
    para.alignment = align
    para.lineBreakMode = lines == 1 ? .byClipping : .byWordWrapping
    var s = s
    if lines == 1, (s as NSString).size(withAttributes: [.font: font]).width > maxWidth {
        // One line that does not fit: cut it at a word or character and end with an ellipsis (CATextLayer draws
        // nothing for an over-long truncating attributed string).
        var lo = 0, hi = s.count
        while lo < hi {
            let mid = (lo + hi + 1) / 2
            let t = String(s.prefix(mid)).trimmingCharacters(in: .whitespaces) + "…"
            if (t as NSString).size(withAttributes: [.font: font]).width <= maxWidth { lo = mid } else { hi = mid - 1 }
        }
        s = String(s.prefix(lo)).trimmingCharacters(in: .whitespaces) + "…"
    }
    var attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: color, .paragraphStyle: para]
    if strike { attrs[.strikethroughStyle] = NSUnderlineStyle.single.rawValue; attrs[.strikethroughColor] = color.withAlphaComponent(0.8) }
    let attr = NSAttributedString(string: s, attributes: attrs)
    let lineH = ceil(font.ascender - font.descender + font.leading)
    let r = attr.boundingRect(with: CGSize(width: lines == 1 ? 10_000 : maxWidth, height: CGFloat(10_000)),
                              options: [.usesLineFragmentOrigin, .usesFontLeading])
    let l = CATextLayer()
    l.string = attr
    l.contentsScale = scale
    l.isWrapped = lines > 1
    l.truncationMode = .end
    l.alignmentMode = align == .right ? .right : (align == .center ? .center : .left)
    l.bounds = CGRect(x: 0, y: 0, width: min(ceil(r.width) + 1, maxWidth), height: min(ceil(r.height), lineH * CGFloat(lines)))
    l.anchorPoint = .zero
    l.actions = ["contents": NSNull(), "bounds": NSNull(), "position": NSNull()]
    return l
}

/// Wrapped text of any length. Its height is the sum of the lines Core Text breaks it into, each at its own height
/// (ascent, descent and leading, so a line with a taller fallback glyph counts as drawn): TextKit's measure runs about
/// a point taller per line at small sizes than CATextLayer draws, which left a gap under a long explanation.
func textBlock(_ s: String, _ font: NSFont, _ color: NSColor, maxWidth: CGFloat, scale: CGFloat) -> CATextLayer {
    let para = NSMutableParagraphStyle()
    para.lineBreakMode = .byWordWrapping
    let attr = NSAttributedString(string: s, attributes: [.font: font, .foregroundColor: color, .paragraphStyle: para])
    let setter = CTFramesetterCreateWithAttributedString(attr)
    let frame = CTFramesetterCreateFrame(setter, CFRange(location: 0, length: 0),
                                         CGPath(rect: CGRect(x: 0, y: 0, width: maxWidth, height: 100_000), transform: nil), nil)
    var height: CGFloat = 0
    for line in (CTFrameGetLines(frame) as? [CTLine]) ?? [] {
        var a: CGFloat = 0, d: CGFloat = 0, l: CGFloat = 0
        CTLineGetTypographicBounds(line, &a, &d, &l)
        height += a + d + l
    }
    height = max(height, font.ascender - font.descender)
    let l = CATextLayer()
    l.string = attr
    l.contentsScale = scale
    l.isWrapped = true
    l.bounds = CGRect(x: 0, y: 0, width: maxWidth, height: ceil(height))
    l.anchorPoint = .zero
    l.actions = ["contents": NSNull(), "bounds": NSNull(), "position": NSNull()]
    return l
}

/// An SF Symbol drawn into a layer at the screen's scale (palette colours: glyph first, then fill).
func symbolLayer(_ name: String, size: CGFloat, colors: [NSColor], weight: NSFont.Weight = .semibold, scale: CGFloat) -> CALayer {
    let l = CALayer()
    l.bounds = CGRect(x: 0, y: 0, width: size, height: size)
    l.anchorPoint = CGPoint(x: 0.5, y: 0.5)
    l.contentsGravity = .resizeAspect
    l.contentsScale = scale
    var cfg = NSImage.SymbolConfiguration(pointSize: size * 0.88, weight: weight)
    cfg = cfg.applying(colors.count > 1 ? .init(paletteColors: colors) : .init(hierarchicalColor: colors.first ?? .white))
    if let img = (NSImage(systemSymbolName: name, accessibilityDescription: nil)
                  ?? NSImage(systemSymbolName: "circle.fill", accessibilityDescription: nil))?.withSymbolConfiguration(cfg) {
        l.contents = img.layerContents(forContentsScale: scale)
    }
    return l
}

/// An island's own icon in `color`: an SF Symbol name, or a tiny inline SVG (`<svg …>`, drawn as a silhouette, any
/// fill or stroke). nil when it cannot be drawn (an unknown symbol, bad SVG): the state's own icon shows instead.
func iconLayer(_ icon: String, size: CGFloat, color: NSColor, scale: CGFloat) -> CALayer? {
    let t = icon.trimmingCharacters(in: .whitespacesAndNewlines)
    guard t.hasPrefix("<svg") else {
        guard NSImage(systemSymbolName: t, accessibilityDescription: nil) != nil else { return nil }
        return symbolLayer(t, size: size, colors: [color], scale: scale)
    }
    // Hand-written files skip Heimdall's check: shapes only here too (nothing fetched, styled or scripted).
    let lower = t.lowercased()
    guard t.utf8.count <= 4096, !["script", "href", "style", "url(", "foreignobject", "<image", "\\", "<!"].contains(where: { lower.contains($0) }),
          let svg = NSImage(data: Data(t.utf8)), svg.size.width > 0, svg.size.height > 0 else { return nil }
    let px = Int(ceil(size * scale))
    guard let ctx = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
    let full = CGRect(x: 0, y: 0, width: px, height: px)
    // Aspect fit, a hair inside the box like the symbols' own margins.
    let box = full.insetBy(dx: CGFloat(px) * 0.05, dy: CGFloat(px) * 0.05)
    let k = min(box.width / svg.size.width, box.height / svg.size.height)
    let fit = CGRect(x: box.midX - svg.size.width * k / 2, y: box.midY - svg.size.height * k / 2, width: svg.size.width * k, height: svg.size.height * k)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(cgContext: ctx, flipped: false)
    svg.draw(in: fit)
    NSGraphicsContext.restoreGraphicsState()
    ctx.setBlendMode(.sourceAtop)
    ctx.setFillColor(color.cgColor)
    ctx.fill(full)
    let l = CALayer()
    l.bounds = CGRect(x: 0, y: 0, width: size, height: size)
    l.anchorPoint = CGPoint(x: 0.5, y: 0.5)
    l.contentsGravity = .resizeAspect
    l.contentsScale = scale
    l.contents = ctx.makeImage()
    return l
}

extension CALayer {
    func noImplicitAnimations() { actions = ["position": NSNull(), "bounds": NSNull(), "opacity": NSNull(), "contents": NSNull(),
                                             "transform": NSNull(), "backgroundColor": NSNull(), "path": NSNull(), "strokeEnd": NSNull(),
                                             "sublayers": NSNull(), "cornerRadius": NSNull(), "frame": NSNull()] }
}
