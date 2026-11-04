// Heimdall's Show Islands on Streams ("hide all islands"): `streams.json` in the data
// folder holds {"shown": true} while the islands appear in screen shares, streams and screenshots (Discord, OBS, macOS
// capture). Missing or false, the default: every island window is unshared (`sharingType = .none`). Read when the
// first window is made, then watched: a change applies to every island window at once (Rust: `mac/streams.rs`).
import AppKit
import IslandModel

enum Streams {
    private static var current: Bool?
    private static var watch: FileWatch?
    static var file: URL { Paths.root.appendingPathComponent("streams.json") }

    static func read() -> Bool {
        guard let o = IslandFile.read(file), let v = o["shown"] else { return false }
        if let b = v as? Bool { return b }
        return (v as? NSNumber)?.doubleValue == 1
    }
    static var shown: Bool {
        if let c = current { return c }
        let v = read()
        current = v
        return v
    }
    static var sharing: NSWindow.SharingType { shown ? .readOnly : .none }

    /// Watches the file from now on (the app's start); `windows` are every island window at the time of a change.
    static func start(windows: @escaping () -> [NSWindow]) {
        let apply = {
            let v = read()
            guard v != shown else { return }
            current = v
            for w in windows() { w.sharingType = sharing }
        }
        watch = FileWatch(file, onChange: apply)
        apply()  // a write between the first read and the watch is not missed
    }
}
