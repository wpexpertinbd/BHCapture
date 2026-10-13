use bhcapture_core::hotkey::*;

// These tests pin the macOS rules. They name macOS explicitly, so they mean the same on every OS the
// suite runs on -- CI on Linux/Windows caught them calling the running OS's parser instead.
fn mac(s: &str) -> Result<Chord, HotkeyError> { Chord::parse_for(s, Os::Mac) }

#[test]
fn section0_default_parses_and_is_not_a_system_shortcut() {
    // The RUNNING OS's default must pass the running OS's rules and print the way it is stored.
    let c = Chord::parse(DEFAULT_REGION_HOTKEY).unwrap();
    assert_eq!(c.display(), DEFAULT_REGION_HOTKEY);
    // And the macOS default is exactly ⌃⌘4, which macOS does not own.
    let m = mac("Ctrl+Cmd+4").unwrap();
    assert_eq!(m, Chord { cmd: true, ctrl: true, alt: false, shift: false, key: Key::Digit(4) });
    assert!(m.macos_conflict().is_none());
}

#[test]
fn the_old_default_is_rejected_because_macos_owns_it() {
    // The bug that shipped: ⌃⇧⌘4 fired BHCapture AND macOS's copy-area-to-clipboard.
    for s in ["Cmd+Shift+Ctrl+4", "ctrl+shift+cmd+4", "⌃⇧⌘4"] {
        assert!(matches!(mac(s), Err(HotkeyError::TakenBySystem(_))), "{s} must be refused");
    }
}

#[test]
fn every_macos_screenshot_shortcut_is_refused() {
    for s in ["Shift+Cmd+3", "Shift+Cmd+4", "Shift+Cmd+5", "Shift+Cmd+6",
              "Ctrl+Shift+Cmd+3", "Ctrl+Shift+Cmd+4", "Ctrl+Shift+Cmd+6"] {
        assert!(matches!(mac(s), Err(HotkeyError::TakenBySystem(_))), "{s} must be refused");
    }
    // Neighbours that macOS does NOT use stay allowed.
    for s in ["Ctrl+Cmd+4", "Ctrl+Shift+Cmd+5", "Ctrl+Shift+4", "Ctrl+Shift+C"] {
        assert!(mac(s).is_ok(), "{s} should be allowed");
    }
}

#[test]
fn spellings_people_type_all_parse_to_the_same_chord() {
    let want = mac("Ctrl+Cmd+4").unwrap();
    for s in ["ctrl+cmd+4", "Control-Command-4", " cmd + ctrl + 4 ", "⌃⌘4", "super+ctl+4"] {
        assert_eq!(mac(s).unwrap(), want, "{s}");
    }
}

#[test]
fn malformed_shortcuts_say_what_is_wrong() {
    assert_eq!(mac("").unwrap_err(), HotkeyError::Empty);
    assert_eq!(mac("  ").unwrap_err(), HotkeyError::Empty);
    assert_eq!(mac("ctrl+cmd").unwrap_err(), HotkeyError::NoKey);
    assert_eq!(mac("ctrl+a+b").unwrap_err(), HotkeyError::TwoKeys);
    assert_eq!(mac("ctrl+banana").unwrap_err(), HotkeyError::UnknownPart("banana".into()));
    // F13-F19 became valid (a PC keyboard's Print Screen reaches a Mac as F13); F20 is still unknown.
    assert!(mac("ctrl+f13").is_ok());
    assert_eq!(mac("ctrl+f20").unwrap_err(), HotkeyError::UnknownPart("f20".into()));
}

#[test]
fn a_bare_key_or_shift_alone_would_steal_typing() {
    assert_eq!(mac("4").unwrap_err(), HotkeyError::NeedsModifier);
    assert_eq!(mac("shift+a").unwrap_err(), HotkeyError::NeedsModifier);
    assert!(mac("F5").is_ok(), "a function key alone is not ordinary typing");
}

#[test]
fn display_is_canonical_and_round_trips() {
    let c = mac("cmd+shift+ctrl+f12").unwrap();
    assert_eq!(c.display_for(Os::Mac), "Ctrl+Shift+Cmd+F12");
    assert_eq!(mac(&c.display_for(Os::Mac)).unwrap(), c);
}

#[test]
fn option_shortcuts_are_refused_because_macos_never_delivers_them() {
    // Ctrl+Option+Cmd+4 registered "successfully" on macOS 27 and never once fired for a real
    // keypress. Refuse every Option chord rather than ship a shortcut that silently does nothing.
    for s in ["Ctrl+Option+Cmd+4", "⌃⌥⌘4", "Option+Shift+Cmd+4", "alt+f5", "Ctrl+Option+S"] {
        assert_eq!(mac(s).unwrap_err(), HotkeyError::UsesOption, "{s}");
    }
}

#[test]
fn windows_rules_allow_alt_refuse_win_and_name_keys_the_windows_way() {
    use bhcapture_core::hotkey::{Chord, HotkeyError, Os};
    // Alt is fine on Windows (macOS blocks Option).
    let c = Chord::parse_for("Ctrl+Alt+4", Os::Windows).unwrap();
    assert_eq!(c.display_for(Os::Windows), "Ctrl+Alt+4");
    assert_eq!(Chord::parse_for("Ctrl+Alt+4", Os::Mac), Err(HotkeyError::UsesOption));
    // The Win key is Windows' own.
    assert!(matches!(Chord::parse_for("Win+Shift+S", Os::Windows), Err(HotkeyError::TakenBySystem(_))));
    assert!(matches!(Chord::parse_for("Ctrl+C", Os::Windows), Err(HotkeyError::TakenBySystem(_))));
    // The Windows defaults are valid Windows chords.
    for d in ["Ctrl+Shift+4", "Ctrl+Shift+5", "Ctrl+Shift+6"] {
        assert!(Chord::parse_for(d, Os::Windows).is_ok(), "{d}");
    }
}

#[test]
fn print_screen_works_alone_and_f13_to_f19_exist() {
    use bhcapture_core::hotkey::{Chord, Key, Os};
    for spelling in ["PrintScreen", "prtsc", "Print", "PrtScn"] {
        let c = Chord::parse_for(spelling, Os::Windows).unwrap();
        assert_eq!(c.key, Key::PrintScreen);
        assert_eq!(c.display_for(Os::Windows), "PrintScreen");
    }
    assert!(Chord::parse_for("F13", Os::Mac).is_ok(), "a PC keyboard's Print Screen on a Mac");
    assert!(Chord::parse_for("F20", Os::Mac).is_err());
    // A letter on its own still steals typing.
    assert!(Chord::parse_for("P", Os::Windows).is_err());
}
