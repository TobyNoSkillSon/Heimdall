// What a viewer island shows: the renderer picked from the file's type, and the proposed actions on top of it.
import Foundation
import UniformTypeIdentifiers

public enum ViewerKind: Equatable {
    case markdown
    /// Plain text or source code; `language` is a lowercase hint for light colouring ("swift", "py", "json"…, nil: plain).
    case text(language: String?)
    case image
    case pdf
    case audio
    case video
    case html
    /// Anything else: QuickLook.
    case other
    case missing

    /// The file a viewer `path` names: `file://` URLs and `~` are accepted.
    public static func url(for rawPath: String) -> URL {
        if rawPath.hasPrefix("file://"), let u = URL(string: rawPath) { return u.standardizedFileURL }
        return URL(fileURLWithPath: (rawPath as NSString).expandingTildeInPath).standardizedFileURL
    }

    static let markdownExts: Set<String> = ["md", "markdown", "mdown", "mkd", "mdx"]
    static let codeExts: Set<String> = ["swift", "rs", "py", "js", "mjs", "cjs", "ts", "tsx", "jsx", "c", "h", "m", "mm", "cpp", "cc", "hpp",
                                        "go", "rb", "java", "kt", "sh", "zsh", "bash", "fish", "json", "jsonl", "toml", "yaml", "yml",
                                        "css", "scss", "sql", "lua", "php", "cs", "xml", "plist", "ini", "conf", "cfg", "env", "log",
                                        "csv", "tsv", "txt", "text", "diff", "patch", "metal", "glsl", "zig", "nix", "makefile", "dockerfile"]
    static let plainHints: Set<String> = ["txt", "text", "log", "csv", "tsv"]

    /// Picks the renderer by extension and UTType; files with no known type are sniffed (UTF-8 text → text, else QuickLook).
    public static func detect(_ url: URL, fileManager fm: FileManager = .default) -> ViewerKind {
        var isDir: ObjCBool = false
        guard fm.fileExists(atPath: url.path, isDirectory: &isDir) else { return .missing }
        if isDir.boolValue { return .other }
        let ext = url.pathExtension.lowercased()
        let name = url.lastPathComponent.lowercased()
        if markdownExts.contains(ext) { return .markdown }
        if ext == "html" || ext == "htm" || ext == "xhtml" { return .html }
        if ext == "svg" { return .html }   // WebKit draws SVG crisply at any size
        if codeExts.contains(ext) { return .text(language: plainHints.contains(ext) ? nil : ext) }
        if ["makefile", "dockerfile", "license", "readme"].contains(name) { return .text(language: nil) }
        if let t = UTType(filenameExtension: ext) {
            if t.conforms(to: .pdf) { return .pdf }
            if t.conforms(to: .html) { return .html }
            if t.conforms(to: .image) { return .image }
            if t.conforms(to: .audio) { return .audio }
            if t.conforms(to: .movie) || t.conforms(to: .video) || t.conforms(to: .audiovisualContent) { return .video }
            if t.conforms(to: .sourceCode) || t.conforms(to: .script) { return .text(language: ext) }
            if t.conforms(to: .plainText) || t.conforms(to: .json) || t.conforms(to: .xml) || t.conforms(to: .commaSeparatedText) { return .text(language: nil) }
            if t.isDeclared && !t.isDynamic { return .other }
        }
        return looksLikeText(url) ? .text(language: nil) : .other
    }

    /// The first 4 KB decode as UTF-8 and hold no NUL bytes.
    static func looksLikeText(_ url: URL) -> Bool {
        guard let h = try? FileHandle(forReadingFrom: url) else { return false }
        defer { try? h.close() }
        guard let d = try? h.read(upToCount: 4096), !d.isEmpty else { return true }
        if d.contains(0) { return false }
        // A multi-byte character cut at the 4 KB boundary is still text.
        for cut in 0..<4 where d.count > cut {
            if String(data: d.prefix(d.count - cut), encoding: .utf8) != nil { return true }
        }
        return false
    }

    /// A short name for logs and tests.
    public var name: String {
        switch self {
        case .markdown: return "markdown"
        case let .text(l): return l.map { "code:\($0)" } ?? "text"
        case .image: return "image"
        case .pdf: return "pdf"
        case .audio: return "audio"
        case .video: return "video"
        case .html: return "html"
        case .other: return "quicklook"
        case .missing: return "missing"
        }
    }
}

/// A proposed action on a viewer: `{"label": "Do all of it", "action": "do-all"}`. A click writes a `button` response.
public struct ViewerAction: Equatable {
    public let label: String
    public let action: String

    public init(label: String, action: String) { self.label = label; self.action = action }

    /// A label made into an action id: lowercase words joined by dashes ("Do all of it" → "do-all-of-it").
    public static func slug(_ label: String) -> String {
        let words = label.lowercased().split { !($0.isLetter || $0.isNumber) }
        return words.isEmpty ? "ok" : words.joined(separator: "-")
    }

    /// Parses an island's `actions`: objects with a label (and an action, else the label's slug) or bare strings.
    /// Entries without a label are skipped; duplicates by action keep the first.
    public static func parse(_ value: Any?) -> [ViewerAction] {
        guard let arr = value as? [Any] else { return [] }
        var out: [ViewerAction] = []
        for e in arr {
            var a: ViewerAction?
            if let s = e as? String { a = fromCLI(s) }
            else if let o = e as? [String: Any] {
                let label = ((o["label"] as? String) ?? (o["title"] as? String))?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
                if !label.isEmpty {
                    let act = ((o["action"] as? String) ?? (o["id"] as? String))?.trimmingCharacters(in: .whitespaces) ?? ""
                    a = ViewerAction(label: label, action: act.isEmpty ? slug(label) : act)
                }
            }
            if let a, !out.contains(where: { $0.action == a.action }) { out.append(a) }
        }
        return out
    }

    /// The CLI's `--action "Label=action"` (no `=`: the action is the label's slug).
    public static func fromCLI(_ s: String) -> ViewerAction? {
        let parts = s.split(separator: "=", maxSplits: 1, omittingEmptySubsequences: false).map { $0.trimmingCharacters(in: .whitespaces) }
        guard let label = parts.first, !label.isEmpty else { return nil }
        let act = parts.count > 1 ? parts[1] : ""
        return ViewerAction(label: label, action: act.isEmpty ? slug(label) : act)
    }

    public var json: [String: Any] { ["label": label, "action": action] }

    /// Rows of at most three, in order (the first is the primary).
    public static func rows(_ actions: [ViewerAction]) -> [[ViewerAction]] {
        stride(from: 0, to: actions.count, by: 3).map { Array(actions[$0..<min($0 + 3, actions.count)]) }
    }
}

extension Island {
    /// A viewer's proposed actions (`actions`).
    public var viewerActions: [ViewerAction] { ViewerAction.parse(raw["actions"]) }
}
