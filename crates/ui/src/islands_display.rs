//! Settings `islands_display` (bug note 2026-11-07T02-02-35, the user: "sometimes I like them followed, sometimes I like
//! to lock them"). Follow Cursor, the default and the behaviour he loves, puts the notch island and the floating
//! islands on the display with the pointer. Locked, they stay on one display, named as macOS names it ("MSI MAG
//! 271QPX"). A locked display that is not connected falls back to following, and the lock is kept for when it is
//! back. The Floating Islands helper reads the same name from its `display.json` (`islands::model::DisplayLock`) and
//! matches it the same way.

use std::sync::Mutex;

/// The word that means "follow the pointer" in the settings tool and the menu.
pub const FOLLOW: &str = "follow";

/// The menu's row for following.
pub const FOLLOW_TITLE: &str = "Follow Cursor";

/// Lower case, letters and digits only: "MSI MAG 271QPX" and "msi mag 271qpx" are one name.
pub fn fold(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Whether a value means Follow Cursor rather than a display ("follow", "follow cursor", "cursor", "pointer", "none",
/// "unlock", or nothing).
pub fn is_follow(v: &str) -> bool {
    matches!(fold(v).as_str(), "" | "follow" | "followcursor" | "followpointer" | "followme" | "cursor" | "pointer" | "none" | "unlock" | "unlocked" | "off")
}

/// The display a name picks among `names` (macOS's names of the connected displays): the same name first (case and
/// punctuation aside), else the one display whose name contains it ("MSI" finds "MSI MAG 271QPX"). Several or none:
/// `None`.
pub fn find(names: &[&str], wanted: &str) -> Option<usize> {
    let want = fold(wanted);
    if want.is_empty() {
        return None;
    }
    if let Some(i) = names.iter().position(|n| fold(n) == want) {
        return Some(i);
    }
    let partial: Vec<usize> = (0..names.len()).filter(|i| fold(names[*i]).contains(&want)).collect();
    match partial.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

static CONNECTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The connected displays' names as macOS gives them, as the UI last read them (at launch and at each display
/// change); empty before the UI runs (tests, replays).
pub fn connected() -> Vec<String> {
    CONNECTED.lock().map(|c| c.clone()).unwrap_or_default()
}

pub fn set_connected(names: Vec<String>) {
    if let Ok(mut c) = CONNECTED.lock() {
        *c = names;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_display_is_found_by_its_name_or_a_part_of_it() {
        let names = ["Built-in Retina Display", "MSI MAG 271QPX"];
        assert_eq!(find(&names, "MSI MAG 271QPX"), Some(1));
        assert_eq!(find(&names, "msi mag 271qpx"), Some(1), "case aside");
        assert_eq!(find(&names, "MSI"), Some(1), "a part names the one display that has it");
        assert_eq!(find(&names, "built-in"), Some(0));
        assert_eq!(find(&names, "LG UltraFine"), None, "not connected: the islands follow the pointer");
        assert_eq!(find(&names, ""), None);
        assert_eq!(find(&["DELL U2723QE", "DELL P2422H"], "dell"), None, "two match: neither");
        assert_eq!(find(&["DELL U2723QE", "DELL P2422H"], "dell p2422h"), Some(1));
    }

    #[test]
    fn follow_has_its_words() {
        for v in ["follow", "Follow Cursor", "cursor", "", "none", "unlock"] {
            assert!(is_follow(v), "{v}");
        }
        assert!(!is_follow("MSI"));
    }
}
