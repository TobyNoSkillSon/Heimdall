// axtool (test-only, VM guest): AX driver using AXUIElement directly (needs only the Accessibility grant of the SSH runner;
// no Apple Events, so no Automation permission). Built on the host, run in the guest.
//   axtool dump <pid|name>                      every window's elements (role | title | value | description)
//   axtool press <pid|name> <title>             AXPress the first button whose title or description matches
//   axtool set <pid|name> <index> <text>        set the value of the index-th text field (0-based) in the front window
//   axtool menu <pid|name> [item]               press the app's status item (menu bar extra), dump it, press [item]
//   axtool row|item <pid|name> <text>           select a sidebar row / browser item showing <text> (open panels)
//   axtool type <pid|name> <text|{return}…>     guest keyboard events; axtool click <pid|name> <x> <y>: guest pointer
// Refuses to run outside a virtual machine: it never drives the host's apps, cursor or keyboard.
import ApplicationServices
import AppKit
import Foundation
setvbuf(stdout, nil, _IOLBF, 0)
var vmm: Int32 = 0; var vmmSize = MemoryLayout<Int32>.size
guard sysctlbyname("kern.hv_vmm_present", &vmm, &vmmSize, nil, 0) == 0, vmm == 1 else { print("axtool runs only in a VM guest"); exit(2) }
guard CommandLine.arguments.count >= 3 else { print("usage"); exit(2) }
let cmd = CommandLine.arguments[1], who = CommandLine.arguments[2]
let pid: pid_t = pid_t(who) ?? (NSWorkspace.shared.runningApplications.first { $0.localizedName == who || $0.executableURL?.lastPathComponent == who }?.processIdentifier ?? -1)
guard pid > 0 else { print("NO PROCESS \(who)"); exit(3) }
let app = AXUIElementCreateApplication(pid)
AXUIElementSetMessagingTimeout(app, 5)
func a(_ e: AXUIElement, _ k: String) -> AnyObject? { var v: AnyObject?; return AXUIElementCopyAttributeValue(e, k as CFString, &v) == .success ? v : nil }
func s(_ e: AXUIElement, _ k: String) -> String { if let v = a(e, k) { if let x = v as? String { return x }; if let n = v as? NSNumber { return n.stringValue } }; return "" }
func kids(_ e: AXUIElement) -> [AXUIElement] { (a(e, kAXChildrenAttribute) as? [AXUIElement]) ?? [] }
var fields: [AXUIElement] = []
func walk(_ e: AXUIElement, _ d: Int, _ visit: (AXUIElement, Int) -> Bool) -> Bool {
    if d > 12 { return false }
    if visit(e, d) { return true }
    for k in kids(e) { if walk(k, d + 1, visit) { return true } }
    return false
}
func line(_ e: AXUIElement, _ d: Int) -> String {
    let en = (a(e, kAXEnabledAttribute) as? Bool) == false ? " [disabled]" : ""
    let mk = s(e, "AXMenuItemMarkChar"); return String(repeating: "  ", count: d) + (mk.isEmpty ? "" : "[\(mk)] ") + "\(s(e, kAXRoleAttribute)) | \(s(e, kAXTitleAttribute)) | \(s(e, kAXValueAttribute).prefix(160).replacingOccurrences(of: "\n", with: " ")) | \(s(e, kAXDescriptionAttribute))\(en)"
}
let windows = (a(app, kAXWindowsAttribute) as? [AXUIElement]) ?? []
switch cmd {
case "dump":
    for (i, w) in windows.enumerated() {
        print("WINDOW \(i) \(s(w, kAXTitleAttribute))")
        _ = walk(w, 1) { e, d in
            let r = s(e, kAXRoleAttribute)
            if !["AXGroup", "AXScrollArea", "AXSplitGroup", "AXLayoutArea", "AXUnknown"].contains(r) || !s(e, kAXTitleAttribute).isEmpty { print(line(e, d)) }
            return false }
    }
case "press":
    let t = CommandLine.arguments[3]
    var done = false
    for w in windows where !done {
        done = walk(w, 1) { e, _ in
            guard s(e, kAXRoleAttribute) == "AXButton", s(e, kAXTitleAttribute) == t || s(e, kAXDescriptionAttribute) == t else { return false }
            if (a(e, kAXEnabledAttribute) as? Bool) == false { print("DISABLED \(t)"); return true }
            print(AXUIElementPerformAction(e, kAXPressAction as CFString) == .success ? "pressed \(t)" : "PRESS FAILED \(t)"); return true }
    }
    if !done { print("NOT FOUND \(t)"); exit(4) }
case "set":
    let idx = Int(CommandLine.arguments[3])!, text = CommandLine.arguments[4]
    if let w = windows.first { _ = walk(w, 1) { e, _ in if s(e, kAXRoleAttribute) == "AXTextField" { fields.append(e) }; return false } }
    guard idx < fields.count else { print("NO FIELD \(idx) of \(fields.count)"); exit(4) }
    print(AXUIElementSetAttributeValue(fields[idx], kAXValueAttribute as CFString, text as CFString) == .success ? "set field \(idx)" : "SET FAILED")
case "menu":
    guard let bar = a(app, "AXExtrasMenuBar") as! AXUIElement? else { print("NO STATUS ITEM"); exit(4) }
    for item in kids(bar) {
        AXUIElementPerformAction(item, kAXPressAction as CFString)
        Thread.sleep(forTimeInterval: 1.0)
        _ = walk(item, 1) { e, d in
            let r = s(e, kAXRoleAttribute); if r == "AXMenuItem" || r == "AXMenu" { print(line(e, d)) }; return false }
        if CommandLine.arguments.count > 3 {
            let t = CommandLine.arguments[3]
            let hit = walk(item, 1) { e, _ in
                guard s(e, kAXRoleAttribute) == "AXMenuItem", s(e, kAXTitleAttribute) == t else { return false }
                print(AXUIElementPerformAction(e, kAXPressAction as CFString) == .success ? "pressed menu \(t)" : "MENU PRESS FAILED"); return true }
            if !hit { print("NO MENU ITEM \(t)"); AXUIElementPerformAction(item, kAXCancelAction as CFString) }
        } else { AXUIElementPerformAction(item, kAXCancelAction as CFString) }
    }
case "type":
    // Guest keyboard events (CGEvent in the guest session; never the host). type <any> <text> | type <any> {cmd+shift+g} | {return}
    let t = CommandLine.arguments[3]
    AXUIElementSetAttributeValue(app, kAXFrontmostAttribute as CFString, kCFBooleanTrue); usleep(300000)
    func key(_ code: CGKeyCode, _ flags: CGEventFlags = []) {
        for down in [true, false] { let e = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down)!; e.flags = flags; e.post(tap: .cghidEventTap); usleep(30000) } }
    switch t {
    case "{cmd+shift+g}": key(5, [.maskCommand, .maskShift])
    case "{return}": key(36)
    case "{esc}": key(53)
    case "{cmd+a}": key(0, [.maskCommand])
    default:
        for ch in t.utf16 { var c = ch
            for down in [true, false] { let e = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: down)!; e.keyboardSetUnicodeString(stringLength: 1, unicodeString: &c); e.post(tap: .cghidEventTap); usleep(20000) } }
    }
    print("typed \(t)")
case "row":
    // Select the n-th (default first) AXRow whose subtree shows the given text, through AX only.
    let t = CommandLine.arguments[3]; let nth = CommandLine.arguments.count > 4 ? Int(CommandLine.arguments[4])! : 0
    var rows: [AXUIElement] = []
    for w in windows { _ = walk(w, 1) { e, _ in
        if s(e, kAXRoleAttribute) == "AXRow" { var hit = false
            _ = walk(e, 0) { c, _ in if (s(c, kAXValueAttribute) == t || s(c, kAXTitleAttribute) == t) { hit = true; return true }; return false }
            if hit { rows.append(e) } }
        return false } }
    guard nth < rows.count else { print("NO ROW \(t) (\(rows.count))"); exit(4) }
    let r = rows[nth]
    var ok = AXUIElementSetAttributeValue(r, kAXSelectedAttribute as CFString, kCFBooleanTrue) == .success
    if !ok, let parent = a(r, kAXParentAttribute) { ok = AXUIElementSetAttributeValue(parent as! AXUIElement, kAXSelectedRowsAttribute as CFString, [r] as CFArray) == .success }
    print(ok ? "selected row \(t) (\(nth) of \(rows.count))" : "SELECT FAILED \(t)")
case "item":
    // Select a browser/list item through AX: the element holding a text field with that value.
    let t = CommandLine.arguments[3]; var done = false
    for w in windows where !done { done = walk(w, 1) { e, _ in
        guard s(e, kAXRoleAttribute) == "AXTextField", s(e, kAXValueAttribute) == t, let item = a(e, kAXParentAttribute) else { return false }
        let it = item as! AXUIElement
        var ok = AXUIElementSetAttributeValue(it, kAXSelectedAttribute as CFString, kCFBooleanTrue) == .success
        if !ok, let list = a(it, kAXParentAttribute) { ok = AXUIElementSetAttributeValue(list as! AXUIElement, kAXSelectedChildrenAttribute as CFString, [it] as CFArray) == .success }
        print(ok ? "selected item \(t) (\(s(it, kAXRoleAttribute)))" : "SELECT FAILED \(t) (\(s(it, kAXRoleAttribute)))"); return true } }
    if !done { print("NO ITEM \(t)"); exit(4) }
case "click":
    // Guest pointer click at guest screen point (x y); the guest's own cursor, never the host's.
    let p = CGPoint(x: Double(CommandLine.arguments[3])!, y: Double(CommandLine.arguments[4])!)
    for t in [CGEventType.mouseMoved, .leftMouseDown, .leftMouseUp] { CGEvent(mouseEventSource: nil, mouseType: t, mouseCursorPosition: p, mouseButton: .left)!.post(tap: .cghidEventTap); usleep(80000) }
    print("clicked \(p)")
default: print("unknown"); exit(2)
}
