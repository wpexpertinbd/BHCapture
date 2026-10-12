//! Settings tests. The central one is that an UNREADABLE file is never overwritten —
//! BHDisplay shipped the opposite and saved an empty list over a real one.

use std::path::PathBuf;
use bhcapture_core::naming::DEFAULT_PATTERN;
use bhcapture_core::settings::*;

/// Each test gets its own directory so they cannot interfere through shared state —
/// the trap where a suite passes only because of the order it happens to run in.
fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bhc-settings-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn section0_rig_selfcheck() {
    let s = Settings::default();
    assert_eq!(s.pattern, DEFAULT_PATTERN);
    assert!(s.output_dir.is_none(), "core must not invent a platform path");
    assert!(s.copy_to_clipboard);
    // Round-trips through JSON unchanged.
    let back: Settings = serde_json::from_str(&s.to_json()).unwrap();
    assert_eq!(back, s);
}

// ---------------------------------------------------------------------------
// The rule
// ---------------------------------------------------------------------------

#[test]
fn a_corrupt_file_is_never_overwritten() {
    let dir = tmpdir("corrupt");
    let p = dir.join("settings.json");
    let original = "{ this is not json at all";
    std::fs::write(&p, original).unwrap();

    let (mut store, outcome) = SettingsStore::load(&p);
    assert!(matches!(outcome, LoadOutcome::Unreadable(_)), "got {outcome:?}");
    assert!(!store.is_writable());

    // Defaults are usable in memory...
    assert_eq!(store.get().pattern, DEFAULT_PATTERN);
    // ...but a save must be refused, with a reason.
    store.set(Settings { copy_to_clipboard: false, ..Default::default() });
    let err = store.save().unwrap_err();
    assert!(matches!(err, SaveError::RefusedUnreadable(_)), "got {err:?}");

    // The decisive assertion: the bytes on disk are untouched.
    assert_eq!(std::fs::read_to_string(&p).unwrap(), original);
}

#[test]
fn an_unreadable_file_is_never_overwritten() {
    let dir = tmpdir("unreadable");
    let p = dir.join("settings.json");
    // A directory where the file should be: exists, cannot be read as a file. Models a
    // permissions failure without needing to drop privileges in a test.
    std::fs::create_dir_all(&p).unwrap();

    let (store, outcome) = SettingsStore::load(&p);
    assert!(matches!(outcome, LoadOutcome::Unreadable(_)), "got {outcome:?}");
    assert!(!store.is_writable(), "a read failure must NOT be treated as 'no settings yet'");
    assert!(matches!(store.save().unwrap_err(), SaveError::RefusedUnreadable(_)));
    assert!(p.is_dir(), "the existing entry must survive");
}

#[test]
fn a_genuinely_missing_file_is_writable() {
    let dir = tmpdir("missing");
    let p = dir.join("settings.json");
    let (mut store, outcome) = SettingsStore::load(&p);
    assert!(matches!(outcome, LoadOutcome::Missing), "got {outcome:?}");
    assert!(store.is_writable(), "absent is not the same as unreadable");
    store.set(Settings { include_cursor: true, ..Default::default() });
    store.save().unwrap();

    let (reloaded, outcome) = SettingsStore::load(&p);
    assert!(matches!(outcome, LoadOutcome::Loaded { .. }));
    assert!(reloaded.get().include_cursor);
}

// ---------------------------------------------------------------------------
// Durability and privacy
// ---------------------------------------------------------------------------

#[test]
fn saving_is_atomic_and_leaves_no_temp_files() {
    let dir = tmpdir("atomic");
    let p = dir.join("settings.json");
    let (mut store, _) = SettingsStore::load(&p);
    store.set(Settings::default());
    store.save().unwrap();
    store.save().unwrap();

    let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "settings.json")
        .collect();
    assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
}

#[cfg(unix)]
#[test]
fn the_settings_file_is_not_world_readable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmpdir("perms");
    let p = dir.join("nested").join("settings.json");
    let (mut store, _) = SettingsStore::load(&p);
    store.set(Settings::default());
    store.save().unwrap();

    let fmode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(fmode, 0o600, "settings file mode {fmode:o}");
    let dmode = std::fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777;
    assert_eq!(dmode, 0o700, "settings dir mode {dmode:o}");
}

// ---------------------------------------------------------------------------
// Repair, not rejection
// ---------------------------------------------------------------------------

#[test]
fn one_bad_field_does_not_discard_the_others() {
    let dir = tmpdir("repair");
    let p = dir.join("settings.json");
    std::fs::write(&p, r#"{
        "pattern": "shot-{yaer}",
        "output_dir": "relative/path",
        "region_hotkey": "   ",
        "copy_to_clipboard": false,
        "include_cursor": true
    }"#).unwrap();

    let (store, outcome) = SettingsStore::load(&p);
    let LoadOutcome::Loaded { repaired } = outcome else { panic!("got {outcome:?}") };
    assert_eq!(repaired.len(), 3, "should report each repair: {repaired:?}");

    let s = store.get();
    assert_eq!(s.pattern, DEFAULT_PATTERN, "invalid pattern reset");
    assert!(s.output_dir.is_none(), "relative path reset -- it would resolve against '/' for a .app");
    assert!(!s.region_hotkey.trim().is_empty());
    // The valid fields must survive.
    assert!(!s.copy_to_clipboard, "a valid field was lost while repairing another");
    assert!(s.include_cursor, "a valid field was lost while repairing another");
}

#[test]
fn unknown_and_missing_keys_fall_back_to_defaults() {
    let dir = tmpdir("partial");
    let p = dir.join("settings.json");
    // A future version's key, plus most keys absent.
    std::fs::write(&p, r#"{ "include_cursor": true, "some_future_key": 42 }"#).unwrap();
    let (store, outcome) = SettingsStore::load(&p);
    assert!(matches!(outcome, LoadOutcome::Loaded { .. }), "got {outcome:?}");
    assert!(store.get().include_cursor);
    assert_eq!(store.get().pattern, DEFAULT_PATTERN);
    assert!(store.get().copy_to_clipboard, "absent keys take the default, not false");
}

#[test]
fn set_reports_repairs_so_the_ui_can_say_so() {
    let dir = tmpdir("set");
    let (mut store, _) = SettingsStore::load(dir.join("settings.json"));
    let repaired = store.set(Settings { pattern: "{nope}".into(), ..Default::default() });
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(store.get().pattern, DEFAULT_PATTERN);
}
