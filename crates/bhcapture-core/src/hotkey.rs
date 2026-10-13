//! Keyboard shortcuts: parsing the user's setting, and refusing chords the OS already owns.
//!
//! Why the conflict check exists: the first default, ⌃⇧⌘4, is macOS's own "copy picture of selected
//! area to the clipboard". Every press fired BOTH tools: BHCapture's selector and macOS's crosshair.
//! Esc closed ours while the system crosshair stayed, played its shutter sound and copied to the
//! clipboard instead of saving -- which looked to the user like BHCapture misbehaving. A shortcut that
//! collides with a system one is therefore rejected, not merely warned about.
//!
//! Pure: produces a description of the chord; the shell maps it onto the platform hotkey API.

/// The key part of a chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Letter(char),   // 'A'..='Z'
    Digit(u8),      // 0..=9
    F(u8),          // 1..=12
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: Key,
}

pub const DEFAULT_REGION_HOTKEY: &str = "Ctrl+Option+Cmd+4";

#[derive(Debug, PartialEq, Eq)]
pub enum HotkeyError {
    Empty,
    UnknownPart(String),
    NoKey,
    TwoKeys,
    /// A shortcut with no Cmd/Ctrl/Option would steal ordinary typing everywhere.
    NeedsModifier,
    /// Already used by the operating system; both would fire.
    TakenBySystem(&'static str),
}

impl std::fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HotkeyError::Empty => write!(f, "the shortcut is empty"),
            HotkeyError::UnknownPart(p) => write!(f, "\"{p}\" is not a key or modifier this app understands"),
            HotkeyError::NoKey => write!(f, "the shortcut has modifiers but no key"),
            HotkeyError::TwoKeys => write!(f, "the shortcut names more than one key"),
            HotkeyError::NeedsModifier => write!(f, "a shortcut needs Cmd, Ctrl or Option, or it would steal ordinary typing"),
            HotkeyError::TakenBySystem(what) => write!(f, "macOS already uses this shortcut for {what}"),
        }
    }
}
impl std::error::Error for HotkeyError {}

impl Chord {
    /// Accepts the forms people actually type: `Ctrl+Option+Cmd+4`, `cmd + shift + s`, `⌃⌥⌘4`,
    /// `Control-Alt-Command-F5`. Case-insensitive; `+`, `-`, spaces and the Mac glyphs all work.
    pub fn parse(s: &str) -> Result<Chord, HotkeyError> {
        // Turn glyphs into words so one tokenizer handles both styles.
        let spelled = s.replace('⌘', "+cmd+").replace('⌃', "+ctrl+").replace('⌥', "+alt+").replace('⇧', "+shift+");
        let parts: Vec<String> = spelled.split(|c: char| c == '+' || c == '-' || c.is_whitespace())
            .filter(|p| !p.is_empty()).map(|p| p.to_ascii_lowercase()).collect();
        if parts.is_empty() { return Err(HotkeyError::Empty); }

        let (mut cmd, mut ctrl, mut alt, mut shift) = (false, false, false, false);
        let mut key: Option<Key> = None;
        for p in &parts {
            match p.as_str() {
                "cmd" | "command" | "super" | "meta" => cmd = true,
                "ctrl" | "control" | "ctl" => ctrl = true,
                "alt" | "option" | "opt" => alt = true,
                "shift" => shift = true,
                other => {
                    let k = parse_key(other).ok_or_else(|| HotkeyError::UnknownPart(other.to_string()))?;
                    if key.replace(k).is_some() { return Err(HotkeyError::TwoKeys); }
                }
            }
        }
        let key = key.ok_or(HotkeyError::NoKey)?;
        // Shift alone is not enough; function keys are a deliberate exception (F5 on its own is
        // not ordinary typing).
        if !(cmd || ctrl || alt) && !matches!(key, Key::F(_)) {
            return Err(HotkeyError::NeedsModifier);
        }
        let chord = Chord { cmd, ctrl, alt, shift, key };
        if let Some(what) = chord.macos_conflict() {
            return Err(HotkeyError::TakenBySystem(what));
        }
        Ok(chord)
    }

    /// Canonical spelling, in macOS menu order (Ctrl, Option, Shift, Cmd).
    pub fn display(&self) -> String {
        let mut v: Vec<String> = Vec::new();
        if self.ctrl { v.push("Ctrl".into()); }
        if self.alt { v.push("Option".into()); }
        if self.shift { v.push("Shift".into()); }
        if self.cmd { v.push("Cmd".into()); }
        v.push(match self.key {
            Key::Letter(c) => c.to_string(),
            Key::Digit(d) => d.to_string(),
            Key::F(n) => format!("F{n}"),
        });
        v.join("+")
    }

    /// macOS's own default shortcuts that a capture tool is likely to collide with. Not every system
    /// shortcut -- the ones that would make BOTH fire for a screenshot app, plus the always-global ones.
    pub fn macos_conflict(&self) -> Option<&'static str> {
        let mods = (self.cmd, self.ctrl, self.alt, self.shift);
        match (mods, self.key) {
            ((true, false, false, true), Key::Digit(3)) => Some("\"Save picture of screen as a file\" (Shift-Cmd-3)"),
            ((true, false, false, true), Key::Digit(4)) => Some("\"Save picture of selected area as a file\" (Shift-Cmd-4)"),
            ((true, false, false, true), Key::Digit(5)) => Some("the Screenshot toolbar (Shift-Cmd-5)"),
            ((true, false, false, true), Key::Digit(6)) => Some("\"Save picture of the Touch Bar\" (Shift-Cmd-6)"),
            ((true, true, false, true), Key::Digit(3)) => Some("\"Copy picture of screen to the clipboard\" (Ctrl-Shift-Cmd-3)"),
            ((true, true, false, true), Key::Digit(4)) => Some("\"Copy picture of selected area to the clipboard\" (Ctrl-Shift-Cmd-4)"),
            ((true, true, false, true), Key::Digit(6)) => Some("\"Copy picture of the Touch Bar\" (Ctrl-Shift-Cmd-6)"),
            ((true, false, false, false), Key::Letter('Q')) => Some("Quit in every app (Cmd-Q)"),
            ((true, false, false, false), Key::Letter('W')) => Some("Close Window in every app (Cmd-W)"),
            ((true, false, false, false), Key::Letter('H')) => Some("Hide in every app (Cmd-H)"),
            _ => None,
        }
    }
}

fn parse_key(s: &str) -> Option<Key> {
    let b = s.as_bytes();
    if b.len() == 1 {
        let c = b[0] as char;
        if c.is_ascii_alphabetic() { return Some(Key::Letter(c.to_ascii_uppercase())); }
        if c.is_ascii_digit() { return Some(Key::Digit(c as u8 - b'0')); }
        return None;
    }
    if let Some(n) = s.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        if (1..=12).contains(&n) { return Some(Key::F(n)); }
    }
    None
}
