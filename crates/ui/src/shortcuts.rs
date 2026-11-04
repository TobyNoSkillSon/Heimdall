//! Heimdall's global shortcuts ("we should also have keybinds in general"): one table, used by
//! the Carbon registration (`mac/shortcuts.rs`), the menu's Shortcuts submenu (native key equivalents) and the
//! conflict checks. Platform-neutral, so the table and its checks are tested without AppKit.
//!
//! The keys are fixed for now; the menu shows them read-only. ⌃⌘\ and ⌃⌘' are the user's live labels for the decider
//! (29 Oct): wake now, suggest now (`crates/app/src/pipeline/hotkeys.rs`).

/// One global shortcut's action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Shortcut {
    /// The conversation pulled down from the notch with its text field focused, to type at once (⌃⌘H).
    Chat,
    /// Drawing mode on or off (⌃⌘M).
    Drawing,
    /// Every floating island shown or hidden (⌃⌘,).
    Islands,
    /// Listening on or off: the microphone and the recorded apps (⌃⌘.).
    Listening,
    /// Seeing on or off: the screen recording (⌃⌘/).
    Seeing,
    /// Wake Heimdall now with what the user said since the last wake, and label that line "send" for the decider's training
    /// (⌃⌘\; "I decide for the decider, and that goes to the training data").
    Wake,
    /// A suggestion card now from the current conversation, and label the line "suggest" for training (⌃⌘', the key
    /// left of backslash).
    Suggest,
}

/// A key and its modifiers: the virtual key code (Carbon's `kVK_*`, layout-independent), the character AppKit shows
/// as the menu item's key equivalent, and the modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Keys {
    pub code: u16,
    pub key: char,
    pub control: bool,
    pub option: bool,
    pub shift: bool,
    pub command: bool,
}

/// kVK_ANSI_M, kVK_ANSI_Comma, kVK_ANSI_Period, kVK_ANSI_Slash, kVK_ANSI_H.
pub const KEY_M: u16 = 46;
pub const KEY_COMMA: u16 = 43;
pub const KEY_PERIOD: u16 = 47;
pub const KEY_SLASH: u16 = 44;
/// kVK_ANSI_Backslash and kVK_ANSI_Quote: physical keys, whatever the layout says they type.
pub const KEY_BACKSLASH: u16 = 42;
pub const KEY_QUOTE: u16 = 39;
pub const KEY_H: u16 = 4;

const fn control_command(code: u16, key: char) -> Keys {
    Keys { code, key, control: true, option: false, shift: false, command: true }
}

impl Shortcut {
    /// In the menu's order.
    pub const ALL: [Shortcut; 7] = [Shortcut::Chat, Shortcut::Wake, Shortcut::Suggest, Shortcut::Listening, Shortcut::Seeing, Shortcut::Drawing, Shortcut::Islands];

    pub fn keys(self) -> Keys {
        match self {
            Shortcut::Chat => control_command(KEY_H, 'h'),
            Shortcut::Drawing => control_command(KEY_M, 'm'),
            Shortcut::Islands => control_command(KEY_COMMA, ','),
            Shortcut::Listening => control_command(KEY_PERIOD, '.'),
            Shortcut::Seeing => control_command(KEY_SLASH, '/'),
            Shortcut::Wake => control_command(KEY_BACKSLASH, '\\'),
            Shortcut::Suggest => control_command(KEY_QUOTE, '\''),
        }
    }

    /// The Shortcuts submenu's row.
    pub fn title(self) -> &'static str {
        match self {
            Shortcut::Chat => "Message Heimdall",
            Shortcut::Drawing => "Drawing Mode",
            Shortcut::Islands => "Show Islands",
            Shortcut::Listening => "Listening",
            Shortcut::Seeing => "Seeing",
            Shortcut::Wake => "Wake Heimdall",
            Shortcut::Suggest => "Suggest Now",
        }
    }

    /// The row's SF Symbol (the notch shows the same glyphs for Listening, Seeing and Islands).
    pub fn symbol(self) -> &'static str {
        match self {
            Shortcut::Chat => "text.bubble",
            Shortcut::Drawing => "scribble.variable",
            Shortcut::Islands => "rectangle.on.rectangle",
            Shortcut::Listening => "mic",
            Shortcut::Seeing => "eye",
            Shortcut::Wake => "waveform",
            Shortcut::Suggest => "lightbulb",
        }
    }

    /// The row's symbol for its state: slashed while off, as the notch shows it (drawing mode and the chat have no
    /// slashed twin).
    pub fn symbol_for(self, on: bool) -> String {
        if on || matches!(self, Shortcut::Drawing | Shortcut::Chat | Shortcut::Wake | Shortcut::Suggest) { self.symbol().into() } else { format!("{}.slash", self.symbol()) }
    }

    /// Its Carbon hotkey id (1-based, stable across launches).
    pub fn id(self) -> u32 {
        match self {
            Shortcut::Listening => 1,
            Shortcut::Seeing => 2,
            Shortcut::Drawing => 3,
            Shortcut::Islands => 4,
            Shortcut::Chat => 5,
            Shortcut::Wake => 6,
            Shortcut::Suggest => 7,
        }
    }

    pub fn from_id(id: u32) -> Option<Shortcut> {
        Shortcut::ALL.into_iter().find(|s| s.id() == id)
    }
}

impl Keys {
    /// As macOS writes it: ⌃⌥⇧⌘ in that order, then the key ("⌃⌘M").
    pub fn label(self) -> String {
        let mut s = String::new();
        for (on, glyph) in [(self.control, '⌃'), (self.option, '⌥'), (self.shift, '⇧'), (self.command, '⌘')] {
            if on {
                s.push(glyph);
            }
        }
        s.extend(self.key.to_uppercase());
        s
    }

    /// Carbon's modifier mask (`cmdKey`, `shiftKey`, `optionKey`, `controlKey`) for RegisterEventHotKey.
    pub fn carbon_modifiers(self) -> u32 {
        (self.command as u32) << 8 | (self.shift as u32) << 9 | (self.option as u32) << 11 | (self.control as u32) << 12
    }

    /// AppKit's modifier flags (`NSEventModifierFlags`, as `com.apple.symbolichotkeys` stores them too).
    pub fn appkit_modifiers(self) -> u64 {
        (self.shift as u64) << 17 | (self.control as u64) << 18 | (self.option as u64) << 19 | (self.command as u64) << 20
    }
}

/// Pairs in `table` that share keys: each would steal the other's press.
pub fn duplicates(table: &[(Shortcut, Keys)]) -> Vec<(Shortcut, Shortcut)> {
    let mut out = vec![];
    for (i, (a, ka)) in table.iter().enumerate() {
        for (b, kb) in &table[i + 1..] {
            if ka.code == kb.code && ka.appkit_modifiers() == kb.appkit_modifiers() {
                out.push((*a, *b));
            }
        }
    }
    out
}

/// One of macOS's own shortcuts (System Settings ▸ Keyboard ▸ Keyboard Shortcuts) as `com.apple.symbolichotkeys`
/// stores it under `AppleSymbolicHotKeys`: its number, whether it is on, and its key code and AppKit modifier flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemHotkey {
    pub id: u32,
    pub enabled: bool,
    pub code: u16,
    pub modifiers: u64,
}

/// The device-independent modifier bits (⇧⌃⌥⌘); the rest (fn, caps lock, device bits) never decide a clash.
const MODIFIER_BITS: u64 = 0b1111 << 17;

/// Heimdall's shortcuts that one of macOS's own enabled shortcuts also uses: macOS takes those presses first.
/// Entries the file does not list keep macOS's defaults; none of the known defaults uses ⌃⌘ with H M , . or /
/// (Apple's Mac keyboard shortcuts list, support.apple.com/102650, checked 26 Oct: ⌃⌘ goes with Space F Q D N T A
/// and Up Arrow; ⌘H hides, ⌥⌘H hides others, ⇧⌘H opens Home), so in practice only entries the user changed can clash.
pub fn system_clashes(table: &[(Shortcut, Keys)], system: &[SystemHotkey]) -> Vec<(Shortcut, u32)> {
    let mut out = vec![];
    for (s, k) in table {
        for h in system {
            if h.enabled && h.code == k.code && h.modifiers & MODIFIER_BITS == k.appkit_modifiers() {
                out.push((*s, h.id));
            }
        }
    }
    out
}

/// The default table.
pub fn defaults() -> Vec<(Shortcut, Keys)> {
    Shortcut::ALL.iter().map(|s| (*s, s.keys())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seven_shortcuts_are_owners_and_each_is_distinct() {
        let labels: Vec<(Shortcut, String)> = Shortcut::ALL.iter().map(|s| (*s, s.keys().label())).collect();
        assert_eq!(
            labels,
            vec![(Shortcut::Chat, "⌃⌘H".into()), (Shortcut::Wake, "⌃⌘\\".into()), (Shortcut::Suggest, "⌃⌘'".into()), (Shortcut::Listening, "⌃⌘.".into()),
                (Shortcut::Seeing, "⌃⌘/".into()), (Shortcut::Drawing, "⌃⌘M".into()), (Shortcut::Islands, "⌃⌘,".into())]
        );
        // Physical keys (Carbon's ANSI codes): the same two keys on a Polish or any other layout.
        assert_eq!((Shortcut::Wake.keys().code, Shortcut::Suggest.keys().code), (42, 39));
        assert!(duplicates(&defaults()).is_empty());
        let mut ids: Vec<u32> = Shortcut::ALL.iter().map(|s| s.id()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 7, "ids are distinct");
        assert!(Shortcut::ALL.iter().all(|s| Shortcut::from_id(s.id()) == Some(*s)));
        assert_eq!(Shortcut::from_id(0), None);
    }

    #[test]
    fn modifier_masks_match_carbon_and_appkit() {
        let k = Shortcut::Drawing.keys();
        // controlKey | cmdKey
        assert_eq!(k.carbon_modifiers(), 0x1000 | 0x100);
        // NSEventModifierFlagControl | NSEventModifierFlagCommand
        assert_eq!(k.appkit_modifiers(), 0x40000 | 0x100000);
        let all = Keys { code: 0, key: 'a', control: true, option: true, shift: true, command: true };
        assert_eq!(all.label(), "⌃⌥⇧⌘A");
        assert_eq!(all.carbon_modifiers(), 0x1000 | 0x800 | 0x200 | 0x100);
    }

    #[test]
    fn a_shared_key_is_a_duplicate_and_other_modifiers_are_not() {
        let mut table = defaults();
        table.push((Shortcut::Seeing, Shortcut::Drawing.keys()));
        assert_eq!(duplicates(&table), vec![(Shortcut::Drawing, Shortcut::Seeing)]);
        let mut table = defaults();
        table.push((Shortcut::Seeing, Keys { shift: true, ..Shortcut::Drawing.keys() }));
        assert!(duplicates(&table).is_empty());
    }

    #[test]
    fn only_an_enabled_system_shortcut_on_the_same_keys_clashes() {
        // The user's file on 24 Oct: the input-source switches on ⌃Space and ⌃⌥Space, nothing on ⌃⌘.
        let owner = [
            SystemHotkey { id: 60, enabled: true, code: 49, modifiers: 262144 },
            SystemHotkey { id: 61, enabled: true, code: 49, modifiers: 786432 },
            SystemHotkey { id: 164, enabled: false, code: 65535, modifiers: 0 },
        ];
        assert!(system_clashes(&defaults(), &owner).is_empty());
        // A screenshot shortcut moved onto ⌃⌘M (with the fn bit macOS adds for some keys) clashes; off, it does not.
        let moved = SystemHotkey { id: 28, enabled: true, code: KEY_M, modifiers: 0x40000 | 0x100000 | 0x800000 };
        assert_eq!(system_clashes(&defaults(), &[moved]), vec![(Shortcut::Drawing, 28)]);
        assert!(system_clashes(&defaults(), &[SystemHotkey { enabled: false, ..moved }]).is_empty());
        // ⇧⌃⌘M is not ⌃⌘M.
        assert!(system_clashes(&defaults(), &[SystemHotkey { modifiers: 0x20000 | 0x40000 | 0x100000, ..moved }]).is_empty());
    }
}
