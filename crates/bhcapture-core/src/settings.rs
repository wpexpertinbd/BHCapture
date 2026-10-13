//! Persisted settings.
//!
//! # The rule this module exists to enforce
//!
//! **"Unreadable" is not "not found".** A settings file that exists but cannot be read or parsed
//! — a permissions problem, a half-written file, a disk error, a future version's format — must
//! never be silently replaced with defaults. Doing that destroys the user's real configuration at
//! the exact moment something is already wrong. (BHDisplay shipped this bug: a refused read at
//! login was treated as "no data", and it saved an empty pairing list over the real one.)
//!
//! So [`SettingsStore`] tracks three distinct outcomes — loaded, genuinely absent, and unreadable
//! — and **refuses to save** in the third. The refusal is in the type, not in a comment: there is
//! no code path that writes while `writable` is false.
//!
//! No Keychain (family rule). Plain files under a directory the shell chooses, 0700/0600.

use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use crate::hotkey::{Chord, DEFAULT_REGION_HOTKEY};
use crate::naming::{Pattern, DEFAULT_PATTERN};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `None` means "use the platform default". Core does no platform discovery, so the shell
    /// resolves that to `~/Pictures/BHCapture`.
    pub output_dir: Option<PathBuf>,
    pub pattern: String,
    /// Open a Save dialog for every capture instead of writing straight to `output_dir`.
    pub always_ask_where_to_save: bool,
    pub copy_to_clipboard: bool,
    pub include_cursor: bool,
    pub region_hotkey: String,
    pub theme: Theme,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output_dir: None,
            pattern: DEFAULT_PATTERN.to_string(),
            always_ask_where_to_save: false,
            copy_to_clipboard: true,
            include_cursor: false,
            region_hotkey: DEFAULT_REGION_HOTKEY.to_string(),
            theme: Theme::System,
        }
    }
}

impl Settings {
    /// Repair individually invalid fields instead of rejecting the whole file.
    ///
    /// A single bad value — a pattern typo, a relative path — should cost the user that one
    /// setting, not all of them. Returns what was repaired so the UI can say so rather than
    /// silently disagreeing with what the user typed.
    pub fn normalise(&mut self) -> Vec<String> {
        let mut fixed = Vec::new();

        if Pattern::parse(&self.pattern).is_err() {
            fixed.push(format!("filename pattern {:?} was invalid; reset to the default", self.pattern));
            self.pattern = DEFAULT_PATTERN.to_string();
        }
        // A relative output directory would resolve against the process's working directory,
        // which for a launched .app is "/" -- captures would vanish somewhere unexpected.
        if let Some(dir) = &self.output_dir {
            if dir.as_os_str().is_empty() || !dir.is_absolute() {
                fixed.push(format!("output folder {} was not an absolute path; reset to the default", dir.display()));
                self.output_dir = None;
            }
        }
        // Parse, and store the CANONICAL spelling, so "cmd+shift+s" and "⇧⌘S" are the same setting.
        // A chord macOS already owns is rejected: both would fire (see bhcapture_core::hotkey).
        match Chord::parse(&self.region_hotkey) {
            Ok(c) => self.region_hotkey = c.display(),
            Err(e) => {
                fixed.push(format!("capture shortcut {:?} was not used: {e}. Reset to {}",
                    self.region_hotkey, DEFAULT_REGION_HOTKEY));
                self.region_hotkey = DEFAULT_REGION_HOTKEY.to_string();
            }
        }
        fixed
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
}

#[derive(Debug)]
pub enum LoadOutcome {
    /// Parsed. `repaired` lists any individually invalid fields that were reset.
    Loaded { repaired: Vec<String> },
    /// No file yet. Defaults are in use and saving is safe.
    Missing,
    /// The file exists but could not be read or parsed. Defaults are in use **in memory only**;
    /// saving is refused so the unreadable file is not destroyed.
    Unreadable(String),
}

#[derive(Debug)]
pub enum SaveError {
    /// Saving was refused because the existing file could not be read. Overwriting it would
    /// destroy settings we failed to understand.
    RefusedUnreadable(String),
    Io(String),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::RefusedUnreadable(why) => write!(
                f, "refusing to overwrite a settings file that could not be read ({why}); \
                    fix or move the file and restart"),
            SaveError::Io(e) => write!(f, "could not write settings: {e}"),
        }
    }
}
impl std::error::Error for SaveError {}

pub struct SettingsStore {
    path: PathBuf,
    settings: Settings,
    /// False only when the file exists and could not be read or parsed.
    writable: bool,
    refusal: Option<String>,
}

impl SettingsStore {
    /// Load from `path`. The shell chooses the path; core does no platform discovery.
    pub fn load(path: impl Into<PathBuf>) -> (Self, LoadOutcome) {
        let path = path.into();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(mut s) => {
                    let repaired = s.normalise();
                    (Self { path, settings: s, writable: true, refusal: None },
                     LoadOutcome::Loaded { repaired })
                }
                Err(e) => {
                    let why = format!("not valid settings JSON: {e}");
                    (Self { path, settings: Settings::default(), writable: false, refusal: Some(why.clone()) },
                     LoadOutcome::Unreadable(why))
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Genuinely absent: defaults are correct and writing is safe.
                (Self { path, settings: Settings::default(), writable: true, refusal: None },
                 LoadOutcome::Missing)
            }
            Err(e) => {
                // Exists but unreadable -- permissions, I/O error, a directory in its place.
                let why = format!("could not be read: {e}");
                (Self { path, settings: Settings::default(), writable: false, refusal: Some(why.clone()) },
                 LoadOutcome::Unreadable(why))
            }
        }
    }

    pub fn get(&self) -> &Settings { &self.settings }
    pub fn path(&self) -> &Path { &self.path }
    /// Whether [`save`](Self::save) can succeed. False means the on-disk file is unreadable.
    pub fn is_writable(&self) -> bool { self.writable }

    /// Replace the in-memory settings. Does not write; call [`save`](Self::save).
    pub fn set(&mut self, mut next: Settings) -> Vec<String> {
        let repaired = next.normalise();
        self.settings = next;
        repaired
    }

    /// Write atomically, 0600, creating the parent directory 0700.
    ///
    /// Atomic because a crash or a full disk partway through a plain write leaves a truncated
    /// file — which on next launch is "unreadable", which now locks saving. Write-then-rename
    /// means the real file is either the old one or the new one.
    pub fn save(&self) -> Result<(), SaveError> {
        if !self.writable {
            return Err(SaveError::RefusedUnreadable(
                self.refusal.clone().unwrap_or_else(|| "unknown reason".into())));
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| SaveError::Io(e.to_string()))?;
            restrict_dir(dir);
        }
        let tmp = self.path.with_extension(format!("tmp{}", std::process::id()));
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp).map_err(|e| SaveError::Io(e.to_string()))?;
            restrict_file(&tmp);
            f.write_all(self.settings.to_json().as_bytes()).map_err(|e| SaveError::Io(e.to_string()))?;
            f.sync_all().map_err(|e| SaveError::Io(e.to_string()))?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            SaveError::Io(e.to_string())
        })?;
        restrict_file(&self.path);
        Ok(())
    }
}

/// Captures are screenshots of whatever was on screen — banking tabs, password managers — and the
/// settings file records where they are kept. Neither is other users' business on a shared Mac.
#[cfg(unix)]
fn restrict_dir(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
}
#[cfg(unix)]
fn restrict_file(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn restrict_dir(_p: &Path) {}
#[cfg(not(unix))]
fn restrict_file(_p: &Path) {}
