// The viewer island's content: any file, rendered by type (IslandModel's ViewerKind): markdown rendered, text and code
// monospaced with light colouring, images, PDF (every page, scrolling), audio (a player with a waveform), video
// (AVPlayerView), HTML (WebKit), anything else through QuickLook; proposed-action buttons on top.
// The size is known at once from cheap header reads; heavy loads (big text, PDF pages, waveforms) run off the main thread.
import AppKit
import AVKit
import IslandModel
import PDFKit
import Quartz
import WebKit

final class FileViewer {
    let url: URL
    let kind: ViewerKind
    private(set) var actions: [ViewerAction]
    var onAction: ((ViewerAction) -> Void)?
    /// The content's natural size became known after the first layout (async text, a web page's height).
    var onResize: (() -> Void)?
    private var webHeight: CGFloat?
    private var webWatcher: WebLoadWatcher?

    private(set) var root: ViewerRootView?
    private var content: NSView?
    private var actionRow: ActionRow?
    private var audio: AudioPlayerView?
    private var video: AVPlayerView?
    private var poster: CGImage?
    private var web: WKWebView?
    private var textView: NSTextView?
    private var loadedText: NSAttributedString?
    private var textLoading = false
    private var imageInfo: CGSize?
    private var pdfPageSizes: [CGSize]?
    private var videoAspect: CGFloat?

    static let syncTextLimit = 150_000
    static let pdfInset: CGFloat = 6, pdfGap: CGFloat = 10
    /// Text shown at most (a larger file shows its start and a "file truncated" line): layout stays fast on resize.
    static let textReadLimit = 200_000
    /// The last text height measured, by width: a resize measures each width once.
    private var measured: (text: NSAttributedString, width: CGFloat, height: CGFloat)?

    /// What was loaded: the file's identity, size and modification time (nil: no file). A viewer whose file no longer
    /// matches is stale and is rebuilt (bug 22 Oct 20:45: an edited file kept showing its first version).
    let stamp: FileStamp?
    var isStale: Bool { FileStamp(url) != stamp }
    /// A scroll position to restore once the content is laid out (a reload keeps the user's place).
    var restoreOffset: CGPoint?

    init(path: String, actions: [ViewerAction]) {
        url = ViewerKind.url(for: path)
        stamp = FileStamp(url)
        kind = ViewerKind.detect(url)
        self.actions = actions
        if case .text = kind, fileSize <= Self.syncTextLimit { loadedText = Self.readText(url, kind: kind) }
        if kind == .markdown, fileSize <= Self.syncTextLimit { loadedText = Self.readText(url, kind: kind) }
    }

    private var fileSize: Int { ((try? FileManager.default.attributesOfItem(atPath: url.path))?[.size] as? NSNumber)?.intValue ?? 0 }

    // MARK: Size

    /// The pill's symbol for a kind of file.
    static func symbol(_ kind: ViewerKind) -> String {
        switch kind {
        case .markdown: return "doc.richtext"
        case .text: return "chevron.left.forwardslash.chevron.right"
        case .image: return "photo"
        case .pdf: return "doc.text"
        case .audio: return "waveform"
        case .video: return "play.rectangle"
        case .html: return "globe"
        case .other: return "doc"
        case .missing: return "questionmark.folder"
        }
    }

    /// The widest the viewer gets for its kind on a screen this size.
    static func maxWidth(_ kind: ViewerKind, screen: CGSize) -> CGFloat {
        switch kind {
        case .audio: return 460
        case .missing: return 320
        case .html, .video, .pdf, .other: return min(920, max(480, screen.width * 0.56))
        default: return min(760, max(420, screen.width * 0.5))
        }
    }

    /// Height of the proposed-action rows plus the gap under them (0 without actions).
    var actionsHeight: CGFloat {
        let rows = ViewerAction.rows(actions).count
        return rows == 0 ? 0 : CGFloat(rows) * ActionRow.h + CGFloat(rows - 1) * ActionRow.gap + 14
    }

    /// The open island's size on a screen this size: chrome (title row, padding), the action rows, the content.
    func islandSize(screen: CGSize) -> CGSize {
        let chrome = Look.bodyTop + Look.bottomPad
        let maxW = Self.maxWidth(kind, screen: screen) - 2 * Look.pad
        let maxH = max(240, screen.height * 0.78) - chrome - actionsHeight
        let c = contentSize(maxW: maxW, maxH: maxH)
        let w = max(c.width + 2 * Look.pad, actions.isEmpty ? 280 : 360)
        return CGSize(width: ceil(w), height: ceil(c.height + actionsHeight + chrome))
    }

    /// Where the viewer's body sits in an open island of `size` (shape coordinates, y down).
    static func bodyRect(_ size: CGSize) -> CGRect {
        CGRect(x: Look.pad, y: Look.bodyTop, width: size.width - 2 * Look.pad, height: size.height - Look.bodyTop - Look.bottomPad)
    }

    /// The content area's size (without the action rows), within `maxW` × `maxH`.
    func contentSize(maxW: CGFloat, maxH: CGFloat) -> CGSize {
        let maxH = max(80, maxH)
        switch kind {
        case .missing:
            return CGSize(width: maxW, height: 44)
        case .markdown, .text:
            guard let t = loadedText else { return CGSize(width: maxW, height: maxH) }
            let w = maxW - 8
            let h: CGFloat
            if let m = measured, m.text === t, m.width == w { h = m.height } else { h = Self.textHeight(t, width: w); measured = (t, w, h) }
            return CGSize(width: maxW, height: ceil(min(maxH, max(44, h + 6))))
        case .image:
            if imageInfo == nil { imageInfo = Self.imagePixelSize(url) }
            guard let px = imageInfo, px.width > 0, px.height > 0 else { return CGSize(width: maxW, height: maxH * 0.6) }
            // Never larger than the image's own points (a 2× screen), never cut.
            let natural = CGSize(width: px.width / 2, height: px.height / 2)
            let k = min(1, maxW / natural.width, maxH / natural.height)
            return CGSize(width: max(240, floor(natural.width * k)), height: max(60, floor(natural.height * k)))
        case .pdf:
            if pdfPageSizes == nil { pdfPageSizes = Self.pdfPageSizes(url, limit: 40) }
            let pages = pdfPageSizes ?? []
            guard let p0 = pages.first, p0.width > 0 else { return CGSize(width: maxW, height: maxH) }
            // The first page fits whole (a white page is a lot of light; it never fills a wide screen).
            let pw = min(maxW - 2 * Self.pdfInset, (maxH - 2 * Self.pdfInset) * p0.width / p0.height)
            let total = pages.reduce(CGFloat(0)) { $0 + $1.height * pw / max(1, $1.width) + Self.pdfGap } + 2 * Self.pdfInset - Self.pdfGap
            return CGSize(width: floor(max(380, pw + 2 * Self.pdfInset)), height: ceil(min(maxH, total)))
        case .audio:
            return CGSize(width: maxW, height: AudioPlayerView.height)
        case .video:
            if videoAspect == nil { videoAspect = Self.videoAspect(url) ?? 9.0 / 16.0 }
            let a = videoAspect ?? 9.0 / 16.0
            var w = maxW, h = w * a
            if h > maxH { h = maxH; w = h / a }
            return CGSize(width: floor(max(320, w)), height: floor(h))
        case .html:
            return CGSize(width: maxW, height: min(maxH, max(160, webHeight ?? maxH)))
        case .other:
            return CGSize(width: min(maxW, 720), height: min(maxH, 540))
        }
    }

    // MARK: Views

    /// The live view, built once: the action rows on top, the renderer under them. `size` is the whole body area.
    @discardableResult
    func makeView(size: CGSize) -> ViewerRootView {
        if let root { layout(size); return root }
        let r = ViewerRootView(frame: CGRect(origin: .zero, size: size))
        r.appearance = NSAppearance(named: .darkAqua)
        root = r
        if !actions.isEmpty {
            let row = ActionRow(actions: actions)
            row.onAction = { [weak self] a in self?.onAction?(a) }
            r.addSubview(row)
            actionRow = row
        }
        let c = buildContent()
        r.addSubview(c)
        content = c
        layout(size)
        return r
    }

    func layout(_ size: CGSize) {
        guard let root else { return }
        root.setFrameSize(size)
        let top = actionsHeight
        actionRow?.frame = CGRect(x: 0, y: 0, width: size.width, height: max(0, top - 14))
        actionRow?.layoutButtons()
        let wasEmpty = content?.frame.isEmpty ?? true
        content?.frame = CGRect(x: 0, y: top, width: size.width, height: max(0, size.height - top))
        if wasEmpty, let pdf = content as? PDFView { Self.toTop(pdf) }
        applyRestore()
    }

    /// The content's scroll view (text, markdown, PDF).
    private var scroller: NSScrollView? {
        (content as? NSScrollView) ?? (content as? PDFView)?.documentView?.enclosingScrollView
    }

    /// Where the content is scrolled to now (nil: nothing scrolls).
    func scrollOffset() -> CGPoint? { scroller?.contentView.bounds.origin }

    /// Scroll to `restoreOffset`, clamped to the new length, once the document has a height.
    private func applyRestore() {
        guard let o = restoreOffset, let sv = scroller, let doc = sv.documentView else { return }
        if let tv = textView, tv.textStorage?.length == 0 { return }
        if let pdf = content as? PDFView, pdf.document == nil { return }
        if let tv = textView, let lm = tv.layoutManager, let tc = tv.textContainer {
            // The text's full height first: clamping against a half-laid-out document would lose the place.
            guard sv.contentView.bounds.width > 0 else { return }
            lm.ensureLayout(for: tc)
            tv.sizeToFit()
        }
        doc.layoutSubtreeIfNeeded()
        let maxY = max(0, doc.frame.height - sv.contentView.bounds.height)
        sv.contentView.scroll(to: CGPoint(x: 0, y: min(max(0, o.y), maxY)))
        sv.reflectScrolledClipView(sv.contentView)
        restoreOffset = nil
    }

    /// The first page's top, whatever scale or frame PDFView settled on.
    static func toTop(_ v: PDFView) {
        v.layoutDocumentView()
        guard let page = v.document?.page(at: 0) else { return }
        let b = page.bounds(for: v.displayBox)
        v.go(to: CGRect(x: b.minX, y: b.maxY - 1, width: 1, height: 1), on: page)
    }

    private func buildContent() -> NSView {
        switch kind {
        case .missing:
            return messageView("Not found", detail: url.lastPathComponent)
        case .markdown, .text:
            return buildText()
        case .image:
            let v = NSImageView()
            v.imageScaling = .scaleProportionallyUpOrDown
            v.animates = false
            v.wantsLayer = true
            v.layer?.cornerRadius = 10
            v.layer?.cornerCurve = .continuous
            v.layer?.masksToBounds = true
            if let img = Self.thumbnail(url, maxPixel: 2400) { v.image = NSImage(cgImage: img, size: .zero) }
            return v
        case .pdf:
            let v = PDFView()
            v.autoScales = true
            v.displayMode = .singlePageContinuous
            v.displaysPageBreaks = true
            v.pageShadowsEnabled = false
            v.backgroundColor = .black
            v.pageBreakMargins = NSEdgeInsets(top: Self.pdfGap / 2, left: Self.pdfInset, bottom: Self.pdfGap / 2, right: Self.pdfInset)
            // PDFDocument parses lazily but can still take a while on a big file: open it off the main thread.
            let u = url
            DispatchQueue.global(qos: .userInitiated).async {
                let doc = PDFDocument(url: u)
                DispatchQueue.main.async { [weak v, weak self] in
                    guard let v, let self else { return }
                    v.document = doc
                    v.autoScales = true
                    Self.toTop(v)
                    self.applyRestore()
                }
            }
            return v
        case .audio:
            let a = AudioPlayerView(url: url)
            audio = a
            return a
        case .video:
            let v = AVPlayerView()
            v.controlsStyle = .inline
            v.videoGravity = .resizeAspect
            v.showsFullScreenToggleButton = false
            v.allowsPictureInPicturePlayback = false
            v.player = AVPlayer(url: url)
            v.player?.actionAtItemEnd = .pause
            v.wantsLayer = true
            v.layer?.cornerRadius = 10
            v.layer?.cornerCurve = .continuous
            v.layer?.masksToBounds = true
            video = v
            let u = url
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                let g = AVAssetImageGenerator(asset: AVURLAsset(url: u))
                g.appliesPreferredTrackTransform = true
                g.maximumSize = CGSize(width: 1800, height: 1800)
                let img = try? g.copyCGImage(at: .zero, actualTime: nil)
                DispatchQueue.main.async { self?.poster = img }
            }
            return v
        case .html:
            let cfg = WKWebViewConfiguration()
            cfg.suppressesIncrementalRendering = true
            let w = WKWebView(frame: .zero, configuration: cfg)
            w.setValue(false, forKey: "drawsBackground")
            w.underPageBackgroundColor = .black
            let watcher = WebLoadWatcher { [weak self, weak w] in
                w?.evaluateJavaScript("(()=>{const b=document.body;if(!b)return 0;const c=getComputedStyle(b);return Math.ceil(b.getBoundingClientRect().height+parseFloat(c.marginTop)+parseFloat(c.marginBottom))})()") { v, _ in
                    guard let self, let h = (v as? NSNumber).map({ CGFloat($0.doubleValue) }), h > 0, h != self.webHeight else { return }
                    self.webHeight = h
                    self.onResize?()
                }
            }
            w.navigationDelegate = watcher
            webWatcher = watcher
            w.loadFileURL(url, allowingReadAccessTo: url.deletingLastPathComponent())
            web = w
            return w
        case .other:
            let q = QLPreviewView(frame: CGRect(x: 0, y: 0, width: 400, height: 300), style: .normal) ?? QLPreviewView()
            q.shouldCloseWithWindow = false
            q.autostarts = false
            q.previewItem = url as NSURL
            return q
        }
    }

    private func buildText() -> NSView {
        let sv = NSScrollView()
        sv.drawsBackground = false
        sv.hasVerticalScroller = true
        sv.scrollerStyle = .overlay
        sv.scrollerKnobStyle = .light
        sv.autohidesScrollers = true
        // TextKit 1: code blocks and tables are text blocks (backgrounds, cells).
        let tv = NSTextView(usingTextLayoutManager: false)
        tv.drawsBackground = false
        tv.isEditable = false
        tv.isSelectable = true
        tv.textContainerInset = NSSize(width: 4, height: 2)
        tv.textContainer?.widthTracksTextView = true
        tv.isVerticallyResizable = true
        tv.autoresizingMask = [.width]
        tv.linkTextAttributes = [.foregroundColor: Look.blue, .cursor: NSCursor.pointingHand]
        tv.selectedTextAttributes = [.backgroundColor: Look.white(0.22)]
        sv.documentView = tv
        textView = tv
        if let t = loadedText {
            tv.textStorage?.setAttributedString(t)
        } else if !textLoading {
            textLoading = true
            let u = url, k = kind
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                let t = Self.readText(u, kind: k)
                DispatchQueue.main.async {
                    guard let self else { return }
                    self.loadedText = t
                    self.textView?.textStorage?.setAttributedString(t)
                    self.onResize?()
                    self.applyRestore()
                }
            }
        }
        return sv
    }

    private func messageView(_ title: String, detail: String) -> NSView {
        let v = FlippedView()
        let t = NSTextField(labelWithString: title)
        t.font = Look.bodyStrong
        t.textColor = Look.ink
        let d = NSTextField(labelWithString: detail)
        d.font = Look.small
        d.textColor = Look.ink2
        d.lineBreakMode = .byTruncatingMiddle
        t.frame = CGRect(x: 0, y: 0, width: 280, height: 20)
        d.frame = CGRect(x: 0, y: 22, width: 280, height: 18)
        d.autoresizingMask = [.width]
        t.autoresizingMask = [.width]
        v.addSubview(t)
        v.addSubview(d)
        return v
    }

    // MARK: Lifecycle

    /// A still of the viewer at `size` (shown while the shape flies open; also the offscreen renders).
    func snapshot(size: CGSize) -> NSBitmapImageRep? {
        let r = makeView(size: size)
        r.layoutSubtreeIfNeeded()
        guard let rep = r.bitmapImageRepForCachingDisplay(in: r.bounds) else { return nil }
        r.cacheDisplay(in: r.bounds, to: rep)
        if kind == .pdf, let v = content, let doc = CGPDFDocument(url as CFURL), let ctx = NSGraphicsContext(bitmapImageRep: rep) {
            NSGraphicsContext.saveGraphicsState()
            NSGraphicsContext.current = ctx
            let f = v.convert(v.bounds, to: r)
            let cg = ctx.cgContext
            cg.clip(to: CGRect(x: f.minX, y: r.bounds.height - f.maxY, width: f.width, height: f.height))
            cg.setFillColor(NSColor.black.cgColor)
            cg.fill(CGRect(x: f.minX, y: r.bounds.height - f.maxY, width: f.width, height: f.height))
            var y = f.minY + Self.pdfInset   // y down, in the root's coordinates
            for i in 1...max(1, doc.numberOfPages) {
                guard y < f.maxY, let page = doc.page(at: i) else { break }
                let box = page.getBoxRect(.cropBox)
                let w = f.width - 2 * Self.pdfInset, h = box.height * w / max(1, box.width)
                let pr = CGRect(x: f.minX + Self.pdfInset, y: r.bounds.height - y - h, width: w, height: h)
                cg.setFillColor(NSColor.white.cgColor)
                cg.fill(pr)
                cg.saveGState()
                cg.translateBy(x: pr.minX, y: pr.minY)
                cg.scaleBy(x: w / box.width, y: h / box.height)
                cg.translateBy(x: -box.minX, y: -box.minY)
                cg.drawPDFPage(page)
                cg.restoreGState()
                y += h + Self.pdfGap
            }
            NSGraphicsContext.restoreGraphicsState()
        }
        // Out-of-process or layer-only content does not draw through cacheDisplay: paint the first video frame.
        if let poster, let v = video, let ctx = NSGraphicsContext(bitmapImageRep: rep) {
            NSGraphicsContext.saveGraphicsState()
            NSGraphicsContext.current = ctx
            let f = v.convert(v.bounds, to: r)
            let flipped = CGRect(x: f.minX, y: r.bounds.height - f.maxY, width: f.width, height: f.height)
            let a = CGFloat(poster.height) / CGFloat(max(1, poster.width))
            var pr = flipped
            if flipped.width * a > flipped.height { pr.size.width = flipped.height / a } else { pr.size.height = flipped.width * a }
            pr.origin = CGPoint(x: flipped.midX - pr.width / 2, y: flipped.midY - pr.height / 2)
            NSBezierPath(roundedRect: flipped, xRadius: 10, yRadius: 10).addClip()
            ctx.cgContext.draw(poster, in: pr)
            NSGraphicsContext.restoreGraphicsState()
        }
        return rep
    }

    /// The viewer closed to its pill: media stop (nothing plays or animates at rest).
    func pause() {
        audio?.pause()
        video?.player?.pause()
        web?.evaluateJavaScript("document.querySelectorAll('video,audio').forEach(m=>m.pause())", completionHandler: nil)
    }

    /// The island left or changed file: stop everything and drop the views.
    func teardown() {
        pause()
        audio?.teardown()
        video?.player?.replaceCurrentItem(with: nil)
        web?.stopLoading()
        (content as? QLPreviewView)?.close()
        root?.removeFromSuperview()
        root = nil
    }

    func showChoice(_ action: String) { actionRow?.showChoice(action) }

    var isWebLoaded: Bool { web.map { !$0.isLoading } ?? true }
    var webView: WKWebView? { web }
    var videoView: AVPlayerView? { video }
    var posterImage: CGImage? { poster }
    var hasText: Bool { loadedText != nil }

    // MARK: Cheap reads

    /// The laid-out height of text in the viewer's text view (TextKit 1, as drawn: text blocks, tables).
    static func textHeight(_ t: NSAttributedString, width: CGFloat) -> CGFloat {
        let storage = NSTextStorage(attributedString: t)
        let lm = NSLayoutManager()
        let tc = NSTextContainer(size: CGSize(width: width, height: .greatestFiniteMagnitude))
        lm.addTextContainer(tc)
        storage.addLayoutManager(lm)
        lm.ensureLayout(for: tc)
        return lm.usedRect(for: tc).height + 4
    }

    static func imagePixelSize(_ url: URL) -> CGSize? {
        guard let src = CGImageSourceCreateWithURL(url as CFURL, nil),
              let p = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any],
              let w = (p[kCGImagePropertyPixelWidth] as? NSNumber)?.doubleValue,
              let h = (p[kCGImagePropertyPixelHeight] as? NSNumber)?.doubleValue else { return nil }
        let o = (p[kCGImagePropertyOrientation] as? NSNumber)?.intValue ?? 1
        return o >= 5 ? CGSize(width: h, height: w) : CGSize(width: w, height: h)
    }

    static func thumbnail(_ url: URL, maxPixel: Int) -> CGImage? {
        let opts: [CFString: Any] = [kCGImageSourceCreateThumbnailFromImageAlways: true, kCGImageSourceThumbnailMaxPixelSize: maxPixel,
                                     kCGImageSourceCreateThumbnailWithTransform: true]
        guard let src = CGImageSourceCreateWithURL(url as CFURL, nil) else { return nil }
        return CGImageSourceCreateThumbnailAtIndex(src, 0, opts as CFDictionary)
    }

    /// Page sizes from the PDF's page dictionaries (no rendering).
    static func pdfPageSizes(_ url: URL, limit: Int) -> [CGSize] {
        guard let doc = CGPDFDocument(url as CFURL) else { return [] }
        return (0..<min(limit, doc.numberOfPages)).compactMap { i in
            guard let p = doc.page(at: i + 1) else { return nil }
            let b = p.getBoxRect(.cropBox)
            return p.rotationAngle % 180 == 0 ? b.size : CGSize(width: b.height, height: b.width)
        }
    }

    /// Height over width of the first video track, as shown (its transform applied).
    static func videoAspect(_ url: URL) -> CGFloat? {
        guard let t = AVURLAsset(url: url).tracks(withMediaType: .video).first else { return nil }
        let s = t.naturalSize.applying(t.preferredTransform)
        guard abs(s.width) > 0 else { return nil }
        return abs(s.height) / abs(s.width)
    }

    /// Text read with a bound: the first 200 KB, rendered as markdown or (coloured) code.
    static func readText(_ url: URL, kind: ViewerKind) -> NSAttributedString {
        guard let h = try? FileHandle(forReadingFrom: url) else { return plain("Cannot read \(url.lastPathComponent)") }
        defer { try? h.close() }
        let data = (try? h.read(upToCount: textReadLimit + 1)) ?? Data()
        var s = String(decoding: data.prefix(textReadLimit), as: UTF8.self)
        if data.count > textReadLimit { s += "\n\n… file truncated: showing the first 200 KB" }
        switch kind {
        case .markdown: return renderMarkdown(s)
        case let .text(lang): return renderCode(s, language: lang)
        default: return plain(s)
        }
    }
}

/// Tells the viewer when a page has loaded (to measure its height).
/// A file's identity and version as `stat` sees it: a rename-replace changes the inode, an in-place write the size or
/// modification time.
struct FileStamp: Equatable {
    let ino: UInt64, size: Int64, mtimeNs: Int64
    init?(_ url: URL) {
        var st = stat()
        guard stat(url.path, &st) == 0 else { return nil }
        ino = UInt64(st.st_ino)
        size = Int64(st.st_size)
        mtimeNs = Int64(st.st_mtimespec.tv_sec) * 1_000_000_000 + Int64(st.st_mtimespec.tv_nsec)
    }
}

/// Watches one file while a viewer shows it: the file itself (written in place) and its folder (replaced by rename,
/// deleted, created again). Events are coalesced over 150 ms into one `onChange`. No polling.
final class FileWatch {
    let url: URL
    private var fileSource: DispatchSourceFileSystemObject?
    private var dirSource: DispatchSourceFileSystemObject?
    private var queued = false
    private let onChange: () -> Void

    init(_ url: URL, onChange: @escaping () -> Void) {
        self.url = url
        self.onChange = onChange
        dirSource = Self.source(url.deletingLastPathComponent().path, [.write, .rename, .delete]) { [weak self] in self?.fired() }
        armFile()
    }

    private static func source(_ path: String, _ mask: DispatchSource.FileSystemEvent, _ handler: @escaping () -> Void) -> DispatchSourceFileSystemObject? {
        let fd = open(path, O_EVTONLY)
        guard fd >= 0 else { return nil }
        let s = DispatchSource.makeFileSystemObjectSource(fileDescriptor: fd, eventMask: mask, queue: .main)
        s.setEventHandler(handler: handler)
        s.setCancelHandler { close(fd) }
        s.resume()
        return s
    }

    /// The file's own watch follows its current inode (an editor's save replaces it).
    private func armFile() {
        fileSource?.cancel()
        fileSource = Self.source(url.path, [.write, .extend, .delete, .rename, .attrib]) { [weak self] in self?.fired() }
    }

    private func fired() {
        guard !queued else { return }
        queued = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) { [weak self] in
            guard let self else { return }
            self.queued = false
            self.armFile()
            self.onChange()
        }
    }

    func cancel() {
        fileSource?.cancel(); fileSource = nil
        dirSource?.cancel(); dirSource = nil
    }
    deinit { cancel() }
}

final class WebLoadWatcher: NSObject, WKNavigationDelegate {
    let loaded: () -> Void
    init(_ loaded: @escaping () -> Void) { self.loaded = loaded }
    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) { loaded() }
}

final class ViewerRootView: NSView {
    override var isFlipped: Bool { true }
}

// MARK: Proposed actions: island buttons (34 pt capsules, at most three a row, the first one primary).

final class ActionRow: NSView {
    static let h: CGFloat = 34, gap: CGFloat = 8
    let actions: [ViewerAction]
    var onAction: ((ViewerAction) -> Void)?
    private var caps: [(ViewerAction, CALayer, CATextLayer)] = []
    private var chosen: String?
    private var pressed: Int?
    private var resetWork: DispatchWorkItem?
    /// "Sure?" over the clicked button's row (Confirm.swift) and the timer that puts the row back.
    private var confirming: (index: Int, row: Confirm.Row, expiry: DispatchWorkItem)?
    var isConfirming: Bool { confirming != nil }

    init(actions: [ViewerAction]) {
        self.actions = actions
        super.init(frame: .zero)
        wantsLayer = true
        layer?.noImplicitAnimations()
    }
    required init?(coder: NSCoder) { fatalError() }
    override var isFlipped: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    private var scale: CGFloat { window?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 2 }

    func layoutButtons() {
        cancelConfirm()
        caps.forEach { $0.1.removeFromSuperlayer(); $0.2.removeFromSuperlayer() }
        caps = []
        var y: CGFloat = 0, index = 0
        for row in ViewerAction.rows(actions) {
            let w = (bounds.width - Self.gap * CGFloat(row.count - 1)) / CGFloat(row.count)
            for (k, a) in row.enumerated() {
                let cap = CALayer()
                cap.frame = CGRect(x: CGFloat(k) * (w + Self.gap), y: y, width: w, height: Self.h)
                cap.cornerRadius = Self.h / 2
                cap.cornerCurve = .continuous
                cap.noImplicitAnimations()
                let t = CATextLayer()
                t.contentsScale = scale
                t.alignmentMode = .center
                t.truncationMode = .end
                t.noImplicitAnimations()
                layer?.addSublayer(cap)
                layer?.addSublayer(t)
                caps.append((a, cap, t))
                style(index, cap, t, a)
                index += 1
            }
            y += Self.h + Self.gap
        }
    }

    private func style(_ i: Int, _ cap: CALayer, _ t: CATextLayer, _ a: ViewerAction) {
        let primary = i == 0
        let isChosen = chosen == a.action
        let dim = chosen != nil && !isChosen
        let fill: NSColor = dim ? Look.white(0.08) : (isChosen || primary ? .white : Look.white(0.16))
        let ink: NSColor = dim ? Look.white(0.35) : (isChosen || primary ? .black : Look.ink)
        cap.backgroundColor = fill.cgColor
        cap.opacity = pressed == i ? 0.7 : 1
        let label = isChosen ? "✓  " + a.label : a.label
        t.string = NSAttributedString(string: label, attributes: [.font: Look.button, .foregroundColor: ink])
        let lh = ceil(Look.button.ascender - Look.button.descender + Look.button.leading) + 1
        t.frame = CGRect(x: cap.frame.minX + 8, y: cap.frame.midY - lh / 2, width: cap.frame.width - 16, height: lh)
    }

    private func restyle() { for (i, c) in caps.enumerated() { style(i, c.1, c.2, c.0) } }

    private func index(at e: NSEvent) -> Int? {
        let p = convert(e.locationInWindow, from: nil)
        return caps.firstIndex { $0.1.frame.contains(p) }
    }

    override func mouseDown(with e: NSEvent) {
        guard chosen == nil, confirming == nil else { return }
        pressed = index(at: e)
        restyle()
    }

    override func mouseUp(with e: NSEvent) {
        defer { pressed = nil; restyle() }
        if let (i, row, _) = confirming {
            switch Confirm.hit(row, convert(e.locationInWindow, from: nil)) {
            case .confirm: cancelConfirm(); send(i)
            case .cancel, .outside: cancelConfirm()
            case .ignored: break
            }
            return
        }
        guard chosen == nil, let p = pressed, index(at: e) == p else { return }
        click(p)
    }

    /// A click (also the self-test's and the renders' path): an answer asks "Sure?" first; "Later" goes at once.
    func click(_ i: Int) {
        guard caps.indices.contains(i) else { return }
        if Confirm.needed(caps[i].0.action) { ask(i) } else { send(i) }
    }

    /// Lab: answer the "Sure?" row.
    func confirm(_ yes: Bool) {
        guard let (i, _, _) = confirming else { return }
        cancelConfirm()
        if yes { send(i) }
    }

    private func ask(_ i: Int) {
        cancelConfirm()
        let clicked = caps[i].1.frame
        let row = caps.map(\.1.frame).filter { abs($0.minY - clicked.minY) < 1 }.reduce(clicked) { $0.union($1) }
        let r = Confirm.row(over: row, clicked: clicked, background: .black, scale: scale)
        layer?.addSublayer(r.layer)
        let w = DispatchWorkItem { [weak self] in self?.cancelConfirm() }
        confirming = (i, r, w)
        DispatchQueue.main.asyncAfter(deadline: .now() + Confirm.window, execute: w)
    }

    private func cancelConfirm() {
        guard let (_, row, w) = confirming else { return }
        w.cancel()
        confirming = nil
        Confirm.dismiss(row)
    }

    /// Report it to the agent and show the choice for a moment.
    private func send(_ i: Int) {
        guard caps.indices.contains(i) else { return }
        let a = caps[i].0
        onAction?(a)
        showChoice(a.action)
    }

    func showChoice(_ action: String) {
        chosen = action
        restyle()
        resetWork?.cancel()
        let w = DispatchWorkItem { [weak self] in self?.chosen = nil; self?.restyle() }
        resetWork = w
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.6, execute: w)
    }
}

// MARK: Text

private let bodyFont = NSFont.systemFont(ofSize: 14, weight: .regular)
private let monoFont = NSFont.monospacedSystemFont(ofSize: 12.5, weight: .regular)

private func para(_ spacingBefore: CGFloat, _ spacingAfter: CGFloat = 0, indent: CGFloat = 0, lineHeight: CGFloat = 1.25) -> NSMutableParagraphStyle {
    let p = NSMutableParagraphStyle()
    p.paragraphSpacingBefore = spacingBefore
    p.paragraphSpacing = spacingAfter
    p.lineHeightMultiple = lineHeight
    p.headIndent = indent
    p.firstLineHeadIndent = indent > 0 ? indent - 14 : 0
    return p
}

func plain(_ s: String) -> NSAttributedString {
    NSAttributedString(string: s, attributes: [.font: monoFont, .foregroundColor: NSColor(white: 1, alpha: 0.88), .paragraphStyle: para(0, lineHeight: 1.15)])
}

/// Source code: monospaced with light colouring (comments, strings, numbers, keywords), Xcode's dark palette.
/// Colouring stops at 200 KB; longer files stay plain.
func renderCode(_ s: String, language: String?) -> NSAttributedString {
    let out = NSMutableAttributedString(attributedString: plain(s))
    guard let lang = language, s.utf16.count <= 200_000 else { return out }
    let ns = s as NSString
    let full = NSRange(location: 0, length: ns.length)
    let kw = Look.rgb(0xFF7AB2), str = Look.rgb(0xFC6A5D), num = Look.rgb(0xD0BF69), com = Look.rgb(0x7F8C98), key = Look.rgb(0x67B7A4)
    func paint(_ pattern: String, _ color: NSColor, _ opts: NSRegularExpression.Options = [], group: Int = 0) {
        guard let re = try? NSRegularExpression(pattern: pattern, options: opts) else { return }
        re.enumerateMatches(in: s, range: full) { m, _, _ in
            if let r = m?.range(at: group), r.location != NSNotFound { out.addAttribute(.foregroundColor, value: color, range: r) }
        }
    }
    let keywords: [String: String] = [
        "swift": "func|let|var|if|else|guard|return|for|in|while|switch|case|default|struct|class|enum|extension|protocol|import|private|public|fileprivate|static|self|init|try|await|async|throws|nil|true|false|final|override|some|any|where|defer|break|continue|lazy|weak",
        "rs": "fn|let|mut|if|else|match|return|for|in|while|loop|struct|enum|impl|trait|use|mod|pub|crate|self|Self|async|await|move|ref|const|static|true|false|where|type|dyn|unsafe|as|break|continue|Some|None|Ok|Err",
        "py": "def|class|return|if|elif|else|for|in|while|import|from|as|with|try|except|finally|raise|lambda|yield|None|True|False|and|or|not|is|pass|break|continue|async|await|self",
        "js": "function|const|let|var|if|else|return|for|of|in|while|class|extends|import|export|from|default|new|this|async|await|try|catch|finally|throw|null|undefined|true|false|typeof|switch|case|break",
        "go": "func|package|import|var|const|type|struct|interface|if|else|for|range|return|go|defer|chan|map|switch|case|default|nil|true|false|break|continue",
        "sh": "if|then|else|elif|fi|for|in|do|done|while|case|esac|function|return|export|local|echo|set",
        "c": "int|char|void|float|double|long|short|unsigned|signed|struct|union|enum|typedef|static|const|return|if|else|for|while|do|switch|case|default|break|continue|sizeof|NULL|include|define",
        "sql": "SELECT|FROM|WHERE|INSERT|INTO|VALUES|UPDATE|SET|DELETE|CREATE|TABLE|INDEX|JOIN|LEFT|RIGHT|INNER|ON|AND|OR|NOT|NULL|GROUP|BY|ORDER|LIMIT|AS|DISTINCT|COUNT|HAVING|PRIMARY|KEY",
    ]
    let family: String = {
        switch lang {
        case "ts", "tsx", "jsx", "mjs", "cjs", "java", "kt", "cs", "php", "css", "scss": return "js"
        case "zsh", "bash", "fish": return "sh"
        case "h", "m", "mm", "cpp", "cc", "hpp", "metal", "glsl", "zig": return "c"
        case "rb", "lua", "nix", "toml", "yaml", "yml", "ini", "conf", "cfg", "env": return lang == "rb" || lang == "lua" ? "py" : "conf"
        default: return lang
        }
    }()
    paint(#"\b(0x[0-9a-fA-F]+|\d+(\.\d+)?)\b"#, num)
    if let words = keywords[family] { paint("\\b(\(words))\\b", kw, family == "sql" ? [.caseInsensitive] : []) }
    if ["json", "jsonl", "conf", "toml", "yaml", "yml"].contains(lang) || family == "conf" {
        paint(#"^\s*([A-Za-z0-9_.\-"]+)\s*[:=]"#, key, [.anchorsMatchLines], group: 1)
        paint(#""([^"\\\n]|\\.)*"\s*:"#, key)
    }
    paint(#""([^"\\\n]|\\.)*"|'([^'\\\n]|\\.)*'"#, str)
    if ["json", "jsonl"].contains(lang) { paint(#""([^"\\\n]|\\.)*"(?=\s*:)"#, key) }
    let hashComments: Set<String> = ["py", "sh", "conf", "rb"]
    if hashComments.contains(family) || ["toml", "yaml", "yml", "nix"].contains(lang) { paint(#"(^|\s)#.*$"#, com, [.anchorsMatchLines]) }
    if family == "sql" || lang == "lua" { paint(#"--.*$"#, com, [.anchorsMatchLines]) }
    if ["swift", "rs", "js", "go", "c"].contains(family) {
        paint(#"(^|[^:"])//.*$"#, com, [.anchorsMatchLines])
        paint(#"/\*[\s\S]*?\*/"#, com)
    }
    return out
}

/// Inline markdown (bold, italic, code, links) through Foundation; blocks (headings, lists, quotes, code fences,
/// tables, rules) line by line.
private func inline(_ s: String, font: NSFont, color: NSColor) -> NSMutableAttributedString {
    let opts = AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
    let out = NSMutableAttributedString()
    guard let a = try? AttributedString(markdown: s, options: opts) else {
        return NSMutableAttributedString(string: s, attributes: [.font: font, .foregroundColor: color])
    }
    for run in a.runs {
        let text = String(a[run.range].characters)
        var f = font
        var c = color
        var attrs: [NSAttributedString.Key: Any] = [:]
        if let intent = run.inlinePresentationIntent {
            if intent.contains(.code) {
                f = NSFont.monospacedSystemFont(ofSize: font.pointSize - 1.5, weight: .regular)
                c = NSColor(white: 1, alpha: 0.95)
                attrs[.backgroundColor] = NSColor(white: 1, alpha: 0.1)
            }
            if intent.contains(.stronglyEmphasized) { f = NSFontManager.shared.convert(f, toHaveTrait: .boldFontMask) }
            if intent.contains(.emphasized) { f = NSFontManager.shared.convert(f, toHaveTrait: .italicFontMask) }
            if intent.contains(.strikethrough) { attrs[.strikethroughStyle] = NSUnderlineStyle.single.rawValue }
        }
        if let link = run.link { c = Look.blue; attrs[.link] = link }
        attrs[.font] = f
        attrs[.foregroundColor] = c
        out.append(NSAttributedString(string: text, attributes: attrs))
    }
    return out
}

private func tableCells(_ t: String) -> [String] {
    var s = t
    if s.hasPrefix("|") { s.removeFirst() }
    if s.hasSuffix("|") { s.removeLast() }
    return s.components(separatedBy: "|").map { $0.trimmingCharacters(in: .whitespaces) }
}

func renderMarkdown(_ src: String) -> NSAttributedString {
    let out = NSMutableAttributedString()
    let body = NSColor(white: 1, alpha: 0.86)
    var codeBlock: NSTextBlock?
    var codeLang: String?
    var codeLines: [String] = []
    var table: [[String]] = []
    var first = true
    func add(_ a: NSAttributedString) {
        if !first { out.append(NSAttributedString(string: "\n")) }
        out.append(a)
        first = false
    }
    func flushCode() {
        guard let block = codeBlock else { return }
        let p = para(0, 0, lineHeight: 1.12)
        p.textBlocks = [block]
        let code = NSMutableAttributedString(attributedString: renderCode(codeLines.joined(separator: "\n"), language: codeLang ?? "swift"))
        code.addAttribute(.paragraphStyle, value: p, range: NSRange(location: 0, length: code.length))
        add(code)
        codeBlock = nil
        codeLines = []
    }
    func flushTable() {
        guard !table.isEmpty else { return }
        let cols = table.map(\.count).max() ?? 1
        let tt = NSTextTable()
        tt.numberOfColumns = cols
        tt.collapsesBorders = true
        tt.hidesEmptyCells = false
        tt.layoutAlgorithm = .automaticLayoutAlgorithm
        tt.setContentWidth(100, type: .percentageValueType)
        tt.setWidth(first ? 0 : 10, type: .absoluteValueType, for: .margin, edge: .minY)
        tt.setWidth(6, type: .absoluteValueType, for: .margin, edge: .maxY)
        for (r, row) in table.enumerated() {
            for c in 0..<cols {
                let cell = NSTextTableBlock(table: tt, startingRow: r, rowSpan: 1, startingColumn: c, columnSpan: 1)
                cell.setWidth(0.75, type: .absoluteValueType, for: .border)
                cell.setBorderColor(NSColor(white: 1, alpha: 0.16))
                cell.setWidth(7, type: .absoluteValueType, for: .padding, edge: .minX)
                cell.setWidth(7, type: .absoluteValueType, for: .padding, edge: .maxX)
                cell.setWidth(4, type: .absoluteValueType, for: .padding, edge: .minY)
                cell.setWidth(4, type: .absoluteValueType, for: .padding, edge: .maxY)
                if r == 0 { cell.backgroundColor = NSColor(white: 1, alpha: 0.07) }
                let text = c < row.count ? row[c] : ""
                let a = inline(text.isEmpty ? " " : text, font: r == 0 ? .systemFont(ofSize: 13, weight: .semibold) : .systemFont(ofSize: 13), color: r == 0 ? .white : body)
                let p = para(0, lineHeight: 1.15)
                p.textBlocks = [cell]
                a.addAttribute(.paragraphStyle, value: p, range: NSRange(location: 0, length: a.length))
                add(a)
            }
        }
        table = []
    }
    for raw in src.components(separatedBy: "\n") {
        let line = raw.replacingOccurrences(of: "\t", with: "    ")
        let t = line.trimmingCharacters(in: .whitespaces)
        if t.hasPrefix("```") || t.hasPrefix("~~~") {
            if codeBlock != nil { flushCode(); continue }
            flushTable()
            let b = NSTextBlock()
            b.backgroundColor = NSColor(white: 1, alpha: 0.07)
            b.setWidth(10, type: .absoluteValueType, for: .padding)
            b.setWidth(first ? 0 : 8, type: .absoluteValueType, for: .margin, edge: .minY)
            b.setWidth(4, type: .absoluteValueType, for: .margin, edge: .maxY)
            b.setContentWidth(100, type: .percentageValueType)
            codeBlock = b
            let lang = t.dropFirst(3).trimmingCharacters(in: .whitespaces).lowercased()
            codeLang = lang.isEmpty ? nil : lang
            continue
        }
        if codeBlock != nil { codeLines.append(line); continue }
        if t.hasPrefix("|") {
            // The separator row (|---|:--:|) is dropped.
            if t.replacingOccurrences(of: "|", with: "").allSatisfy({ "-: ".contains($0) }) { continue }
            table.append(tableCells(t))
            continue
        }
        flushTable()
        if t.isEmpty { continue }
        if t == "---" || t == "***" || t == "___" {
            add(NSAttributedString(string: " ", attributes: [.font: NSFont.systemFont(ofSize: 4), .paragraphStyle: {
                let p = para(0, 0); p.textBlocks = [{ let b = NSTextBlock(); b.setWidth(0.75, type: .absoluteValueType, for: .border, edge: .minY)
                    b.setWidth(12, type: .absoluteValueType, for: .margin, edge: .minY); b.setWidth(4, type: .absoluteValueType, for: .margin, edge: .maxY)
                    b.setBorderColor(NSColor(white: 1, alpha: 0.2)); b.setContentWidth(100, type: .percentageValueType); return b }()]; return p }()]))
            continue
        }
        if t.hasPrefix("#") {
            let level = t.prefix(while: { $0 == "#" }).count
            let text = t.dropFirst(level).trimmingCharacters(in: .whitespaces)
            let size: CGFloat = level == 1 ? 22 : (level == 2 ? 17 : 15)
            let a = inline(text, font: .systemFont(ofSize: size, weight: level == 1 ? .bold : .semibold), color: .white)
            a.addAttribute(.paragraphStyle, value: para(first ? 0 : (level == 1 ? 16 : 13), 4), range: NSRange(location: 0, length: a.length))
            add(a)
            continue
        }
        if t.hasPrefix(">") {
            let a = inline(String(t.dropFirst()).trimmingCharacters(in: .whitespaces), font: bodyFont, color: NSColor(white: 1, alpha: 0.6))
            let p = para(0)
            let b = NSTextBlock()
            b.setWidth(8, type: .absoluteValueType, for: .margin, edge: .minY)
            b.setWidth(2, type: .absoluteValueType, for: .margin, edge: .maxY)
            b.setWidth(2.5, type: .absoluteValueType, for: .border, edge: .minX)
            b.setBorderColor(NSColor(white: 1, alpha: 0.25))
            b.setWidth(12, type: .absoluteValueType, for: .padding, edge: .minX)
            b.setContentWidth(100, type: .percentageValueType)
            p.textBlocks = [b]
            a.addAttribute(.paragraphStyle, value: p, range: NSRange(location: 0, length: a.length))
            add(a)
            continue
        }
        var bullet: String?
        var rest = t
        if let m = t.range(of: #"^[-*+] \[( |x|X)\] "#, options: .regularExpression) {
            bullet = t[m].contains(" ] ") || t[m].contains("[ ]") ? "○" : "✓"
            rest = String(t[m.upperBound...])
        } else if t.hasPrefix("- ") || t.hasPrefix("* ") || t.hasPrefix("+ ") { bullet = "•"; rest = String(t.dropFirst(2)) }
        else if let m = t.range(of: #"^\d+[.)] "#, options: .regularExpression) { bullet = String(t[m]).trimmingCharacters(in: .whitespaces); rest = String(t[m.upperBound...]) }
        if let bullet {
            let depth = CGFloat((line.prefix(while: { $0 == " " }).count) / 2)
            let a = NSMutableAttributedString(string: bullet + "\t", attributes: [.font: bodyFont, .foregroundColor: NSColor(white: 1, alpha: 0.5)])
            a.append(inline(rest, font: bodyFont, color: body))
            let p = NSMutableParagraphStyle()
            p.paragraphSpacingBefore = 3
            p.lineHeightMultiple = 1.25
            p.firstLineHeadIndent = depth * 18
            p.headIndent = depth * 18 + 20
            p.tabStops = [NSTextTab(textAlignment: .left, location: depth * 18 + 20)]
            a.addAttribute(.paragraphStyle, value: p, range: NSRange(location: 0, length: a.length))
            add(a)
            continue
        }
        let a = inline(t, font: bodyFont, color: body)
        a.addAttribute(.paragraphStyle, value: para(first ? 0 : 8), range: NSRange(location: 0, length: a.length))
        add(a)
    }
    flushCode()
    flushTable()
    return out
}
