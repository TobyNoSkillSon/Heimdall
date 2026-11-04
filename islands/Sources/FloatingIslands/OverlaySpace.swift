// Keeping islands still while Spaces slide.
//
// collectionBehavior .canJoinAllSpaces puts a window on every Space, but the window still belongs to the Space
// stack: when the user swipes, WindowServer slides it out with the old Space and back in with the new one. The windows
// that stay still (menu bar, Dock, notch overlays) live in a Space of their own above the user Spaces. SkyLight lets
// a process create one: CGSSpaceCreate + CGSSpaceSetAbsoluteLevel + CGSShowSpaces, then CGSAddWindowsToSpaces.
// The symbols are private, so they are looked up at run time; if any is missing the islands fall back to
// canJoinAllSpaces (they still show on every Space, they just slide).
import AppKit

final class OverlaySpace {
    typealias Conn = Int32
    typealias SpaceID = UInt64
    private typealias DefaultConnection = @convention(c) () -> Conn
    private typealias SpaceCreate = @convention(c) (Conn, Int32, CFDictionary?) -> SpaceID
    private typealias SpaceSetLevel = @convention(c) (Conn, SpaceID, Int32) -> Void
    private typealias ShowSpaces = @convention(c) (Conn, CFArray) -> Void
    private typealias WindowsToSpaces = @convention(c) (Conn, CFArray, CFArray) -> Void

    static let shared = OverlaySpace()

    private let conn: Conn
    private let space: SpaceID
    private let add: WindowsToSpaces?
    let active: Bool

    private init() {
        // Lab switch for the comparison recordings: FI_OVERLAY=standard keeps plain canJoinAllSpaces windows.
        let wanted = ProcessInfo.processInfo.environment["FI_OVERLAY"] != "standard"
        let h = dlopen("/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight", RTLD_LAZY)
        func sym<T>(_ name: String, _: T.Type) -> T? {
            guard let h, let p = dlsym(h, name) else { return nil }
            return unsafeBitCast(p, to: T.self)
        }
        guard wanted,
              let dc = sym("_CGSDefaultConnection", DefaultConnection.self),
              let create = sym("CGSSpaceCreate", SpaceCreate.self),
              let level = sym("CGSSpaceSetAbsoluteLevel", SpaceSetLevel.self),
              let show = sym("CGSShowSpaces", ShowSpaces.self),
              let add = sym("CGSAddWindowsToSpaces", WindowsToSpaces.self) else {
            conn = 0; space = 0; self.add = nil; active = false
            return
        }
        conn = dc()
        space = create(conn, 1, nil)
        level(conn, space, 100)
        show(conn, [NSNumber(value: space)] as CFArray)
        self.add = add
        active = space != 0
    }

    /// Move a window into the overlay space (call once its window number exists).
    func adopt(_ w: NSWindow) {
        guard active, let add, w.windowNumber > 0 else { return }
        add(conn, [NSNumber(value: w.windowNumber)] as CFArray, [NSNumber(value: space)] as CFArray)
    }
}
