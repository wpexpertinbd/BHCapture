use bhcapture_core::hotkey::*;

#[test]
fn section0_default_parses_and_is_not_a_system_shortcut() {
    let c = Chord::parse(DEFAULT_REGION_HOTKEY).unwrap();
    assert_eq!(c, Chord { cmd: true, ctrl: true, alt: false, shift: false, key: Key::Digit(4) });
    assert!(c.macos_conflict().is_none());
    assert_eq!(c.display(), DEFAULT_REGION_HOTKEY);
}

#[test]
fn the_old_default_is_rejected_because_macos_owns_it() {
    // The bug that shipped: ⌃⇧⌘4 fired BHCapture AND macOS's copy-area-to-clipboard.
    for s in ["Cmd+Shift+Ctrl+4", "ctrl+shift+cmd+4", "⌃⇧⌘4"] {
        assert!(matches!(Chord::parse(s), Err(HotkeyError::TakenBySystem(_))), "{s} must be refused");
    }
}

#[test]
fn every_macos_screenshot_shortcut_is_refused() {
    for s in ["Shift+Cmd+3", "Shift+Cmd+4", "Shift+Cmd+5", "Shift+Cmd+6",
              "Ctrl+Shift+Cmd+3", "Ctrl+Shift+Cmd+4", "Ctrl+Shift+Cmd+6"] {
        assert!(matches!(Chord::parse(s), Err(HotkeyError::TakenBySystem(_))), "{s} must be refused");
    }
    // Neighbours that macOS does NOT use stay allowed.
    for s in ["Ctrl+Cmd+4", "Ctrl+Shift+Cmd+5", "Ctrl+Shift+4", "Ctrl+Shift+C"] {
        assert!(Chord::parse(s).is_ok(), "{s} should be allowed");
    }
}

#[test]
fn spellings_people_type_all_parse_to_the_same_chord() {
    let want = Chord::parse("Ctrl+Cmd+4").unwrap();
    for s in ["ctrl+cmd+4", "Control-Command-4", " cmd + ctrl + 4 ", "⌃⌘4", "super+ctl+4"] {
        assert_eq!(Chord::parse(s).unwrap(), want, "{s}");
    }
}

#[test]
fn malformed_shortcuts_say_what_is_wrong() {
    assert_eq!(Chord::parse("").unwrap_err(), HotkeyError::Empty);
    assert_eq!(Chord::parse("  ").unwrap_err(), HotkeyError::Empty);
    assert_eq!(Chord::parse("ctrl+cmd").unwrap_err(), HotkeyError::NoKey);
    assert_eq!(Chord::parse("ctrl+a+b").unwrap_err(), HotkeyError::TwoKeys);
    assert_eq!(Chord::parse("ctrl+banana").unwrap_err(), HotkeyError::UnknownPart("banana".into()));
    assert_eq!(Chord::parse("ctrl+f13").unwrap_err(), HotkeyError::UnknownPart("f13".into()));
}

#[test]
fn a_bare_key_or_shift_alone_would_steal_typing() {
    assert_eq!(Chord::parse("4").unwrap_err(), HotkeyError::NeedsModifier);
    assert_eq!(Chord::parse("shift+a").unwrap_err(), HotkeyError::NeedsModifier);
    assert!(Chord::parse("F5").is_ok(), "a function key alone is not ordinary typing");
}

#[test]
fn display_is_canonical_and_round_trips() {
    let c = Chord::parse("cmd+shift+ctrl+f12").unwrap();
    assert_eq!(c.display(), "Ctrl+Shift+Cmd+F12");
    assert_eq!(Chord::parse(&c.display()).unwrap(), c);
}

#[test]
fn option_shortcuts_are_refused_because_macos_never_delivers_them() {
    // Ctrl+Option+Cmd+4 registered "successfully" on macOS 27 and never once fired for a real
    // keypress. Refuse every Option chord rather than ship a shortcut that silently does nothing.
    for s in ["Ctrl+Option+Cmd+4", "⌃⌥⌘4", "Option+Shift+Cmd+4", "alt+f5", "Ctrl+Option+S"] {
        assert_eq!(Chord::parse(s).unwrap_err(), HotkeyError::UsesOption, "{s}");
    }
}
