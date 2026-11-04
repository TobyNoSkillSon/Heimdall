// Floating Islands: small black islands over every app and Space, driven entirely by files.
//   FloatingIslands [--max-hours N (0: no limit)] [--parent-pid PID] [--selftest] [--flicktest] [--render DIR]
// Data folder: ~/Library/Application Support/FloatingIslands (FLOATING_ISLANDS_DIR overrides it).
import AppKit
import IslandModel

let args = CommandLine.arguments
if let i = args.firstIndex(of: "--viewer-shots"), i + 2 < args.count { ViewerShots.run(out: args[i + 1], fixtures: args[i + 2]) }   // offscreen viewer renders (ViewerShots.swift)
Paths.ensure()

// One instance per data folder.
let lockFD = open(Paths.root.appendingPathComponent(".app.lock").path, O_CREAT | O_RDWR, 0o644)
if lockFD < 0 || flock(lockFD, LOCK_EX | LOCK_NB) != 0 {
    FileHandle.standardError.write("FloatingIslands is already running for \(Paths.root.path)\n".data(using: .utf8)!)
    exit(1)
}

try? "\(getpid())".write(to: Paths.pid, atomically: true, encoding: .utf8)
// Quit cleanly on SIGTERM (`islands quit`, test traps): windows close, state and pid files go.
signal(SIGTERM, SIG_IGN)
let term = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
term.setEventHandler { NSApp.terminate(nil) }
term.resume()
NotificationCenter.default.addObserver(forName: NSApplication.willTerminateNotification, object: nil, queue: nil) { _ in
    try? FileManager.default.removeItem(at: Paths.pid)
    try? FileManager.default.removeItem(at: Paths.state)
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
// Lab renders build islands offscreen: no window is ever shown.
if args.contains("--render") || args.contains("--render-lane") { IslandController.offscreen = true }
let controller = AppController()
controller.start()

// A hard lifetime, so a forgotten test instance never lingers (default 24 h). `--max-hours 0` means no limit:
// Heimdall launches and supervises the helper and quits it itself.
if let maxHours = Lifetime.maxHours(args) {
    DispatchQueue.main.asyncAfter(deadline: .now() + maxHours * 3600) { NSApp.terminate(nil) }
}
// `--parent-pid N`: quit when Heimdall exits, however it ends (a crash, a kill, a takeover). A kernel process-exit
// event, no polling; a parent already gone at launch quits at once.
var parentExit: DispatchSourceProcess?
if let parent = Lifetime.parentPID(args) {
    let source = DispatchSource.makeProcessSource(identifier: parent, eventMask: .exit, queue: .main)
    source.setEventHandler { NSApp.terminate(nil) }
    source.resume()
    parentExit = source
    if kill(parent, 0) != 0 && errno == ESRCH { DispatchQueue.main.async { NSApp.terminate(nil) } }
}

if args.contains("--selftest") { SelfTest(controller).run() }
if args.contains("--flicktest") { FlickTest(controller).run() }
if let i = args.firstIndex(of: "--render"), i + 1 < args.count { Render(controller, out: args[i + 1]).run() }
if let i = args.firstIndex(of: "--render-lane"), i + 1 < args.count { LaneRender(controller, out: args[i + 1]).run() }   // LaneRender.swift

app.run()
