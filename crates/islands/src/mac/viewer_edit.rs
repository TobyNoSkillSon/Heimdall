//! Copying and web searches from the viewer (bug note 2026-11-05T03-17-56: the user could not copy a robot model's name
//! from a report, and the context menu's "Search With Google" opened Safari, not his default browser, Ego Lite).
//!
//! - The helper app has no main menu, so ⌘C and ⌘A never became `copy:` and `selectAll:` (AppKit sends those from
//!   the Edit menu's key equivalents). The viewer's root sends them itself (`edit_action`), down the key panel's
//!   responder chain: the page or the text view under the selection copies.
//! - "Search With Google" in WebKit's and the text view's context menus is a macOS service owned by Safari, so it
//!   opened Safari whatever the default browser. The viewer's web and text views put their own action on that item:
//!   the selection is searched at Google through `NSWorkspace`, which opens the default browser.

use std::cell::RefCell;

use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSEvent, NSMenu, NSTextView, NSView};
use objc2_foundation::{NSString, NSURL};

use super::look::ns;

/// The editing action a key equivalent means in the viewer: ⌘C copies, ⌘A selects all (⌘ alone, no ⌃ or ⌥).
pub fn edit_action(chars: &str, command: bool, control_or_option: bool) -> Option<&'static str> {
    if !command || control_or_option {
        return None;
    }
    match chars {
        "c" | "C" => Some("copy:"),
        "a" | "A" => Some("selectAll:"),
        _ => None,
    }
}

/// Characters searched at most (a long selection is cut at a word).
pub const SEARCH_CHARS: usize = 200;

/// The web search for a selection: whitespace collapsed, cut to `SEARCH_CHARS`, percent-encoded. `None` when empty.
pub fn search_url(text: &str) -> Option<String> {
    let q = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if q.is_empty() {
        return None;
    }
    let q: String = if q.chars().count() > SEARCH_CHARS {
        let cut: String = q.chars().take(SEARCH_CHARS).collect();
        cut.rsplit_once(' ').map(|(a, _)| a.to_owned()).unwrap_or(cut)
    } else {
        q
    };
    let mut enc = String::new();
    for b in q.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => enc.push(b as char),
            b' ' => enc.push('+'),
            _ => enc.push_str(&format!("%{b:02X}")),
        }
    }
    Some(format!("https://www.google.com/search?q={enc}"))
}

/// The context menu's web-search item: WebKit's (`WKMenuItemIdentifierSearchWeb`), AppKit's text view item
/// (`_searchWithGoogleFromMenu:`), or any item titled "Search With …" (the service, by its menu title).
pub fn is_search_item(identifier: &str, action: &str, title: &str) -> bool {
    identifier == "WKMenuItemIdentifierSearchWeb"
        || action.to_lowercase().contains("searchwith")
        || action.contains("SearchWeb")
        || title.starts_with("Search With ")
        || title.starts_with("Search with ")
}

/// Open a URL in the default browser.
pub fn open_in_browser(url: &str) -> bool {
    let Some(u) = NSURL::URLWithString(&ns(url)) else { return false };
    objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&u)
}

pub struct SearchIvars {
    view: RefCell<Weak<NSView>>,
    web: bool,
}

define_class!(
    /// The target of the retargeted search item: reads the view's selection and searches it in the default browser.
    #[unsafe(super(objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FIViewerWebSearch"]
    #[ivars = SearchIvars]
    pub struct WebSearch;

    impl WebSearch {
        #[unsafe(method(searchWeb:))]
        fn search_web(&self, _sender: Option<&AnyObject>) {
            let Some(view) = self.ivars().view.borrow().load() else { return };
            if self.ivars().web {
                let block = block2::RcBlock::new(move |v: *mut AnyObject, _e: *mut AnyObject| {
                    if v.is_null() {
                        return;
                    }
                    // SAFETY: the page's answer to a string expression is an NSString.
                    let s: Retained<NSString> = unsafe { msg_send![&*v, description] };
                    if let Some(u) = search_url(&s.to_string()) {
                        open_in_browser(&u);
                    }
                });
                unsafe {
                    let _: () = msg_send![&*view, evaluateJavaScript: &*ns("String(window.getSelection()||'')"), completionHandler: &*block];
                }
            } else {
                // SAFETY: only text views get a text search target.
                let tv: &NSTextView = unsafe { &*(&*view as *const NSView as *const NSTextView) };
                let all = tv.string();
                let r = tv.selectedRange();
                let len = all.length();
                if r.location >= len {
                    return;
                }
                let r = objc2_foundation::NSRange::new(r.location, r.length.min(len - r.location));
                let sel = all.substringWithRange(r);
                if let Some(u) = search_url(&sel.to_string()) {
                    open_in_browser(&u);
                }
            }
        }
    }
);

impl WebSearch {
    fn new(view: &NSView, web: bool) -> Retained<WebSearch> {
        let mtm = MainThreadMarker::from(view);
        let this = Self::alloc(mtm).set_ivars(SearchIvars { view: RefCell::new(Weak::from(view)), web });
        unsafe { msg_send![super(this), init] }
    }
}

/// The menu's search items point at a `WebSearch` for `view` (kept alive as the item's represented object).
pub fn patch_menu(menu: &NSMenu, view: &NSView, web: bool) {
    let mut target: Option<Retained<WebSearch>> = None;
    for item in menu.itemArray().iter() {
        let identifier = unsafe {
            let id: Option<Retained<NSString>> = msg_send![&*item, identifier];
            id.map(|s| s.to_string()).unwrap_or_default()
        };
        let action = item.action().map(|s| s.name().to_string_lossy().into_owned()).unwrap_or_default();
        if !is_search_item(&identifier, &action, &item.title().to_string()) {
            continue;
        }
        let t = target.get_or_insert_with(|| WebSearch::new(view, web)).clone();
        unsafe {
            item.setTarget(Some(&*t));
            item.setAction(Some(sel!(searchWeb:)));
            item.setRepresentedObject(Some(&*t));
        }
    }
}

/// The viewer's web view class: WKWebView whose context menu searches in the default browser (`patch_menu`).
pub fn web_view_class() -> &'static AnyClass {
    static CLS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let p = *CLS.get_or_init(|| {
        let sup = AnyClass::get(c"WKWebView").expect("WebKit");
        if let Some(c) = AnyClass::get(c"FIViewerWebView") {
            return c as *const AnyClass as usize;
        }
        let mut b = ClassBuilder::new(c"FIViewerWebView", sup).expect("FIViewerWebView");
        unsafe extern "C-unwind" fn will_open(this: &AnyObject, _cmd: Sel, menu: *mut NSMenu, event: *mut NSEvent) {
            if let Some(m) = unsafe { menu.as_ref() } {
                // SAFETY: `this` is the web view, an NSView.
                patch_menu(m, unsafe { &*(this as *const AnyObject as *const NSView) }, true);
            }
            let sup = AnyClass::get(c"WKWebView").expect("WebKit");
            unsafe {
                let _: () = msg_send![super(this, sup), willOpenMenu: menu, withEvent: event];
            }
        }
        // A page that takes its own pinch (`model::page_pinch`) gets the trackpad's magnify and two-finger double-tap
        // first (`set_pinch_hook`); a page without a hook goes to WebKit's own, as before.
        unsafe extern "C-unwind" fn magnify(this: &AnyObject, _cmd: Sel, event: *mut NSEvent) {
            pinch_or_super(this, event, false);
        }
        unsafe extern "C-unwind" fn smart_magnify(this: &AnyObject, _cmd: Sel, event: *mut NSEvent) {
            pinch_or_super(this, event, true);
        }
        unsafe {
            b.add_method(sel!(willOpenMenu:withEvent:), will_open as unsafe extern "C-unwind" fn(_, _, _, _));
            b.add_method(sel!(magnifyWithEvent:), magnify as unsafe extern "C-unwind" fn(_, _, _));
            b.add_method(sel!(smartMagnifyWithEvent:), smart_magnify as unsafe extern "C-unwind" fn(_, _, _));
        }
        b.register() as *const AnyClass as usize
    });
    // SAFETY: a registered class lives for the process.
    unsafe { &*(p as *const AnyClass) }
}

/// A web view's pinch hook: (the web view, the event, whether it is the two-finger double-tap) → true when it took
/// the event (WebKit's own magnify does not run).
pub type PinchHook = std::rc::Rc<dyn Fn(&NSView, &NSEvent, bool) -> bool>;

thread_local! {
    /// Pinch hooks by web view (its address); a view without one magnifies as WebKit does.
    static PINCH_HOOKS: RefCell<std::collections::HashMap<usize, PinchHook>> = RefCell::new(std::collections::HashMap::new());
}

/// Sets (or with `None` removes) `view`'s pinch hook.
pub fn set_pinch_hook(view: &NSView, hook: Option<PinchHook>) {
    let key = view as *const NSView as usize;
    PINCH_HOOKS.with(|h| match hook {
        Some(f) => {
            h.borrow_mut().insert(key, f);
        }
        None => {
            h.borrow_mut().remove(&key);
        }
    });
}

fn pinch_or_super(this: &AnyObject, event: *mut NSEvent, smart: bool) {
    // SAFETY: `this` is the web view, an NSView.
    let view = unsafe { &*(this as *const AnyObject as *const NSView) };
    let hook = PINCH_HOOKS.with(|h| h.borrow().get(&(view as *const NSView as usize)).cloned());
    if let (Some(f), Some(e)) = (hook, unsafe { event.as_ref() }) {
        if f(view, e, smart) {
            return;
        }
    }
    // SAFETY: `event` came from AppKit (or is null, which WebKit tolerates as AppKit does).
    unsafe { webkit_magnify(view, event, smart) };
}

/// WebKit's own magnify (or smart magnify) for `view` with `event`: what a page that does not take the pinch gets,
/// and what a gesture the page declined is replayed to.
///
/// # Safety
/// `view` is an instance of `web_view_class`; `event` is a magnify event or null.
pub unsafe fn webkit_magnify(view: &NSView, event: *mut NSEvent, smart: bool) {
    let sup = AnyClass::get(c"WKWebView").expect("WebKit");
    let this = view as *const NSView as *const AnyObject;
    unsafe {
        if smart {
            let _: () = msg_send![super(&*this, sup), smartMagnifyWithEvent: event];
        } else {
            let _: () = msg_send![super(&*this, sup), magnifyWithEvent: event];
        }
    }
}

define_class!(
    /// The viewer's text view: its context menu searches in the default browser (`patch_menu`).
    #[unsafe(super(NSTextView, objc2_app_kit::NSText, NSView, objc2_app_kit::NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FIViewerTextView"]
    pub struct ViewerTextView;

    impl ViewerTextView {
        #[unsafe(method_id(menuForEvent:))]
        fn menu_for_event(&self, e: &NSEvent) -> Option<Retained<NSMenu>> {
            let m: Option<Retained<NSMenu>> = unsafe { msg_send![super(self), menuForEvent: e] };
            if let Some(m) = &m {
                patch_menu(m, self, false);
            }
            m
        }
    }
);

/// A TextKit 1 text view of the viewer's class.
pub fn text_view(mtm: MainThreadMarker) -> Retained<NSTextView> {
    let this = ViewerTextView::alloc(mtm).set_ivars(());
    let tv: Retained<ViewerTextView> = unsafe { msg_send![super(this), initUsingTextLayoutManager: false] };
    Retained::into_super(tv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_c_copies_and_command_a_selects_all_only_with_command_alone() {
        assert_eq!(edit_action("c", true, false), Some("copy:"));
        assert_eq!(edit_action("C", true, false), Some("copy:"), "caps lock");
        assert_eq!(edit_action("a", true, false), Some("selectAll:"));
        assert_eq!(edit_action("c", false, false), None, "plain c types nothing here");
        assert_eq!(edit_action("c", true, true), None, "⌃⌘C or ⌥⌘C is something else");
        assert_eq!(edit_action("=", true, false), None, "zoom keys stay the zoom's");
    }

    #[test]
    fn a_selection_becomes_one_google_search_in_the_default_browser() {
        assert_eq!(search_url("  KUKA KR 6\n R900 ").as_deref(), Some("https://www.google.com/search?q=KUKA+KR+6+R900"));
        assert_eq!(search_url("Fanuc LR Mate 200iD/4S & co").as_deref(), Some("https://www.google.com/search?q=Fanuc+LR+Mate+200iD%2F4S+%26+co"));
        assert_eq!(search_url("Łódź").as_deref(), Some("https://www.google.com/search?q=%C5%81%C3%B3d%C5%BA"));
        assert_eq!(search_url(" \n\t"), None);
        let long = "word ".repeat(100);
        let u = search_url(&long).unwrap();
        let q = u.trim_start_matches("https://www.google.com/search?q=");
        assert!(q.len() <= SEARCH_CHARS && q.ends_with("word"), "cut at a word: {q}");
    }

    #[test]
    fn the_search_item_is_found_in_webkit_and_text_view_menus_and_nothing_else_is() {
        assert!(is_search_item("WKMenuItemIdentifierSearchWeb", "", "Search with Google"));
        assert!(is_search_item("", "_searchWithGoogleFromMenu:", "Search With Google"));
        assert!(is_search_item("", "", "Search With Bing"));
        assert!(!is_search_item("WKMenuItemIdentifierCopy", "copy:", "Copy"));
        assert!(!is_search_item("", "_lookUpClicked:", "Look Up “KUKA”"));
        assert!(!is_search_item("", "", "Searching"));
    }
}
