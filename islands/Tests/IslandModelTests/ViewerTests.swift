import XCTest
@testable import IslandModel

final class ViewerTests: XCTestCase {
    private var dir: URL!

    override func setUpWithError() throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("vk-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws { try? FileManager.default.removeItem(at: dir) }

    private func file(_ name: String, _ data: Data = Data("hello\n".utf8)) -> URL {
        let u = dir.appendingPathComponent(name)
        try? data.write(to: u)
        return u
    }

    func testKindsByExtension() {
        let cases: [(String, String)] = [
            ("notes.md", "markdown"), ("README.markdown", "markdown"),
            ("main.swift", "code:swift"), ("lib.rs", "code:rs"), ("x.py", "code:py"), ("data.json", "code:json"),
            ("log.txt", "text"), ("server.log", "text"), ("table.csv", "text"),
            ("photo.png", "image"), ("photo.JPG", "image"), ("shot.heic", "image"), ("anim.gif", "image"),
            ("paper.pdf", "pdf"),
            ("song.mp3", "audio"), ("voice.m4a", "audio"), ("take.wav", "audio"), ("clip.aiff", "audio"), ("x.flac", "audio"),
            ("clip.mov", "video"), ("clip.mp4", "video"), ("clip.m4v", "video"),
            ("page.html", "html"), ("page.htm", "html"), ("logo.svg", "html"),
            ("deck.key", "quicklook"), ("sheet.xlsx", "quicklook"), ("doc.docx", "quicklook"), ("arch.zip", "quicklook"),
        ]
        for (name, want) in cases {
            XCTAssertEqual(ViewerKind.detect(file(name)).name, want, name)
        }
    }

    func testUnknownTypesAreSniffed() {
        XCTAssertEqual(ViewerKind.detect(file("Makefile")).name, "text")
        XCTAssertEqual(ViewerKind.detect(file("notes.zzunknown", Data("just words\n".utf8))).name, "text")
        XCTAssertEqual(ViewerKind.detect(file("blob.zzunknown", Data([0x00, 0xFF, 0x10, 0x00]))).name, "quicklook")
        XCTAssertEqual(ViewerKind.detect(file("empty.zzunknown", Data())).name, "text")
        // A multi-byte character cut by the 4 KB sniff window is still text.
        let s = String(repeating: "a", count: 4095) + "é tail"
        XCTAssertEqual(ViewerKind.detect(file("cut.zzunknown", Data(s.utf8))).name, "text")
    }

    func testMissingAndFolders() {
        XCTAssertEqual(ViewerKind.detect(dir.appendingPathComponent("gone.md")), .missing)
        XCTAssertEqual(ViewerKind.detect(dir), .other)
    }

    func testPathForms() {
        XCTAssertEqual(ViewerKind.url(for: "file:///tmp/a%20b.md").path, "/tmp/a b.md")
        XCTAssertEqual(ViewerKind.url(for: "~/x.md").path, (NSHomeDirectory() as NSString).appendingPathComponent("x.md"))
        XCTAssertEqual(ViewerKind.url(for: "/tmp/../tmp/y.md").path, "/tmp/y.md")
    }

    func testActionsParse() {
        let raw: [Any] = [
            ["label": "Do all of it", "action": "do-all"],
            ["label": "Only the first"],                       // action from the label
            "Skip=skip",                                        // CLI form
            ["action": "nolabel"],                              // skipped
            ["label": "  ", "action": "blank"],                 // skipped
            ["label": "Again", "action": "do-all"],             // duplicate action skipped
            42,                                                 // junk skipped
        ]
        let a = ViewerAction.parse(raw)
        XCTAssertEqual(a, [ViewerAction(label: "Do all of it", action: "do-all"),
                           ViewerAction(label: "Only the first", action: "only-the-first"),
                           ViewerAction(label: "Skip", action: "skip")])
        XCTAssertEqual(ViewerAction.parse(nil), [])
        XCTAssertEqual(ViewerAction.parse("nope"), [])
        XCTAssertEqual(Island(id: "v", raw: ["actions": raw]).viewerActions.count, 3)
    }

    func testCLIActionForm() {
        XCTAssertEqual(ViewerAction.fromCLI("Do all=do-all"), ViewerAction(label: "Do all", action: "do-all"))
        XCTAssertEqual(ViewerAction.fromCLI("Ship it!"), ViewerAction(label: "Ship it!", action: "ship-it"))
        XCTAssertEqual(ViewerAction.fromCLI("Keep = "), ViewerAction(label: "Keep", action: "keep"))
        XCTAssertEqual(ViewerAction.fromCLI("a=b=c"), ViewerAction(label: "a", action: "b=c"))
        XCTAssertNil(ViewerAction.fromCLI("=x"))
        XCTAssertNil(ViewerAction.fromCLI(""))
    }

    func testRowsOfThreePrimaryFirst() {
        let a = (1...5).map { ViewerAction(label: "A\($0)", action: "a\($0)") }
        let rows = ViewerAction.rows(a)
        XCTAssertEqual(rows.map(\.count), [3, 2])
        XCTAssertEqual(rows[0].first?.action, "a1")
        XCTAssertEqual(ViewerAction.rows([]).count, 0)
    }
}
