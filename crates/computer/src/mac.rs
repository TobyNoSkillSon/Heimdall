//! The real backend: AXUIElement over ApplicationServices, NSWorkspace for the apps, CGEvent for the off-by-default
//! pointer fallback (posted to the app's process, so the pointer never moves).
//!
//! One `Mac` lives for one tool call: it keeps every element it read (retained) so later steps of the call can act on
//! them, and releases them all when dropped.
use crate::{AppInfo, Backend, Node, Snapshot};
use anyhow::{bail, Result};
use std::cell::RefCell;
use std::ffi::{c_char, c_void, CString};
use std::time::{Duration, Instant};

type CF = *const c_void;
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGSize {
    w: f64,
    h: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateApplication(pid: i32) -> CF;
    fn AXUIElementCopyAttributeValue(element: CF, key: CF, value: *mut CF) -> i32;
    fn AXUIElementCopyMultipleAttributeValues(element: CF, keys: CF, options: u32, values: *mut CF) -> i32;
    fn AXUIElementSetAttributeValue(element: CF, key: CF, value: CF) -> i32;
    fn AXUIElementCopyActionNames(element: CF, names: *mut CF) -> i32;
    fn AXUIElementPerformAction(element: CF, action: CF) -> i32;
    fn AXUIElementSetMessagingTimeout(element: CF, seconds: f32) -> i32;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetType(value: CF) -> u32;
    fn AXValueGetValue(value: CF, kind: u32, out: *mut c_void) -> bool;
    fn CGEventCreateMouseEvent(source: CF, kind: u32, at: CGPoint, button: u32) -> CF;
    fn CGEventPostToPid(pid: i32, event: CF);
    fn CGEventSourceSecondsSinceLastEventType(state: i32, kind: u32) -> f64;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CF;
    static kCFTypeArrayCallBacks: c_void;
    fn CFRelease(value: CF);
    fn CFRetain(value: CF) -> CF;
    fn CFHash(value: CF) -> usize;
    fn CFEqual(a: CF, b: CF) -> bool;
    fn CFGetTypeID(value: CF) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFBooleanGetValue(value: CF) -> bool;
    fn CFNumberGetValue(value: CF, kind: i32, out: *mut c_void) -> bool;
    fn CFStringCreateWithCString(allocator: CF, text: *const c_char, encoding: u32) -> CF;
    fn CFStringGetLength(value: CF) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(value: CF, buffer: *mut c_char, size: isize, encoding: u32) -> bool;
    fn CFArrayGetCount(value: CF) -> isize;
    fn CFArrayGetValueAtIndex(value: CF, index: isize) -> CF;
    fn CFArrayCreate(allocator: CF, values: *const CF, count: isize, callbacks: *const c_void) -> CF;
}

const UTF8: u32 = 0x0800_0100;
/// Per-message timeout: a busy app answers late rather than hanging the call.
const MESSAGE_TIMEOUT_S: f32 = 0.5;
/// Walk budget: an Electron window holds thousands of elements.
const MAX_NODES: usize = 6000;
const MAX_DEPTH: usize = 80;
const WALK_TIME: Duration = Duration::from_secs(5);
/// AX error codes (AXError.h).
const AX_NO_VALUE: i32 = -25212;
const AX_ATTRIBUTE_UNSUPPORTED: i32 = -25205;
const AX_CANNOT_COMPLETE: i32 = -25204;
const AX_INVALID_ELEMENT: i32 = -25202;
const AX_API_DISABLED: i32 = -25211;
const AX_ACTION_UNSUPPORTED: i32 = -25206;

struct Owned(CF);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}
fn cfstr(s: &str) -> Owned {
    let c = CString::new(s).unwrap_or_default();
    Owned(unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) })
}
fn string(value: CF) -> Option<String> {
    if value.is_null() || unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
        return None;
    }
    let cap = unsafe { CFStringGetMaximumSizeForEncoding(CFStringGetLength(value), UTF8) } + 1;
    if !(1..=1 << 20).contains(&cap) {
        return None;
    }
    let mut bytes = vec![0u8; cap as usize];
    if !unsafe { CFStringGetCString(value, bytes.as_mut_ptr().cast(), cap, UTF8) } {
        return None;
    }
    let n = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8(bytes[..n].to_vec()).ok()
}
fn scalar(value: CF) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let ty = unsafe { CFGetTypeID(value) };
    if ty == unsafe { CFStringGetTypeID() } {
        return string(value);
    }
    if ty == unsafe { CFBooleanGetTypeID() } {
        return Some(if unsafe { CFBooleanGetValue(value) } { "1" } else { "0" }.into());
    }
    if ty == unsafe { CFNumberGetTypeID() } {
        let mut n = 0f64;
        if unsafe { CFNumberGetValue(value, 13 /* kCFNumberDoubleType */, (&mut n as *mut f64).cast()) } {
            return Some(if n.fract() == 0.0 && n.abs() < 1e15 { format!("{}", n as i64) } else { format!("{n}") });
        }
    }
    None
}
fn boolean(value: CF) -> Option<bool> {
    if value.is_null() || unsafe { CFGetTypeID(value) != CFBooleanGetTypeID() } {
        return None;
    }
    Some(unsafe { CFBooleanGetValue(value) })
}
fn is_ax_value(value: CF) -> bool {
    !value.is_null() && unsafe { CFGetTypeID(value) == AXValueGetTypeID() }
}
fn point(value: CF) -> Option<CGPoint> {
    let mut p = CGPoint::default();
    (is_ax_value(value) && unsafe { AXValueGetType(value) == 1 && AXValueGetValue(value, 1, (&mut p as *mut CGPoint).cast()) }).then_some(p)
}
fn size(value: CF) -> Option<CGSize> {
    let mut s = CGSize::default();
    (is_ax_value(value) && unsafe { AXValueGetType(value) == 2 && AXValueGetValue(value, 2, (&mut s as *mut CGSize).cast()) }).then_some(s)
}

/// The attributes read for each element in one message, in this order.
const ATTRS: [&str; 14] = [
    "AXRole", "AXSubrole", "AXTitle", "AXDescription", "AXValue", "AXHelp", "AXIdentifier", "AXPlaceholderValue",
    "AXEnabled", "AXFocused", "AXMenuItemMarkChar", "AXPosition", "AXSize", "AXChildren",
];

pub fn trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}
/// macOS's own Accessibility prompt, when not granted. Only from the user turning Control Apps on.
pub fn request() -> bool {
    use objc2_foundation::{NSDictionary, NSNumber, NSString};
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    }
    if trusted() {
        return true;
    }
    let key = NSString::from_str("AXTrustedCheckOptionPrompt");
    let yes = NSNumber::numberWithBool(true);
    let options = NSDictionary::from_slices(&[&*key], &[&*yes]);
    unsafe { AXIsProcessTrustedWithOptions(&*options as *const _ as *const c_void) }
}

pub struct Mac {
    /// Every element read in this call, retained; a handle is an index.
    elements: RefCell<Vec<Owned>>,
    keys: Owned,
    /// The app elements by pid, for `focused`.
    apps: RefCell<Vec<(i32, usize)>>,
    /// Handles by `CFHash`: an element read again (a second walk, the focused field) keeps its first handle, so steps
    /// of one call can tell the same element from a new one (review 10 Nov: a fresh handle per read let the focused
    /// field skip its window's check and made every menu look newly opened).
    by_hash: RefCell<std::collections::HashMap<usize, Vec<usize>>>,
}

impl Default for Mac {
    fn default() -> Self {
        Self::new()
    }
}

impl Mac {
    pub fn new() -> Self {
        let names: Vec<Owned> = ATTRS.iter().map(|a| cfstr(a)).collect();
        let raw: Vec<CF> = names.iter().map(|n| n.0).collect();
        let keys = Owned(unsafe { CFArrayCreate(std::ptr::null(), raw.as_ptr(), raw.len() as isize, &kCFTypeArrayCallBacks) });
        Mac { elements: RefCell::new(vec![]), keys, apps: RefCell::new(vec![]), by_hash: RefCell::new(Default::default()) }
    }
    /// Keeps `element` (borrowed: retained here) and returns its handle.
    fn keep(&self, element: CF) -> usize {
        let hash = unsafe { CFHash(element) };
        let mut els = self.elements.borrow_mut();
        let mut by_hash = self.by_hash.borrow_mut();
        let same = by_hash.entry(hash).or_default();
        if let Some(h) = same.iter().copied().find(|h| unsafe { CFEqual(els[*h].0, element) }) {
            return h;
        }
        // Every element answers within the call's timeout, not the system's six seconds (the timeout is per element).
        unsafe { AXUIElementSetMessagingTimeout(element, MESSAGE_TIMEOUT_S) };
        els.push(Owned(unsafe { CFRetain(element) }));
        same.push(els.len() - 1);
        els.len() - 1
    }
    fn element(&self, handle: usize) -> Result<CF> {
        self.elements.borrow().get(handle).map(|o| o.0).ok_or_else(|| anyhow::anyhow!("AX_FAILED: unknown element {handle}"))
    }
    fn app_element(&self, pid: i32) -> Result<usize> {
        if let Some((_, h)) = self.apps.borrow().iter().find(|(p, _)| *p == pid) {
            return Ok(*h);
        }
        let app = Owned(unsafe { AXUIElementCreateApplication(pid) });
        if app.0.is_null() {
            bail!("AX_FAILED: cannot reach process {pid}");
        }
        unsafe { AXUIElementSetMessagingTimeout(app.0, MESSAGE_TIMEOUT_S) };
        let h = self.keep(app.0);
        self.apps.borrow_mut().push((pid, h));
        Ok(h)
    }
    /// One element's attributes in one message; children as raw borrowed refs valid while `values` lives.
    fn read(&self, element: CF) -> Option<(Node, Owned)> {
        let mut values: CF = std::ptr::null();
        let status = unsafe { AXUIElementCopyMultipleAttributeValues(element, self.keys.0, 0, &mut values) };
        if status != 0 || values.is_null() {
            return None;
        }
        let values = Owned(values);
        let get = |i: usize| -> CF {
            if unsafe { CFArrayGetCount(values.0) } <= i as isize {
                return std::ptr::null();
            }
            let v = unsafe { CFArrayGetValueAtIndex(values.0, i as isize) };
            // A missing attribute comes back as an AXValue of the error type (kAXValueAXErrorType = 5).
            if is_ax_value(v) && unsafe { AXValueGetType(v) } == 5 { std::ptr::null() } else { v }
        };
        let s = |i: usize| scalar(get(i)).unwrap_or_default();
        let role = s(0);
        if role.is_empty() {
            return None;
        }
        let frame = match (point(get(11)), size(get(12))) {
            (Some(p), Some(z)) => Some((p.x, p.y, z.w, z.h)),
            _ => None,
        };
        let node = Node {
            handle: 0,
            role,
            subrole: s(1),
            title: s(2),
            description: s(3),
            value: s(4),
            help: s(5),
            identifier: s(6),
            placeholder: s(7),
            enabled: boolean(get(8)).unwrap_or(true),
            focused: boolean(get(9)).unwrap_or(false),
            mark: s(10),
            frame,
            children: vec![],
        };
        Some((node, values))
    }
    fn walk(&self, element: CF, depth: usize, budget: &mut (usize, Instant, bool)) -> Option<Node> {
        if depth > MAX_DEPTH || budget.0 >= MAX_NODES || Instant::now() >= budget.1 {
            budget.2 = true;
            return None;
        }
        budget.0 += 1;
        let (mut node, values) = self.read(element)?;
        node.handle = self.keep(element);
        // The menu bar is hundreds of items no request needs; open context menus are the app's own AXMenu children.
        if matches!(node.role.as_str(), "AXMenuBar" | "AXExtrasMenuBar") {
            return Some(node);
        }
        let children = unsafe { CFArrayGetValueAtIndex(values.0, 13) };
        if !children.is_null() && unsafe { CFGetTypeID(children) == CFArrayGetTypeID() } {
            for i in 0..unsafe { CFArrayGetCount(children) } {
                if budget.0 >= MAX_NODES || Instant::now() >= budget.1 {
                    budget.2 = true;
                    break;
                }
                if let Some(c) = self.walk(unsafe { CFArrayGetValueAtIndex(children, i) }, depth + 1, budget) {
                    node.children.push(c);
                }
            }
        }
        Some(node)
    }
}

impl Backend for Mac {
    fn trusted(&self) -> bool {
        trusted()
    }
    fn apps(&self) -> Vec<AppInfo> {
        objc2::rc::autoreleasepool(|_| {
            use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
            NSWorkspace::sharedWorkspace()
                .runningApplications()
                .iter()
                .filter(|a| a.activationPolicy() != NSApplicationActivationPolicy::Prohibited && !a.isTerminated())
                .filter_map(|a| {
                    Some(AppInfo {
                        name: a.localizedName()?.to_string(),
                        bundle_id: a.bundleIdentifier().map(|b| b.to_string()).unwrap_or_default(),
                        pid: a.processIdentifier(),
                        active: a.isActive(),
                    })
                })
                .collect()
        })
    }
    fn snapshot(&self, pid: i32) -> Result<Snapshot> {
        let h = self.app_element(pid)?;
        let app = self.element(h)?;
        // Chromium and Electron apps (Discord, Slack, browsers) build their web content's tree only when asked; this is
        // the switch Electron documents for assistive apps. The first read after it can be thin, so wait once.
        let manual = cfstr("AXManualAccessibility");
        let mut was: CF = std::ptr::null();
        let had = unsafe { AXUIElementCopyAttributeValue(app, manual.0, &mut was) } == 0 && boolean(was).unwrap_or(false);
        if !was.is_null() {
            unsafe { CFRelease(was) };
        }
        if !had && unsafe { AXUIElementSetAttributeValue(app, manual.0, kCFBooleanTrue) } == 0 {
            std::thread::sleep(Duration::from_millis(400));
        }
        let mut budget = (0usize, Instant::now() + WALK_TIME, false);
        match self.walk(app, 0, &mut budget) {
            Some(root) => Ok(Snapshot { root, truncated: budget.2 }),
            None => bail!("AX_FAILED: process {pid} did not answer Accessibility (it may be busy or not support it); try once more"),
        }
    }
    fn actions(&self, handle: usize) -> Vec<String> {
        let Ok(el) = self.element(handle) else { return vec![] };
        let mut names: CF = std::ptr::null();
        if unsafe { AXUIElementCopyActionNames(el, &mut names) } != 0 || names.is_null() {
            return vec![];
        }
        let names = Owned(names);
        (0..unsafe { CFArrayGetCount(names.0) }).filter_map(|i| string(unsafe { CFArrayGetValueAtIndex(names.0, i) })).collect()
    }
    fn perform(&self, handle: usize, action: &str) -> Result<()> {
        let el = self.element(handle)?;
        let a = cfstr(action);
        match unsafe { AXUIElementPerformAction(el, a.0) } {
            0 => Ok(()),
            AX_CANNOT_COMPLETE => bail!("AX_CANNOT_COMPLETE: the app did not confirm {action} in time"),
            AX_INVALID_ELEMENT => bail!("AX_GONE: the element went away before {action}; call find again"),
            AX_ACTION_UNSUPPORTED => bail!("AX_FAILED: the element does not support {action}"),
            AX_API_DISABLED => bail!("ACCESSIBILITY_MISSING: macOS refused Accessibility to Heimdall"),
            e => bail!("AX_FAILED: {action} failed (AX error {e})"),
        }
    }
    fn set_text(&self, handle: usize, attribute: &str, text: &str) -> Result<()> {
        let el = self.element(handle)?;
        let (k, v) = (cfstr(attribute), cfstr(text));
        match unsafe { AXUIElementSetAttributeValue(el, k.0, v.0) } {
            0 => Ok(()),
            AX_ATTRIBUTE_UNSUPPORTED | AX_NO_VALUE => bail!("AX_FAILED: the element does not take {attribute}"),
            AX_INVALID_ELEMENT => bail!("AX_GONE: the element went away; call find again"),
            e => bail!("AX_FAILED: setting {attribute} failed (AX error {e})"),
        }
    }
    fn focus(&self, handle: usize) -> Result<()> {
        let el = self.element(handle)?;
        let k = cfstr("AXFocused");
        match unsafe { AXUIElementSetAttributeValue(el, k.0, kCFBooleanTrue) } {
            0 => Ok(()),
            e => bail!("AX_FAILED: could not focus the field (AX error {e})"),
        }
    }
    fn reread(&self, handle: usize) -> Option<Node> {
        let el = self.element(handle).ok()?;
        let (mut node, _) = self.read(el)?;
        node.handle = handle;
        Some(node)
    }
    fn focused(&self, pid: i32) -> Option<Node> {
        let app = self.element(self.app_element(pid).ok()?).ok()?;
        let k = cfstr("AXFocusedUIElement");
        let mut v: CF = std::ptr::null();
        if unsafe { AXUIElementCopyAttributeValue(app, k.0, &mut v) } != 0 || v.is_null() {
            return None;
        }
        let v = Owned(v);
        let (mut node, _) = self.read(v.0)?;
        node.handle = self.keep(v.0);
        Some(node)
    }
    fn idle_seconds(&self) -> f64 {
        // kCGEventSourceStateHIDSystemState, kCGAnyInputEventType: the user's own hardware input only.
        unsafe { CGEventSourceSecondsSinceLastEventType(1, u32::MAX) }
    }
    fn click(&self, pid: i32, x: f64, y: f64, right: bool) -> Result<()> {
        // Left: down 1, up 2; right: down 3, up 4. Posted to the process, never to the HID system: the pointer stays.
        let (down, up, button) = if right { (3, 4, 1) } else { (1, 2, 0) };
        for kind in [down, up] {
            let ev = Owned(unsafe { CGEventCreateMouseEvent(std::ptr::null(), kind, CGPoint { x, y }, button) });
            if ev.0.is_null() {
                bail!("AX_FAILED: could not make the click event");
            }
            unsafe { CGEventPostToPid(pid, ev.0) };
            std::thread::sleep(Duration::from_millis(30));
        }
        Ok(())
    }
    fn settle(&self, millis: u64) {
        std::thread::sleep(Duration::from_millis(millis));
    }
}
