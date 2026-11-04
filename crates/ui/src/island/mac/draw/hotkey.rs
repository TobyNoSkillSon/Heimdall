//! Heimdall's global keys through Carbon's RegisterEventHotKey (the table is `crate::shortcuts`; `mac/shortcuts.rs`
//! decides what each does). No Accessibility or Input Monitoring permission, no event tap, nothing running between
//! presses. The handler runs on the main thread (AppKit's run loop dispatches Carbon events).
//!
//! A registered key never reaches any app as a key press, Heimdall's own open menu included, and ⌃⌘M did nothing
//! while Heimdall's menu was open (bug note 2026-10-23T22-12-34-2; the 22:11 frame shows the menu open). So
//! `suspend` takes every key back while the menu is open, where the Shortcuts items' key equivalents answer the same
//! keys, and `resume` registers them again when it closes.

use std::cell::RefCell;
use std::ffi::c_void;

use objc2_foundation::MainThreadMarker;

#[repr(C)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}

#[repr(C)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(target: *mut c_void, handler: Handler, count: usize, types: *const EventTypeSpec, user: *mut c_void, out: *mut *mut c_void) -> i32;
    fn RegisterEventHotKey(code: u32, modifiers: u32, id: EventHotKeyID, target: *mut c_void, options: u32, out: *mut *mut c_void) -> i32;
    fn UnregisterEventHotKey(hotkey: *mut c_void) -> i32;
    fn GetEventParameter(event: *mut c_void, name: u32, kind: u32, out_kind: *mut u32, size: usize, out_size: *mut usize, out: *mut c_void) -> i32;
}

const CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const HOT_KEY_PRESSED: u32 = 5;
const SIGNATURE: u32 = u32::from_be_bytes(*b"HDrw");
/// kEventParamDirectObject, typeEventHotKeyID
const PARAM_DIRECT: u32 = u32::from_be_bytes(*b"----");
const TYPE_HOT_KEY_ID: u32 = u32::from_be_bytes(*b"hkid");
/// eventNotHandledErr: a hot key that is not Heimdall's goes on to the next handler.
const NOT_HANDLED: i32 = -9874;

struct Key {
    id: u32,
    code: u32,
    modifiers: u32,
    /// Registered now (null while suspended or after a failure).
    hotkey: *mut c_void,
}

#[derive(Default)]
struct Keys {
    handler: bool,
    keys: Vec<Key>,
    action: Option<fn(u32, MainThreadMarker)>,
    suspended: bool,
}

thread_local! {
    static KEYS: RefCell<Keys> = RefCell::new(Keys::default());
}

extern "C" fn pressed(_call: *mut c_void, event: *mut c_void, _user: *mut c_void) -> i32 {
    let mut id = EventHotKeyID { signature: 0, id: 0 };
    // SAFETY: reads the pressed key's id out of the Carbon event into a struct of the documented layout.
    let status = unsafe {
        GetEventParameter(event, PARAM_DIRECT, TYPE_HOT_KEY_ID, std::ptr::null_mut(), std::mem::size_of::<EventHotKeyID>(), std::ptr::null_mut(), (&mut id as *mut EventHotKeyID).cast())
    };
    if status != 0 || id.signature != SIGNATURE {
        return NOT_HANDLED;
    }
    let Some(mtm) = MainThreadMarker::new() else { return NOT_HANDLED };
    // The action runs after the borrow ends (it may suspend or resume the keys).
    let action = KEYS.with(|k| k.try_borrow().ok().and_then(|k| k.action));
    if let Some(action) = action {
        action(id.id, mtm);
    }
    0
}

/// Where presses go: `action(id)` with the id the key was registered under.
pub(crate) fn set_action(action: fn(u32, MainThreadMarker)) {
    KEYS.with(|k| k.borrow_mut().action = Some(action));
}

fn install_handler(k: &mut Keys) -> Result<(), String> {
    if k.handler {
        return Ok(());
    }
    let spec = EventTypeSpec { class: CLASS_KEYBOARD, kind: HOT_KEY_PRESSED };
    let mut handler = std::ptr::null_mut();
    // SAFETY: installs a plain C callback on the application target for one event type.
    let status = unsafe { InstallEventHandler(GetApplicationEventTarget(), pressed, 1, &spec, std::ptr::null_mut(), &mut handler) };
    if status != 0 {
        return Err(format!("InstallEventHandler failed ({status})"));
    }
    k.handler = true;
    Ok(())
}

fn register_key(key: &mut Key) -> i32 {
    let mut hotkey = std::ptr::null_mut();
    // SAFETY: Carbon copies the id; `hotkey` receives the reference we unregister with.
    let status = unsafe { RegisterEventHotKey(key.code, key.modifiers, EventHotKeyID { signature: SIGNATURE, id: key.id }, GetApplicationEventTarget(), 0, &mut hotkey) };
    key.hotkey = if status == 0 { hotkey } else { std::ptr::null_mut() };
    status
}

/// Registers a key under `id` (Carbon's virtual key code and modifier mask). Fails when the combination is taken
/// (another app registered it first); the key is kept, so `resume` tries it again.
pub(crate) fn register(id: u32, code: u32, modifiers: u32, _mtm: MainThreadMarker) -> Result<(), String> {
    KEYS.with(|k| {
        let mut k = k.borrow_mut();
        install_handler(&mut k)?;
        k.keys.retain(|key| key.id != id || !key.hotkey.is_null());
        if k.keys.iter().any(|key| key.id == id) {
            return Ok(());
        }
        let mut key = Key { id, code, modifiers, hotkey: std::ptr::null_mut() };
        let status = if k.suspended { 0 } else { register_key(&mut key) };
        k.keys.push(key);
        if status != 0 {
            return Err(format!("RegisterEventHotKey failed ({status})"));
        }
        Ok(())
    })
}

/// Every key unregistered until `resume` (Heimdall's menu is open: its items answer the keys).
pub(crate) fn suspend() {
    KEYS.with(|k| {
        let Ok(mut k) = k.try_borrow_mut() else { return };
        k.suspended = true;
        for key in &mut k.keys {
            if !key.hotkey.is_null() {
                // SAFETY: a reference RegisterEventHotKey returned, unregistered once.
                unsafe { UnregisterEventHotKey(key.hotkey) };
                key.hotkey = std::ptr::null_mut();
            }
        }
    });
}

/// The key under `id` released for good (a question card's digit, once the card no longer answers it).
pub(crate) fn unregister(id: u32) {
    KEYS.with(|k| {
        let Ok(mut k) = k.try_borrow_mut() else { return };
        k.keys.retain(|key| {
            if key.id != id {
                return true;
            }
            if !key.hotkey.is_null() {
                // SAFETY: a reference RegisterEventHotKey returned, unregistered once (it leaves `keys` here).
                unsafe { UnregisterEventHotKey(key.hotkey) };
            }
            false
        });
    });
}

/// Every key registered again; returns the ids that could not be (taken by another app).
pub(crate) fn resume() -> Vec<u32> {
    KEYS.with(|k| {
        let Ok(mut k) = k.try_borrow_mut() else { return vec![] };
        k.suspended = false;
        k.keys.iter_mut().filter(|key| key.hotkey.is_null()).filter_map(|key| (register_key(key) != 0).then_some(key.id)).collect()
    })
}

/// The ids registered now (checks).
pub(crate) fn registered() -> Vec<u32> {
    KEYS.with(|k| k.borrow().keys.iter().filter(|key| !key.hotkey.is_null()).map(|key| key.id).collect())
}
