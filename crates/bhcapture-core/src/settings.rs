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

use crate::export::{ImageFormat, DEFAULT_JPEG_QUALITY};
use crate::hotkey::{Chord, DEFAULT_CAPTURE_HOTKEY, DEFAULT_RECORD_HOTKEY, DEFAULT_REGION_HOTKEY, PAUSE_HOTKEY};
use crate::naming::{Pattern, DEFAULT_PATTERN};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// What the All-in-One selector does on Enter until the user flips its switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMode {
    #[default]
    Image,
    Video,
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
    /// Format a capture is saved in unless the user picks another in the editor or types an
    /// extension in Save As.
    pub image_format: ImageFormat,
    /// 1-100; only JPEG uses it.
    pub jpeg_quality: u8,
    /// Shortcut that starts a screen recording (selector first, then the control bar).
    pub record_hotkey: String,
    /// All-in-One: the selector with an Image ⇄ Video switch (decision 27).
    pub capture_hotkey: String,
    /// Pause/resume while recording. Registered only while a recording runs.
    pub pause_hotkey: String,
    /// Which mode All-in-One starts in. Screenshot unless the user chooses otherwise.
    pub capture_default_mode: CaptureMode,
    /// Recording options, remembered from the control bar's toggles.
    pub record_cursor: bool,
    pub record_system_audio: bool,
    /// On by default (Benjamin, 2026-10-13). The first recording raises macOS's Microphone
    /// permission prompt; on macOS 14 the shell ignores it (needs 15).
    pub record_microphone: bool,
    /// RNNoise on the microphone (fans, hum, hiss). On by default: it is why it was added.
    pub record_noise_filter: bool,
    /// Bring a quiet microphone voice up to a standard level (see `bhcapture_core::level`).
    pub record_auto_level: bool,
    /// Voice EQ on the microphone: less boom, more clarity (see `bhcapture_core::voice_eq`).
    pub record_voice_clarity: bool,
    /// Draw a ring where the mouse clicks, into the video.
    pub record_click_highlight: bool,
    pub record_countdown: bool,
    /// 1-60.
    pub record_fps: u32,
    /// Camera bubble in recordings (decision 34). Off until the user turns it on.
    pub camera_enabled: bool,
    /// Camera id from the OS ("" = the default camera).
    pub camera_device: String,
    /// "circle" | "rounded" | "square" | "wide" (16:9).
    pub camera_shape: String,
    /// Bubble height in points, 120-480.
    pub camera_size: u32,
    /// Starting corner: "bottom-right" | "bottom-left" | "top-right" | "top-left".
    pub camera_corner: String,
    pub camera_mirror: bool,
    /// Auto light + a little colour (bhcapture_core::camera).
    pub camera_enhance: bool,
    /// "none" | "blur" | "color" | "preset:<name>" | "image" (decision 35).
    pub camera_background: String,
    /// "#rrggbb" for the "color" background.
    pub camera_bg_color: String,
    /// The user's own background picture, for "image".
    pub camera_bg_image: Option<PathBuf>,
    /// Look for a newer release at launch and every few hours (BHServe's model). On by default.
    pub auto_update_check: bool,
    /// Unix seconds of the last AUTOMATIC check, so restarts do not each ask GitHub.
    pub last_update_check: i64,
    /// The newest version the user has already been told about, so one release announces itself
    /// once rather than at every launch.
    pub notified_update: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output_dir: None,
            pattern: DEFAULT_PATTERN.to_string(),
            always_ask_where_to_save: false,
            // Off: the clipboard is readable by every app and by clipboard-history tools, and a
            // capture can hold anything that was on screen. Opt-in (Benjamin, 2026-10-13).
            copy_to_clipboard: false,
            include_cursor: false,
            region_hotkey: DEFAULT_REGION_HOTKEY.to_string(),
            theme: Theme::System,
            // JPEG by default: much smaller files for screenshots (Benjamin, 2026-10-13).
            image_format: ImageFormat::Jpeg,
            jpeg_quality: DEFAULT_JPEG_QUALITY,
            record_hotkey: DEFAULT_RECORD_HOTKEY.to_string(),
            capture_hotkey: DEFAULT_CAPTURE_HOTKEY.to_string(),
            pause_hotkey: PAUSE_HOTKEY.to_string(),
            capture_default_mode: CaptureMode::Image,
            record_cursor: true,
            record_system_audio: true,
            record_microphone: true,
            record_noise_filter: true,
            record_auto_level: true,
            record_voice_clarity: true,
            record_click_highlight: true,
            record_countdown: true,
            record_fps: 30,
            camera_enabled: false,
            camera_device: String::new(),
            camera_shape: "circle".into(),
            camera_size: 200,
            camera_corner: "bottom-right".into(),
            camera_mirror: true,
            camera_enhance: true,
            camera_background: "none".into(),
            camera_bg_color: "#1f6feb".into(),
            camera_bg_image: None,
            auto_update_check: true,
            last_update_check: 0,
            notified_update: String::new(),
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
        if !(1..=100).contains(&self.jpeg_quality) {
            fixed.push(format!("JPEG quality {} is outside 1-100; reset to {DEFAULT_JPEG_QUALITY}", self.jpeg_quality));
            self.jpeg_quality = DEFAULT_JPEG_QUALITY;
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
        for (field, name, default) in [("record", "record shortcut", DEFAULT_RECORD_HOTKEY),
                                       ("capture", "All-in-One shortcut", DEFAULT_CAPTURE_HOTKEY),
                                       ("pause", "pause shortcut", PAUSE_HOTKEY)] {
            let slot = match field { "record" => &mut self.record_hotkey, "capture" => &mut self.capture_hotkey, _ => &mut self.pause_hotkey };
            match Chord::parse(slot) {
                Ok(c) => *slot = c.display(),
                Err(e) => {
                    fixed.push(format!("{name} {:?} was not used: {e}. Reset to {default}", slot));
                    *slot = Chord::parse(default).map(|c| c.display()).unwrap_or_else(|_| default.to_string());
                }
            }
        }
        // One chord cannot start two things. Earlier in this list keeps it; a later one that repeats
        // it goes back to its own default (and is noted), so a typo never silently disables a shortcut.
        let order = ["capture", "region", "record", "pause"];
        for i in 1..order.len() {
            let taken: Vec<String> = order[..i].iter().map(|k| self.hotkey(k).to_string()).collect();
            if taken.contains(&self.hotkey(order[i]).to_string()) {
                let def = Chord::parse(Self::default_hotkey(order[i])).map(|c| c.display()).unwrap_or_default();
                fixed.push(format!("{} {} is already used by another BHCapture shortcut; reset to {def}",
                    Self::hotkey_name(order[i]), self.hotkey(order[i])));
                self.set_hotkey(order[i], def);
            }
        }
        if !(1..=60).contains(&self.record_fps) {
            fixed.push(format!("recording frame rate {} is outside 1-60; reset to 30", self.record_fps));
            self.record_fps = 30;
        }
        if !["circle", "rounded", "square", "wide"].contains(&self.camera_shape.as_str()) {
            fixed.push(format!("camera shape {:?} is unknown; reset to circle", self.camera_shape));
            self.camera_shape = "circle".into();
        }
        if !["bottom-right", "bottom-left", "top-right", "top-left"].contains(&self.camera_corner.as_str()) {
            fixed.push(format!("camera position {:?} is unknown; reset to bottom-right", self.camera_corner));
            self.camera_corner = "bottom-right".into();
        }
        if !(120..=480).contains(&self.camera_size) {
            let v = self.camera_size.clamp(120, 480);
            fixed.push(format!("camera size {} is outside 120-480; set to {v}", self.camera_size));
            self.camera_size = v;
        }
        let bg_ok = ["none", "blur", "color", "image"].contains(&self.camera_background.as_str())
            || self.camera_background.strip_prefix("preset:").map_or(false, |p| crate::camera::PRESETS.iter().any(|(id, _)| *id == p));
        if !bg_ok {
            fixed.push(format!("camera background {:?} is unknown; reset to none", self.camera_background));
            self.camera_background = "none".into();
        }
        if crate::camera::parse_hex_color(&self.camera_bg_color).is_none() {
            fixed.push(format!("camera background colour {:?} is not #rrggbb; reset", self.camera_bg_color));
            self.camera_bg_color = "#1f6feb".into();
        }
        if self.camera_bg_image.as_ref().map_or(false, |p| !p.is_absolute()) {
            fixed.push("camera background picture must be a full path; cleared".into());
            self.camera_bg_image = None;
        }
        fixed
    }

    fn hotkey(&self, which: &str) -> &str {
        match which { "capture" => &self.capture_hotkey, "region" => &self.region_hotkey, "record" => &self.record_hotkey, _ => &self.pause_hotkey }
    }
    fn set_hotkey(&mut self, which: &str, v: String) {
        match which { "capture" => self.capture_hotkey = v, "region" => self.region_hotkey = v, "record" => self.record_hotkey = v, _ => self.pause_hotkey = v }
    }
    fn default_hotkey(which: &str) -> &'static str {
        match which { "capture" => DEFAULT_CAPTURE_HOTKEY, "region" => DEFAULT_REGION_HOTKEY, "record" => DEFAULT_RECORD_HOTKEY, _ => PAUSE_HOTKEY }
    }
    fn hotkey_name(which: &str) -> &'static str {
        match which { "capture" => "All-in-One shortcut", "region" => "screenshot shortcut", "record" => "record shortcut", _ => "pause shortcut" }
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
