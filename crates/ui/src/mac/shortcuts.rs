//! The global shortcuts' AppKit side (the table is `crate::shortcuts`): registered through Carbon at launch
//! (`draw::hotkey`), each running the same code path as its menu control, taken back while Heimdall's menu is open
//! (its Shortcuts items answer the same keys there), and checked against macOS's own shortcuts.
//!
//! - ⌃⌘. Listening and ⌃⌘/ Seeing: the menu tiles' `toggle_tile`.
//! - ⌃⌘M drawing mode: `overlay::toggle`.
//! - ⌃⌘, every floating island: Show Islands' `toggle_islands_shown`.
//! - ⌃⌘H the conversation pulled down with its text field focused (`island::open_chat`); again, with the field still
//!   focused, pulls it up.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_foundation::MainThreadMarker;

use super::draw::{hotkey, overlay};
use super::{island, with_state};
use crate::shortcuts::{defaults, system_clashes, Shortcut, SystemHotkey};
use crate::Tile;

thread_local! {
    /// Why a shortcut does not work from other apps, for the menu ("Taken by another app").
    static UNAVAILABLE: RefCell<Vec<(Shortcut, &'static str)>> = const { RefCell::new(Vec::new()) };
}

pub(crate) const TAKEN: &str = "Taken by another app";
pub(crate) const SYSTEM: &str = "Used by a macOS shortcut";

/// Registers every shortcut (the user's own Heimdall only: never in renders or a lab instance). A key another app holds
/// or a macOS shortcut also uses is said once at the notch ("⌃⌘M unavailable") and marked in the menu.
pub(crate) fn install(mtm: MainThreadMarker) {
    hotkey::set_action(pressed);
    let clashes = system_clashes(&defaults(), &system_hotkeys());
    for s in Shortcut::ALL {
        let keys = s.keys();
        let why = match hotkey::register(s.id(), keys.code as u32, keys.carbon_modifiers(), mtm) {
            Err(e) => {
                eprintln!("Shortcuts: {} ({}): {e}", keys.label(), s.title());
                Some(TAKEN)
            }
            Ok(()) => clashes.iter().find(|(c, _)| *c == s).map(|(_, id)| {
                eprintln!("Shortcuts: {} ({}) is also macOS shortcut {id}", keys.label(), s.title());
                SYSTEM
            }),
        };
        if let Some(why) = why {
            UNAVAILABLE.with(|u| u.borrow_mut().push((s, why)));
            island::apply(crate::island::object::Event::Health(crate::island::HealthCue::Notice { part: part(s), label: format!("{} unavailable", keys.label()) }));
        }
    }
}

fn part(s: Shortcut) -> crate::island::Part {
    match s {
        Shortcut::Listening => crate::island::Part::Microphone,
        Shortcut::Seeing | Shortcut::Drawing => crate::island::Part::Screen,
        Shortcut::Islands | Shortcut::Chat | Shortcut::Wake | Shortcut::Suggest => crate::island::Part::Heimdall,
    }
}

fn pressed(id: u32, mtm: MainThreadMarker) {
    if let Some(s) = Shortcut::from_id(id) {
        run(s, mtm);
    } else {
        // Option plus a digit answers the question card on screen (`island::digit_pressed`).
        island::digit_pressed(id);
    }
}

/// What a shortcut does, from the key or its menu item.
pub(crate) fn run(s: Shortcut, mtm: MainThreadMarker) {
    match s {
        Shortcut::Chat => island::open_chat(),
        Shortcut::Drawing => overlay::toggle(mtm),
        Shortcut::Islands => with_state(|st| st.toggle_islands_shown()),
        Shortcut::Listening => with_state(|st| st.toggle_tile(Tile::Listening)),
        Shortcut::Seeing => with_state(|st| st.toggle_tile(Tile::Seeing)),
        Shortcut::Wake => with_state(|st| st.emit(crate::UiAction::ForceWake)),
        Shortcut::Suggest => with_state(|st| st.emit(crate::UiAction::ForceSuggest)),
    }
}

/// Heimdall's menu opened: the keys go to it (its Shortcuts items), not to Carbon.
pub(crate) fn menu_opened() {
    hotkey::suspend();
}

/// The menu closed: the keys are global again. A key taken meanwhile is marked in the menu. One that was taken and
/// now registers (the other app let go) loses its mark. Only changes are logged.
pub(crate) fn menu_closed() {
    let failed = hotkey::resume();
    let registered = hotkey::registered();
    UNAVAILABLE.with(|u| {
        let mut u = u.borrow_mut();
        u.retain(|(s, why)| *why != TAKEN || !registered.contains(&s.id()));
        for s in failed.iter().filter_map(|id| Shortcut::from_id(*id)) {
            if !u.iter().any(|(x, _)| *x == s) {
                eprintln!("Shortcuts: {} ({}) taken by another app", s.keys().label(), s.title());
                u.push((s, TAKEN));
            }
        }
    });
}

/// Why `s` does not work from other apps, if it does not.
pub(crate) fn unavailable(s: Shortcut) -> Option<&'static str> {
    UNAVAILABLE.with(|u| u.borrow().iter().find(|(x, _)| *x == s).map(|(_, why)| *why))
}

/// macOS's own shortcuts as the user's `com.apple.symbolichotkeys` lists them (read only; entries it does not list keep
/// their defaults). The domain's dictionary goes through JSON, which holds every value it stores.
pub(crate) fn system_hotkeys() -> Vec<SystemHotkey> {
    let Some(json) = symbolic_hotkeys_json() else { return vec![] };
    parse_symbolic_hotkeys(&json)
}

fn symbolic_hotkeys_json() -> Option<Vec<u8>> {
    let defaults_class = AnyClass::get(c"NSUserDefaults")?;
    let json_class = AnyClass::get(c"NSJSONSerialization")?;
    unsafe {
        let defaults: Retained<AnyObject> = msg_send![defaults_class, standardUserDefaults];
        let name = objc2_foundation::NSString::from_str("com.apple.symbolichotkeys");
        let domain: Option<Retained<AnyObject>> = msg_send![&*defaults, persistentDomainForName: &*name];
        let domain = domain?;
        let valid: bool = msg_send![json_class, isValidJSONObject: &*domain];
        if !valid {
            return None;
        }
        let data: Option<Retained<objc2_foundation::NSData>> = msg_send![json_class, dataWithJSONObject: &*domain, options: 0usize, error: std::ptr::null_mut::<*mut AnyObject>()];
        Some(data?.to_vec())
    }
}

/// `{"AppleSymbolicHotKeys": {"60": {"enabled": 1, "value": {"parameters": [32, 49, 262144]}}}}` → the entries
/// with keys (parameters are the character, the key code and the modifier flags).
pub(crate) fn parse_symbolic_hotkeys(json: &[u8]) -> Vec<SystemHotkey> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(json) else { return vec![] };
    let Some(map) = v.get("AppleSymbolicHotKeys").and_then(|m| m.as_object()) else { return vec![] };
    let mut out: Vec<SystemHotkey> = map
        .iter()
        .filter_map(|(id, e)| {
            let p = e.get("value")?.get("parameters")?.as_array()?;
            let code = p.get(1)?.as_u64()?;
            let modifiers = p.get(2)?.as_u64()?;
            let enabled = e.get("enabled").is_some_and(|x| x.as_bool().unwrap_or(false) || x.as_u64() == Some(1));
            Some(SystemHotkey { id: id.parse().ok()?, enabled, code: u16::try_from(code).ok()?, modifiers })
        })
        .collect();
    out.sort_by_key(|h| h.id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owners_symbolic_hotkeys_parse_and_none_clashes() {
        // The user's file on 24 Oct (`defaults read com.apple.symbolichotkeys`), as JSON.
        let json = br#"{"AppleSymbolicHotKeys":{"15":{"enabled":0},"60":{"enabled":1,"value":{"parameters":[32,49,262144],"type":"standard"}},"61":{"enabled":1,"value":{"parameters":[32,49,786432],"type":"standard"}},"164":{"enabled":0,"value":{"parameters":[65535,65535,0],"type":"standard"}},"79":{"enabled":1}}}"#;
        let hotkeys = parse_symbolic_hotkeys(json);
        assert_eq!(
            hotkeys,
            vec![
                SystemHotkey { id: 60, enabled: true, code: 49, modifiers: 262144 },
                SystemHotkey { id: 61, enabled: true, code: 49, modifiers: 786432 },
                SystemHotkey { id: 164, enabled: false, code: 65535, modifiers: 0 },
            ]
        );
        assert!(system_clashes(&defaults(), &hotkeys).is_empty());
        assert!(parse_symbolic_hotkeys(b"not json").is_empty());
        assert!(parse_symbolic_hotkeys(br#"{"other":1}"#).is_empty());
    }
}
