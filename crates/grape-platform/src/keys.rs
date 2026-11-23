//! Keys and chords for the Shortcuts wheel
//! (`docs/design/m10-shortcuts-wheel.md` §3).
//!
//! A [`KeyCode`] is a place on the keyboard, named by its W3C
//! `KeyboardEvent.code`. A chord the user sets is replayed from the same
//! place, so it is right on every layout; only the labels assume the US
//! layout.
//!
//! Pure: no OS calls, so it builds and is tested on every OS.

use crate::Modifiers;

/// Writes [`KeyCode`], its list, its ids and its labels from one table, so
/// none of them can miss a key.
macro_rules! key_codes {
    ($($name:ident => $label:literal,)+) => {
        /// A key that isn't a modifier, named by its W3C
        /// `KeyboardEvent.code` (a place on the keyboard).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum KeyCode {
            $(
                #[doc = concat!("The `", stringify!($name), "` key.")]
                $name,
            )+
        }

        impl KeyCode {
            /// Every key, in the order Settings lists them.
            pub const ALL: &'static [KeyCode] = &[$(KeyCode::$name,)+];

            /// The W3C code: `"KeyA"`, `"Digit4"`, `"F5"`, `"ArrowUp"`.
            pub fn id(self) -> &'static str {
                match self {
                    $(KeyCode::$name => stringify!($name),)+
                }
            }

            /// What the key is called on a US Mac keyboard: `"A"`, `"4"`,
            /// `"F5"`, `"↑"`, `"Space"`, `"Return"`. Backspace is
            /// `"Delete"` there, and Delete is `"Forward Delete"`.
            pub fn label(self) -> &'static str {
                match self {
                    $(KeyCode::$name => $label,)+
                }
            }
        }
    };
}

key_codes! {
    KeyA => "A",
    KeyB => "B",
    KeyC => "C",
    KeyD => "D",
    KeyE => "E",
    KeyF => "F",
    KeyG => "G",
    KeyH => "H",
    KeyI => "I",
    KeyJ => "J",
    KeyK => "K",
    KeyL => "L",
    KeyM => "M",
    KeyN => "N",
    KeyO => "O",
    KeyP => "P",
    KeyQ => "Q",
    KeyR => "R",
    KeyS => "S",
    KeyT => "T",
    KeyU => "U",
    KeyV => "V",
    KeyW => "W",
    KeyX => "X",
    KeyY => "Y",
    KeyZ => "Z",
    Digit0 => "0",
    Digit1 => "1",
    Digit2 => "2",
    Digit3 => "3",
    Digit4 => "4",
    Digit5 => "5",
    Digit6 => "6",
    Digit7 => "7",
    Digit8 => "8",
    Digit9 => "9",
    F1 => "F1",
    F2 => "F2",
    F3 => "F3",
    F4 => "F4",
    F5 => "F5",
    F6 => "F6",
    F7 => "F7",
    F8 => "F8",
    F9 => "F9",
    F10 => "F10",
    F11 => "F11",
    F12 => "F12",
    F13 => "F13",
    F14 => "F14",
    F15 => "F15",
    F16 => "F16",
    F17 => "F17",
    F18 => "F18",
    F19 => "F19",
    F20 => "F20",
    Space => "Space",
    Enter => "Return",
    Tab => "Tab",
    Escape => "Esc",
    Backspace => "Delete",
    Delete => "Forward Delete",
    ArrowUp => "↑",
    ArrowDown => "↓",
    ArrowLeft => "←",
    ArrowRight => "→",
    Home => "Home",
    End => "End",
    PageUp => "Page Up",
    PageDown => "Page Down",
    Minus => "-",
    Equal => "=",
    BracketLeft => "[",
    BracketRight => "]",
    Backslash => "\\",
    Semicolon => ";",
    Quote => "'",
    Comma => ",",
    Period => ".",
    Slash => "/",
    Backquote => "`",
}

impl KeyCode {
    /// The key with this W3C code. Exact match only: `"keya"` and `" KeyA"`
    /// name no key.
    pub fn from_id(id: &str) -> Option<KeyCode> {
        Self::ALL.iter().copied().find(|key| key.id() == id)
    }

    /// `true` for F1 to F20, which may open the wheel with no modifier.
    fn is_function_key(self) -> bool {
        matches!(
            self,
            KeyCode::F1
                | KeyCode::F2
                | KeyCode::F3
                | KeyCode::F4
                | KeyCode::F5
                | KeyCode::F6
                | KeyCode::F7
                | KeyCode::F8
                | KeyCode::F9
                | KeyCode::F10
                | KeyCode::F11
                | KeyCode::F12
                | KeyCode::F13
                | KeyCode::F14
                | KeyCode::F15
                | KeyCode::F16
                | KeyCode::F17
                | KeyCode::F18
                | KeyCode::F19
                | KeyCode::F20
        )
    }
}

/// The keys that, with Command (and Shift or not), belong to every Mac app:
/// quit, close, hide, minimize, the app and input switchers, copy, paste,
/// cut, select all, undo and save.
const RESERVED_WITH_COMMAND: [KeyCode; 12] = [
    KeyCode::KeyQ,
    KeyCode::KeyW,
    KeyCode::KeyH,
    KeyCode::KeyM,
    KeyCode::Tab,
    KeyCode::Space,
    KeyCode::KeyC,
    KeyCode::KeyV,
    KeyCode::KeyX,
    KeyCode::KeyA,
    KeyCode::KeyZ,
    KeyCode::KeyS,
];

/// A key with the modifiers held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    /// The key.
    pub key: KeyCode,
    /// The modifiers held with it.
    pub mods: Modifiers,
}

/// Why a chord can't open the Shortcuts wheel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyProblem {
    /// It needs Control, Option/Alt or Command/Meta, unless its key is F1 to
    /// F20. Shift alone isn't enough: it would eat capital letters.
    NeedsModifier,
    /// Every app uses it (Command+Q and the like, on the Mac).
    Reserved,
}

impl Chord {
    /// Why this chord can't open the wheel, if it can't. `mac` is whether
    /// the Mac's rules apply; nothing is reserved elsewhere.
    pub fn hotkey_problem(self, mac: bool) -> Option<HotkeyProblem> {
        let Modifiers {
            shift: _,
            ctrl,
            alt,
            meta,
        } = self.mods;
        if !(ctrl || alt || meta || self.key.is_function_key()) {
            return Some(HotkeyProblem::NeedsModifier);
        }
        if mac && meta && !ctrl && !alt && RESERVED_WITH_COMMAND.contains(&self.key) {
            return Some(HotkeyProblem::Reserved);
        }
        None
    }

    /// "Control+Option+G" on the Mac, "Ctrl+Alt+G" elsewhere. The order is
    /// always ctrl, alt, shift, meta, key.
    pub fn label(self, mac: bool) -> String {
        let names = if mac {
            ["Control", "Option", "Shift", "Command"]
        } else {
            ["Ctrl", "Alt", "Shift", "Win"]
        };
        let held = [
            self.mods.ctrl,
            self.mods.alt,
            self.mods.shift,
            self.mods.meta,
        ];
        let mut label = String::new();
        for (name, held) in names.into_iter().zip(held) {
            if held {
                label.push_str(name);
                label.push('+');
            }
        }
        label.push_str(self.key.label());
        label
    }

    /// Every chord with this key and a subset of these modifiers: 1 to 16
    /// of them, the bare key first and this chord last.
    pub fn with_modifier_subsets(self) -> Vec<Chord> {
        let mut chords = Vec::with_capacity(1 << self.mods.count());
        for bits in 0_u8..16 {
            let mods = Modifiers {
                shift: bits & 1 != 0,
                ctrl: bits & 2 != 0,
                alt: bits & 4 != 0,
                meta: bits & 8 != 0,
            };
            if mods & self.mods == mods {
                chords.push(Chord {
                    key: self.key,
                    mods,
                });
            }
        }
        chords
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn chord(key: KeyCode, mods: Modifiers) -> Chord {
        Chord { key, mods }
    }

    fn mods(shift: bool, ctrl: bool, alt: bool, meta: bool) -> Modifiers {
        Modifiers {
            shift,
            ctrl,
            alt,
            meta,
        }
    }

    /// All 16 sets of modifiers.
    fn every_mods() -> Vec<Modifiers> {
        (0_u8..16)
            .map(|bits| mods(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0))
            .collect()
    }

    const ALL_FOUR: Modifiers = Modifiers {
        shift: true,
        ctrl: true,
        alt: true,
        meta: true,
    };

    /// The `KeyboardEvent.code` values of the W3C's UI Events spec that a
    /// US keyboard has outside the letters, digits and function keys.
    const W3C_NAMED: [&str; 25] = [
        "Space",
        "Enter",
        "Tab",
        "Escape",
        "Backspace",
        "Delete",
        "ArrowUp",
        "ArrowDown",
        "ArrowLeft",
        "ArrowRight",
        "Home",
        "End",
        "PageUp",
        "PageDown",
        "Minus",
        "Equal",
        "BracketLeft",
        "BracketRight",
        "Backslash",
        "Semicolon",
        "Quote",
        "Comma",
        "Period",
        "Slash",
        "Backquote",
    ];

    fn w3c_codes() -> HashSet<String> {
        let mut codes: HashSet<String> = W3C_NAMED.iter().map(|code| (*code).to_owned()).collect();
        codes.extend(('A'..='Z').map(|letter| format!("Key{letter}")));
        codes.extend((0..=9).map(|digit| format!("Digit{digit}")));
        codes.extend((1..=24).map(|n| format!("F{n}")));
        codes
    }

    #[test]
    fn the_list_holds_every_key_once() {
        assert_eq!(KeyCode::ALL.len(), 26 + 10 + 20 + 14 + 11);
        let unique: HashSet<KeyCode> = KeyCode::ALL.iter().copied().collect();
        assert_eq!(unique.len(), KeyCode::ALL.len());
        assert_eq!(KeyCode::ALL.first(), Some(&KeyCode::KeyA));
        assert_eq!(KeyCode::ALL.last(), Some(&KeyCode::Backquote));
    }

    #[test]
    fn ids_round_trip() {
        for &key in KeyCode::ALL {
            assert_eq!(KeyCode::from_id(key.id()), Some(key), "{}", key.id());
        }
    }

    #[test]
    fn ids_are_unique_w3c_codes() {
        let w3c = w3c_codes();
        let mut seen = HashSet::new();
        for &key in KeyCode::ALL {
            assert!(w3c.contains(key.id()), "{} isn't a W3C code", key.id());
            assert!(seen.insert(key.id()), "{} is there twice", key.id());
            // The variant is named by its code.
            assert_eq!(format!("{key:?}"), key.id());
        }
        assert_eq!(KeyCode::KeyA.id(), "KeyA");
        assert_eq!(KeyCode::Digit4.id(), "Digit4");
        assert_eq!(KeyCode::F5.id(), "F5");
        assert_eq!(KeyCode::ArrowUp.id(), "ArrowUp");
    }

    #[test]
    fn from_id_takes_an_exact_match_only() {
        assert_eq!(KeyCode::from_id("KeyG"), Some(KeyCode::KeyG));
        assert_eq!(KeyCode::from_id("F20"), Some(KeyCode::F20));
        for wrong in [
            "", "keyg", "KEYG", " KeyG", "KeyG ", "G", "Key", "F0", "F21", "Return", "↑",
        ] {
            assert_eq!(KeyCode::from_id(wrong), None, "{wrong:?}");
        }
    }

    #[test]
    fn labels_are_short_names_nobody_shares() {
        let mut seen = HashSet::new();
        for &key in KeyCode::ALL {
            let label = key.label();
            assert!(!label.is_empty(), "{}", key.id());
            assert_eq!(label.trim(), label, "{}", key.id());
            assert!(!label.chars().any(char::is_control), "{}", key.id());
            assert!(seen.insert(label), "{label} names two keys");
        }
        assert_eq!(KeyCode::KeyA.label(), "A");
        assert_eq!(KeyCode::Digit4.label(), "4");
        assert_eq!(KeyCode::F5.label(), "F5");
        assert_eq!(KeyCode::ArrowUp.label(), "↑");
        assert_eq!(KeyCode::Space.label(), "Space");
        assert_eq!(KeyCode::Enter.label(), "Return");
        assert_eq!(KeyCode::Backspace.label(), "Delete");
        assert_eq!(KeyCode::Delete.label(), "Forward Delete");
        assert_eq!(KeyCode::Backslash.label(), "\\");
    }

    #[test]
    fn a_hotkey_needs_control_option_or_command() {
        for mac in [true, false] {
            for &key in KeyCode::ALL {
                if key.is_function_key() {
                    continue;
                }
                for held in [Modifiers::NONE, Modifiers::SHIFT] {
                    assert_eq!(
                        chord(key, held).hotkey_problem(mac),
                        Some(HotkeyProblem::NeedsModifier),
                        "{} mac={mac}",
                        key.id()
                    );
                }
                // Control and Option together are never reserved.
                for held in [
                    mods(false, true, true, false),
                    mods(true, true, true, false),
                    Modifiers::CTRL,
                    Modifiers::ALT,
                ] {
                    assert_eq!(chord(key, held).hotkey_problem(mac), None, "{}", key.id());
                }
            }
        }
    }

    #[test]
    fn a_function_key_needs_no_modifier() {
        let function_keys: Vec<KeyCode> = KeyCode::ALL
            .iter()
            .copied()
            .filter(|key| key.is_function_key())
            .collect();
        assert_eq!(function_keys.len(), 20);
        assert_eq!(function_keys.first(), Some(&KeyCode::F1));
        assert_eq!(function_keys.last(), Some(&KeyCode::F20));
        for mac in [true, false] {
            for &key in &function_keys {
                for held in every_mods() {
                    assert_eq!(chord(key, held).hotkey_problem(mac), None, "{}", key.id());
                }
            }
        }
    }

    #[test]
    fn command_with_an_every_app_key_is_reserved_on_the_mac() {
        let reserved = [
            KeyCode::KeyQ,
            KeyCode::KeyW,
            KeyCode::KeyH,
            KeyCode::KeyM,
            KeyCode::Tab,
            KeyCode::Space,
            KeyCode::KeyC,
            KeyCode::KeyV,
            KeyCode::KeyX,
            KeyCode::KeyA,
            KeyCode::KeyZ,
            KeyCode::KeyS,
        ];
        let command_shift = mods(true, false, false, true);
        for key in reserved {
            for held in [Modifiers::META, command_shift] {
                assert_eq!(
                    chord(key, held).hotkey_problem(true),
                    Some(HotkeyProblem::Reserved),
                    "{}",
                    key.id()
                );
                assert_eq!(chord(key, held).hotkey_problem(false), None, "{}", key.id());
            }
            // With Control or Option too, the chord is the user's to take.
            for held in [
                mods(false, true, false, true),
                mods(false, false, true, true),
                ALL_FOUR,
            ] {
                assert_eq!(chord(key, held).hotkey_problem(true), None, "{}", key.id());
            }
        }
    }

    #[test]
    fn nothing_else_is_reserved_and_nothing_at_all_off_the_mac() {
        let count = |mac: bool| {
            KeyCode::ALL
                .iter()
                .flat_map(|&key| every_mods().into_iter().map(move |held| chord(key, held)))
                .filter(|chord| chord.hotkey_problem(mac) == Some(HotkeyProblem::Reserved))
                .count()
        };
        // Twelve keys, with Command and with Command+Shift.
        assert_eq!(count(true), 24);
        assert_eq!(count(false), 0);
        assert_eq!(
            chord(KeyCode::KeyG, Modifiers::META).hotkey_problem(true),
            None
        );
    }

    #[test]
    fn the_default_hotkey_has_no_problem() {
        let default = chord(KeyCode::KeyG, mods(false, true, true, false));
        assert_eq!(default.hotkey_problem(true), None);
        assert_eq!(default.hotkey_problem(false), None);
        assert_eq!(default.label(true), "Control+Option+G");
        assert_eq!(default.label(false), "Ctrl+Alt+G");
    }

    #[test]
    fn labels_name_the_modifiers_in_one_order() {
        let all = chord(KeyCode::KeyA, ALL_FOUR);
        assert_eq!(all.label(true), "Control+Option+Shift+Command+A");
        assert_eq!(all.label(false), "Ctrl+Alt+Shift+Win+A");
        let bare = chord(KeyCode::F5, Modifiers::NONE);
        assert_eq!(bare.label(true), "F5");
        assert_eq!(bare.label(false), "F5");
        let cases = [
            (Modifiers::CTRL, "Control+↑", "Ctrl+↑"),
            (Modifiers::ALT, "Option+↑", "Alt+↑"),
            (Modifiers::SHIFT, "Shift+↑", "Shift+↑"),
            (Modifiers::META, "Command+↑", "Win+↑"),
            (
                mods(true, false, false, true),
                "Shift+Command+↑",
                "Shift+Win+↑",
            ),
            (
                mods(true, false, true, false),
                "Option+Shift+↑",
                "Alt+Shift+↑",
            ),
        ];
        for (held, on_mac, elsewhere) in cases {
            let chord = chord(KeyCode::ArrowUp, held);
            assert_eq!(chord.label(true), on_mac);
            assert_eq!(chord.label(false), elsewhere);
        }
        // A key whose own name is a sign keeps it.
        assert_eq!(
            chord(KeyCode::Minus, Modifiers::CTRL).label(false),
            "Ctrl+-"
        );
        assert_eq!(
            chord(KeyCode::Enter, Modifiers::META).label(true),
            "Command+Return"
        );
    }

    #[test]
    fn subsets_are_every_chord_under_this_one() {
        for held in every_mods() {
            let full = chord(KeyCode::Escape, held);
            let subsets = full.with_modifier_subsets();
            assert_eq!(subsets.len(), 1 << held.count());
            let unique: HashSet<Chord> = subsets.iter().copied().collect();
            assert_eq!(unique.len(), subsets.len());
            assert_eq!(
                subsets.first(),
                Some(&chord(KeyCode::Escape, Modifiers::NONE))
            );
            assert_eq!(subsets.last(), Some(&full));
            for subset in subsets {
                assert_eq!(subset.key, KeyCode::Escape);
                assert_eq!(subset.mods & held, subset.mods);
            }
        }
    }

    #[test]
    fn two_modifiers_make_four_chords_in_a_fixed_order() {
        let open = chord(KeyCode::Enter, mods(false, true, true, false));
        assert_eq!(
            open.with_modifier_subsets(),
            vec![
                chord(KeyCode::Enter, Modifiers::NONE),
                chord(KeyCode::Enter, Modifiers::CTRL),
                chord(KeyCode::Enter, Modifiers::ALT),
                chord(KeyCode::Enter, mods(false, true, true, false)),
            ]
        );
    }
}
