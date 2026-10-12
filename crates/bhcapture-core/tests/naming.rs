//! Naming tests, with the security cases first: `{title}` and `{app}` come from window metadata
//! that ANY running application controls, so they are untrusted input flowing into a filesystem
//! path.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use bhcapture_core::naming::*;

fn at() -> Stamp { Stamp { year: 2026, month: 10, day: 13, hour: 3, minute: 7, second: 9 } }

fn titled(t: &str) -> String {
    Pattern::parse("{title}").unwrap().render(at(), &Fields { title: Some(t), ..Default::default() })
}

// ---------------------------------------------------------------------------
// 0. Self-check
// ---------------------------------------------------------------------------

#[test]
fn section0_rig_selfcheck() {
    let p = Pattern::default();
    assert_eq!(p.render(at(), &Fields::default()), "BHCapture 2026-10-13 at 03.07.09");
}

// ---------------------------------------------------------------------------
// 1. A hostile window title cannot escape the output directory.
// ---------------------------------------------------------------------------

#[test]
fn a_hostile_title_cannot_traverse_out_of_the_directory() {
    let hostile = [
        "../../../../etc/cron.d/evil",
        "..\\..\\Windows\\System32\\drivers\\etc\\hosts",
        "/etc/passwd",
        "foo/bar/baz",
        "a:b",
        "..",
        ".",
        "../",
    ];
    for t in hostile {
        let got = titled(t);
        assert!(!got.contains('/'), "{t:?} -> {got:?} still contains '/'");
        assert!(!got.contains('\\'), "{t:?} -> {got:?} still contains '\\\\'");
        assert_ne!(got, "..");
        assert_ne!(got, ".");
        // The decisive check: joining it must stay inside the directory.
        let joined = Path::new("/out").join(&got);
        assert_eq!(joined.parent(), Some(Path::new("/out")), "{t:?} -> {got:?} escaped");
        assert!(joined.starts_with("/out"), "{t:?} -> {got:?} escaped");
    }
}

#[test]
fn control_characters_and_nul_are_removed() {
    let got = titled("ev\u{0}il\nnew\tline\u{7f}");
    assert!(!got.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f),
        "control characters survived in {got:?}");
    assert!(!got.contains('\u{0}'));
}

#[test]
fn windows_illegal_characters_are_replaced() {
    let got = titled(r#"a*b?c"d<e>f|g:h"#);
    for c in ['*', '?', '"', '<', '>', '|', ':'] {
        assert!(!got.contains(c), "{c:?} survived in {got:?}");
    }
}

#[test]
fn windows_reserved_device_names_cannot_be_produced() {
    for name in ["CON", "con", "PRN", "aux", "NUL", "COM1", "lpt9"] {
        let got = titled(name);
        assert!(!RESERVED_EQ(&got), "{name:?} -> {got:?} is still a reserved device name");
    }
}
#[allow(non_snake_case)]
fn RESERVED_EQ(s: &str) -> bool {
    ["CON","PRN","AUX","NUL","COM1","COM2","COM3","COM4","COM5","COM6","COM7","COM8","COM9",
     "LPT1","LPT2","LPT3","LPT4","LPT5","LPT6","LPT7","LPT8","LPT9"]
        .iter().any(|r| r.eq_ignore_ascii_case(s))
}

#[test]
fn trailing_dots_and_spaces_go_away() {
    // Windows strips these silently, which would defeat a uniqueness check.
    for t in ["name.", "name ", "name. . ", " .name. "] {
        let got = titled(t);
        assert!(!got.ends_with('.') && !got.ends_with(' '), "{t:?} -> {got:?}");
        assert!(!got.starts_with('.') && !got.starts_with(' '), "{t:?} -> {got:?}");
    }
}

/// Covers the OUTCOME ("never a directory entry") rather than any one layer. The explicit
/// `== "."` / `== ".."` arms in sanitize_component are unreachable because the trim gets there
/// first — deleting them fails nothing — so they are documented as defence in depth, not as a
/// guard this suite verifies.
#[test]
fn an_empty_or_fully_stripped_title_still_yields_a_usable_name() {
    for t in ["", "   ", "...", "///", "\u{0}\u{0}"] {
        let got = titled(t);
        assert!(!got.is_empty(), "{t:?} produced an empty filename");
        assert_eq!(Path::new("/out").join(&got).parent(), Some(Path::new("/out")));
    }
}

#[test]
fn long_titles_are_capped_without_corrupting_utf8() {
    // Bengali: every character is multi-byte, so a naive byte truncation makes invalid UTF-8.
    let long = "একটিবড়শিরোনাম".repeat(80);
    let got = titled(&long);
    assert!(got.len() <= 200, "not capped: {} bytes", got.len());
    assert!(std::str::from_utf8(got.as_bytes()).is_ok(), "truncation corrupted UTF-8");
    // Emoji sit outside the BMP, four bytes each -- the other way to split a character.
    let emoji = "🙂".repeat(100);
    let got2 = titled(&emoji);
    assert!(got2.len() <= 200);
    assert!(std::str::from_utf8(got2.as_bytes()).is_ok());
    assert!(!got2.is_empty());
}

// ---------------------------------------------------------------------------
// 2. Pattern parsing
// ---------------------------------------------------------------------------

#[test]
fn bad_patterns_are_rejected_when_saved_not_when_used() {
    assert_eq!(Pattern::parse("").unwrap_err(), PatternError::Empty);
    assert_eq!(Pattern::parse("   ").unwrap_err(), PatternError::Empty);
    assert_eq!(Pattern::parse("shot-{yaer}").unwrap_err(), PatternError::UnknownToken("yaer".into()));
    assert_eq!(Pattern::parse("shot-{year").unwrap_err(), PatternError::UnclosedToken);
    for t in TOKENS {
        assert!(Pattern::parse(&format!("x{{{t}}}")).is_ok(), "token {t} should be valid");
    }
}

#[test]
fn every_documented_token_actually_interpolates() {
    let f = Fields { app: Some("Safari"), title: Some("Inbox"), width: 1280, height: 840 };
    // Separator is '_' deliberately: '|' is Windows-illegal and the sanitiser replaces it, so a
    // '|'-separated expectation would be testing the sanitiser, not interpolation.
    let p = Pattern::parse(
        "{year}_{month}_{day}_{hour}_{minute}_{second}_{app}_{title}_{width}_{height}").unwrap();
    assert_eq!(p.render(at(), &f), "2026_10_13_03_07_09_Safari_Inbox_1280_840");
}

#[test]
fn missing_app_and_title_leave_no_literal_placeholder() {
    let got = Pattern::parse("shot {app}{title}").unwrap().render(at(), &Fields::default());
    assert!(!got.contains('{') && !got.contains("app") && !got.contains("title"), "{got:?}");
    assert!(!got.is_empty());
}

// ---------------------------------------------------------------------------
// 3. Collisions
// ---------------------------------------------------------------------------

#[test]
fn collisions_walk_forward_and_never_return_a_taken_path() {
    let dir = PathBuf::from("/out");
    let taken: HashSet<PathBuf> = ["/out/shot.png", "/out/shot (2).png", "/out/shot (3).png"]
        .iter().map(PathBuf::from).collect();
    let got = unique_path(&dir, "shot", "png", &|p| taken.contains(p));
    assert_eq!(got, PathBuf::from("/out/shot (4).png"));
    assert!(!taken.contains(&got));
}

#[test]
fn no_collision_means_the_plain_name() {
    let got = unique_path(Path::new("/out"), "shot", "png", &|_| false);
    assert_eq!(got, PathBuf::from("/out/shot.png"));
}

#[test]
fn unique_path_sanitises_the_stem_too() {
    // Otherwise the sanitiser could be bypassed by calling unique_path directly.
    let got = unique_path(Path::new("/out"), "../../evil", "png", &|_| false);
    assert_eq!(got.parent(), Some(Path::new("/out")), "escaped via unique_path: {got:?}");
}

#[test]
fn an_exhausted_counter_still_returns_a_path_rather_than_hanging() {
    // Every counted candidate taken: must terminate, and must not return a taken path.
    let got = unique_path(Path::new("/out"), "shot", "png", &|p| {
        p.file_name().unwrap().to_str().unwrap().starts_with("shot")
            && !p.to_str().unwrap().contains(&std::process::id().to_string())
    });
    assert!(got.to_str().unwrap().contains(&std::process::id().to_string()), "{got:?}");
}
