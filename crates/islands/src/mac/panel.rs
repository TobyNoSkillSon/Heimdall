//! The island's window and view: a borderless, non-activating panel holding one flipped layer-hosting view. Mouse and
//! key events go through the window's own event path to the island's controller.

use std::cell::{Cell, RefCell};
use std::rc::Weak;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSDragOperation, NSDraggingContext, NSDraggingSession, NSDraggingSource, NSEvent, NSEventModifierFlags, NSMenu, NSMenuItem, NSPanel, NSResponder, NSView, NSWindowAnimationBehavior,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize};

use super::island::Ctl;
use super::look::ns;

pub struct PanelIvars {
    pub accepts_key: Cell<bool>,
    pub ctl: RefCell<Weak<Ctl>>,
}

define_class!(
    #[unsafe(super(NSPanel, objc2_app_kit::NSWindow, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FIIslandPanel"]
    #[ivars = PanelIvars]
    pub struct IslandPanel;

    impl IslandPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            self.ivars().accepts_key.get()
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            false
        }

        #[unsafe(method(cancelOperation:))]
        fn cancel_operation(&self, _sender: Option<&AnyObject>) {
            if let Some(c) = self.ivars().ctl.borrow().upgrade() {
                Ctl::close_viewer(&c);
            }
        }
    }
);

impl IslandPanel {
    pub fn make(mtm: MainThreadMarker) -> Retained<IslandPanel> {
        let this = Self::alloc(mtm).set_ivars(PanelIvars { accepts_key: Cell::new(false), ctl: RefCell::new(Weak::new()) });
        let r = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(28.0, 28.0));
        let p: Retained<IslandPanel> = unsafe {
            msg_send![super(this), initWithContentRect: r, styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                      backing: NSBackingStoreType::Buffered, defer: false]
        };
        p.setFloatingPanel(true);
        // Out of screen shares and streams unless Heimdall's Show Islands on Streams is on (`streams`).
        p.setSharingType(super::streams::sharing());
        p.setLevel(objc2_app_kit::NSStatusWindowLevel);
        p.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        p.setOpaque(false);
        p.setBackgroundColor(Some(&NSColor::clearColor()));
        p.setHasShadow(false);
        p.setHidesOnDeactivate(false);
        p.setBecomesKeyOnlyIfNeeded(true);
        // SAFETY: we keep our own reference.
        unsafe { p.setReleasedWhenClosed(false) };
        p.setAnimationBehavior(NSWindowAnimationBehavior::None);
        p.setMovable(false);
        p
    }
}

pub struct ViewIvars {
    pub ctl: RefCell<Weak<Ctl>>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FIIslandView"]
    #[ivars = ViewIvars]
    pub struct IslandView;

    impl IslandView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            self.ctl().is_some_and(|c| c.is_viewer())
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            let Some(c) = self.ctl() else { return };
            if e.modifierFlags().contains(NSEventModifierFlags::Control) {
                self.show_menu(&c, e);
            } else {
                Ctl::mouse_down(&c, e);
            }
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, e: &NSEvent) {
            if let Some(c) = self.ctl() {
                self.show_menu(&c, e);
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            if let Some(c) = self.ctl() {
                Ctl::mouse_dragged(&c, e);
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, e: &NSEvent) {
            if let Some(c) = self.ctl() {
                Ctl::mouse_up(&c, e);
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, e: &NSEvent) {
            if e.keyCode() == 53 {
                if let Some(c) = self.ctl() {
                    Ctl::close_viewer(&c);
                }
            } else {
                let _: () = unsafe { msg_send![super(self), keyDown: e] };
            }
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, e: &NSEvent) {
            let took = self.ctl().is_some_and(|c| Ctl::scroll_wheel(&c, e));
            if !took {
                let _: () = unsafe { msg_send![super(self), scrollWheel: e] };
            }
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _e: &NSEvent) {
            if let Some(c) = self.ctl() {
                c.share_hover(true);
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _e: &NSEvent) {
            if let Some(c) = self.ctl() {
                c.share_hover(false);
            }
        }

        #[unsafe(method(fiMenuToggle:))]
        fn menu_toggle(&self, _s: Option<&AnyObject>) {
            if let Some(c) = self.ctl() {
                Ctl::menu_toggle(&c);
            }
        }

        #[unsafe(method(fiMenuClose:))]
        fn menu_close(&self, _s: Option<&AnyObject>) {
            if let Some(c) = self.ctl() {
                Ctl::close_island(&c);
            }
        }

        #[unsafe(method(fiMenuOpenTarget:))]
        fn menu_open_target(&self, _s: Option<&AnyObject>) {
            if let Some(c) = self.ctl() {
                Ctl::menu_open_target(&c);
            }
        }
    }

    unsafe impl NSObjectProtocol for IslandView {}

    // The viewer's drag handle hands out a copy of its file (`share.rs`).
    unsafe impl NSDraggingSource for IslandView {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_operations(&self, _s: &NSDraggingSession, _context: NSDraggingContext) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn session_ended(&self, _s: &NSDraggingSession, p: NSPoint, op: NSDragOperation) {
            super::share::LAB_ENDED.with(|e| e.set(Some((p.x, p.y, op.0))));
        }
    }
);

/// The island view as a drag source (`share.rs`).
pub fn drag_source(view: &NSView) -> Option<&ProtocolObject<dyn NSDraggingSource>> {
    let v: &IslandView = view.downcast_ref()?;
    Some(ProtocolObject::from_ref(v))
}

impl IslandView {
    pub fn make(mtm: MainThreadMarker) -> Retained<IslandView> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars { ctl: RefCell::new(Weak::new()) });
        unsafe { msg_send![super(this), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(28.0, 28.0))] }
    }

    pub fn ctl(&self) -> Option<std::rc::Rc<Ctl>> {
        self.ivars().ctl.borrow().upgrade()
    }

    /// Right-click (or control-click): open or minimise, open the target, close.
    fn show_menu(&self, c: &std::rc::Rc<Ctl>, e: &NSEvent) {
        let mtm = MainThreadMarker::from(self);
        let m = NSMenu::new(mtm);
        let item = |title: &str, action: objc2::runtime::Sel| -> Retained<NSMenuItem> {
            let i = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &ns(title), Some(action), &ns("")) };
            unsafe { i.setTarget(Some(self)) };
            i
        };
        m.addItem(&item(if c.expanded() { "Minimise" } else { "Open" }, sel!(fiMenuToggle:)));
        if let Some(t) = c.island().open() {
            if !crate::model::OpenTarget::missing(&t) {
                m.addItem(&item(&format!("Open {}", crate::model::OpenTarget::label(&t)), sel!(fiMenuOpenTarget:)));
            }
        }
        m.addItem(&item("Close", sel!(fiMenuClose:)));
        NSMenu::popUpContextMenu_withEvent_forView(&m, e, self);
    }
}

define_class!(
    #[unsafe(super(NSView, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FIFlippedView"]
    pub struct FlippedView;

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl FlippedView {
    pub fn make(mtm: MainThreadMarker, frame: NSRect) -> Retained<FlippedView> {
        unsafe { msg_send![Self::alloc(mtm), initWithFrame: frame] }
    }
    pub fn view(&self) -> &NSView {
        self
    }
}
