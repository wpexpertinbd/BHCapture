//! Capture file naming: a configurable pattern, sanitising, and collision handling.
//!
//! # Why this is a security surface, not just string formatting
//!
//! The pattern may include `{title}` and `{app}`, which come from **window metadata that any
//! running application controls**. A window titled `../../../../Library/LaunchAgents/eve` must not
//! be able to steer where a capture is written, and a title containing a NUL or a path separator
//! must not produce a path the caller did not intend.
//!
//! So [`Pattern::render`] always returns **exactly one path component**: separators, control
//! characters, and the characters that are illegal or dangerous on either platform are replaced,
//! never passed through. The result is also trimmed and length-capped, and can never be empty,
//! `.`, `..`, or a reserved Windows device name.
//!
//! The module does no I/O at all — [`unique_path`] takes an `exists` predicate — so every one of
//! these rules is testable without touching a filesystem.

use std::path::{Path, PathBuf};

/// Local wall-clock time, resolved by the shell. Core deliberately does not know about time
/// zones: that is platform discovery, and this crate does none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Values a pattern can interpolate.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fields<'a> {
    pub app: Option<&'a str>,
    pub title: Option<&'a str>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PatternError {
    Empty,
    UnknownToken(String),
    UnclosedToken,
}

impl std::fmt::Display for PatternError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatternError::Empty => write!(f, "pattern is empty"),
            PatternError::UnknownToken(t) => write!(f, "unknown token {{{t}}}"),
            PatternError::UnclosedToken => write!(f, "unclosed {{ in pattern"),
        }
    }
}
impl std::error::Error for PatternError {}

pub const TOKENS: &[&str] = &[
    "year", "month", "day", "hour", "minute", "second", "app", "title", "width", "height",
];

/// macOS's own screenshot naming, which is what most users already expect.
pub const DEFAULT_PATTERN: &str = "BHCapture {year}-{month}-{day} at {hour}.{minute}.{second}";

/// Leaves room for a " (12)" suffix and a ".png" inside a 255-byte filesystem component.
const MAX_STEM_BYTES: usize = 200;

/// Windows reserves these as device names, with or without an extension. A file called `CON.png`
/// cannot be created there, so a window titled "CON" must not produce one.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL",
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    raw: String,
}

impl Default for Pattern {
    fn default() -> Self {
        Pattern::parse(DEFAULT_PATTERN).expect("DEFAULT_PATTERN must be valid")
    }
}

impl Pattern {
    /// Validates the token names up front, so a typo is reported when the user saves their
    /// settings rather than silently producing files named `BHCapture {yaer}`.
    pub fn parse(raw: &str) -> Result<Self, PatternError> {
        if raw.trim().is_empty() {
            return Err(PatternError::Empty);
        }
        let mut rest = raw;
        while let Some(open) = rest.find('{') {
            let after = &rest[open + 1..];
            let close = after.find('}').ok_or(PatternError::UnclosedToken)?;
            let name = &after[..close];
            if !TOKENS.contains(&name) {
                return Err(PatternError::UnknownToken(name.to_string()));
            }
            rest = &after[close + 1..];
        }
        Ok(Pattern { raw: raw.to_string() })
    }

    pub fn as_str(&self) -> &str { &self.raw }

    /// Render to **one** sanitised path component (no extension).
    pub fn render(&self, at: Stamp, f: &Fields) -> String {
        let mut out = String::with_capacity(self.raw.len() + 32);
        let mut rest = self.raw.as_str();
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            // parse() already proved every brace closes on a known token.
            let close = after.find('}').unwrap_or(0);
            let name = &after[..close];
            match name {
                "year" => out.push_str(&format!("{:04}", at.year)),
                "month" => out.push_str(&format!("{:02}", at.month)),
                "day" => out.push_str(&format!("{:02}", at.day)),
                "hour" => out.push_str(&format!("{:02}", at.hour)),
                "minute" => out.push_str(&format!("{:02}", at.minute)),
                "second" => out.push_str(&format!("{:02}", at.second)),
                "app" => out.push_str(f.app.unwrap_or("")),
                "title" => out.push_str(f.title.unwrap_or("")),
                "width" => out.push_str(&f.width.to_string()),
                "height" => out.push_str(&f.height.to_string()),
                _ => {}
            }
            rest = &after[close + 1..];
        }
        out.push_str(rest);
        sanitize_component(&out)
    }
}

/// Reduce arbitrary text to a single safe filename component.
pub fn sanitize_component(input: &str) -> String {
    let mut s = String::with_capacity(input.len());
    for c in input.chars() {
        let mapped = match c {
            // Path separators -- the traversal risk. Never pass through.
            '/' | '\\' => '-',
            // Illegal on Windows; ':' is also the classic macOS/HFS separator.
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            // Control characters, DEL, and anything that could terminate a C string.
            c if (c as u32) < 0x20 || c as u32 == 0x7f => ' ',
            c => c,
        };
        s.push(mapped);
    }

    // Collapse runs of whitespace so a stripped control character does not leave a gap.
    let mut collapsed = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws { collapsed.push(' '); }
            in_ws = true;
        } else {
            collapsed.push(c);
            in_ws = false;
        }
    }

    // Windows silently strips trailing dots and spaces, which turns "a." into "a" and can defeat
    // a uniqueness check; leading dots make a hidden file on Unix.
    let trimmed = collapsed.trim().trim_matches(|c| c == '.' || c == ' ');
    let mut out = truncate_on_char_boundary(trimmed, MAX_STEM_BYTES).to_string();

    // "." and ".." are directory entries, not names.
    //
    // NOTE: the `== "."` / `== ".."` arms are **unreachable today** — the `trim_matches` above
    // already reduces both to "", which the is_empty() arm catches. Verified by deleting them and
    // watching no test fail. They are kept as defence in depth: if the trim is ever relaxed (say
    // to preserve a leading dot in a user's pattern) this is the layer that still refuses to
    // return a directory entry. No test covers these arms independently, by construction.
    if out.is_empty() || out == "." || out == ".." {
        out = "BHCapture".to_string();
    }
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&out)) {
        out = format!("{out}_");
    }
    out
}

/// Byte-cap without splitting a character. Truncating by bytes alone would corrupt any
/// multi-byte text — Bengali, emoji, accented Latin — into invalid UTF-8.
fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// First free path of the form `dir/stem.ext`, `dir/stem (2).ext`, ...
///
/// `exists` is injected rather than calling the filesystem, so core stays I/O-free and the
/// collision behaviour is testable. Note this is inherently racy against another process — the
/// caller should still create the file exclusively and retry.
pub fn unique_path(dir: &Path, stem: &str, ext: &str, exists: &dyn Fn(&Path) -> bool) -> PathBuf {
    let stem = sanitize_component(stem);
    let first = dir.join(format!("{stem}.{ext}"));
    if !exists(&first) {
        return first;
    }
    for n in 2..=9999u32 {
        let p = dir.join(format!("{stem} ({n}).{ext}"));
        if !exists(&p) {
            return p;
        }
    }
    // Pathological: fall back to something that cannot collide by counting.
    dir.join(format!("{stem} ({}).{ext}", std::process::id()))
}
