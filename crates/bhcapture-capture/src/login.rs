//! Open at login (decision 38). Our own entry on every OS -- no plugin -- so we decide exactly what
//! is written and where:
//! - macOS: a per-user LaunchAgent that asks LaunchServices to open the app (`open -a`), the way
//!   BHServe does, so the app starts as an ordinary app launch and only in a graphical session.
//! - Windows: a value under HKCU `...\CurrentVersion\Run`, path quoted. Never HKLM: a machine-wide
//!   entry written by one elevated run could not be removed again by a normal one.
//! - Linux: `~/.config/autostart/com.biswashost.bhcapture.desktop`.
//!
//! The OS's own off switches are left alone: macOS Login Items and Windows Startup apps keep the
//! entry and mark it disabled (we report the macOS case, never override it), and GNOME's Startup
//! Applications edits our .desktop file in place, which is why Linux never reports "stale".

use std::path::{Path, PathBuf};

/// What is registered now, compared with the program that should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Missing,
    /// Present and starting `target`.
    Current,
    /// Present but starting something else (the app was moved).
    Stale,
}

/// Windows' canonical form is `\\?\C:\…`; a Run entry (and Startup apps' listing) wants `C:\…`.
fn plain(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            if rest.as_bytes().get(1) == Some(&b':') { return PathBuf::from(rest); }
        }
    }
    p
}

/// What the entry starts: the `.app` bundle on macOS, the executable elsewhere.
pub fn current_target() -> Option<PathBuf> {
    let exe = plain(std::env::current_exe().ok()?.canonicalize().ok()?);
    if cfg!(target_os = "macos") {
        // `…/X.app/Contents/MacOS/X` -> `…/X.app`
        let app = exe.parent()?.parent()?.parent()?;
        return app.extension().is_some_and(|e| e == "app").then(|| app.to_path_buf());
    }
    Some(exe)
}

/// Only an INSTALLED copy registers. A copy run from a build folder, the opened .dmg or a quarantine
/// mount would leave an entry that starts a stray copy -- or nothing, once that path is gone.
pub fn is_installed_location(target: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        let home_apps = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Applications"));
        let parent = target.parent();
        parent == Some(Path::new("/Applications")) || (home_apps.is_some() && parent == home_apps.as_deref())
    }
    #[cfg(windows)]
    {
        // The installer is per-user (NSIS currentUser): %LOCALAPPDATA%\BHCapture. Program Files too,
        // in case it was installed for everyone.
        let lower = target.to_string_lossy().to_lowercase();
        ["LOCALAPPDATA", "ProgramFiles", "ProgramW6432"].iter()
            .filter_map(std::env::var_os)
            .any(|d| {
                let d = std::fs::canonicalize(&d).map(plain).unwrap_or_else(|_| PathBuf::from(&d));
                lower.starts_with(&d.to_string_lossy().to_lowercase())
            })
            && !lower.contains("\\target\\") && !lower.contains("\\target.noindex\\")
    }
    #[cfg(target_os = "linux")]
    { let s = target.to_string_lossy(); s.starts_with("/usr/") || s.starts_with("/opt/") }
}

#[cfg(target_os = "macos")]
mod os {
    use super::*;

    pub const LABEL: &str = "com.biswashost.bhcapture.login";

    pub fn path() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
    }

    fn xml(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    pub fn text_for(app: &Path) -> String {
        format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/bin/open</string>
    <string>-a</string>
    <string>{}</string>
    <string>--args</string>
    <string>--autostart</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
</dict>
</plist>
"#, xml(&app.display().to_string()))
    }

    pub fn state(target: &Path) -> Entry {
        let Some(p) = path() else { return Entry::Missing };
        match std::fs::read_to_string(&p) {
            Ok(t) if t == text_for(target) => Entry::Current,
            Ok(_) => Entry::Stale,
            // Unreadable is not "missing": never treat it as the user having removed it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && std::fs::symlink_metadata(&p).is_err() => Entry::Missing,
            Err(_) => Entry::Stale,
        }
    }

    pub fn write(target: &Path) -> std::io::Result<()> {
        let p = path().ok_or_else(|| std::io::Error::other("no home folder"))?;
        super::replace_file(&p, text_for(target).as_bytes(), 0o644)
    }

    pub fn remove() -> std::io::Result<()> { super::remove_file(path()) }
}

#[cfg(target_os = "linux")]
mod os {
    use super::*;

    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("autostart/com.biswashost.bhcapture.desktop"))
    }

    /// Exec= quoting per the Desktop Entry spec: double quotes, with `" ` $ \` escaped inside, and
    /// every backslash doubled again for the key-file layer. A newline cannot be expressed: refused.
    fn exec_quote(p: &Path) -> Option<String> {
        let s = p.to_str()?;
        if s.contains(['\n', '\r']) { return None; }
        let mut q = String::from("\"");
        for c in s.chars() {
            match c {
                '"' | '`' | '$' => { q.push_str("\\\\"); q.push(c); }
                '%' => q.push_str("%%"),
                '\\' => q.push_str("\\\\\\\\"),
                _ => q.push(c),
            }
        }
        q.push('"');
        Some(q)
    }

    pub fn text_for(exe: &Path) -> Option<String> {
        Some(format!("[Desktop Entry]\nType=Application\nName=BHCapture\nComment=Screenshots and screen recording\n\
                      Exec={} --autostart\nTerminal=false\nX-GNOME-Autostart-enabled=true\n", exec_quote(exe)?))
    }

    /// Present = current: desktops switch an autostart entry off by EDITING it, so any difference may
    /// be the user's choice, and rewriting it would undo that.
    pub fn state(_target: &Path) -> Entry {
        match path() {
            Some(p) if std::fs::symlink_metadata(&p).is_ok() => Entry::Current,
            _ => Entry::Missing,
        }
    }

    pub fn write(target: &Path) -> std::io::Result<()> {
        let p = path().ok_or_else(|| std::io::Error::other("no home folder"))?;
        let t = text_for(target).ok_or_else(|| std::io::Error::other("the program path cannot be written into an autostart entry"))?;
        super::replace_file(&p, t.as_bytes(), 0o644)
    }

    pub fn remove() -> std::io::Result<()> { super::remove_file(path()) }
}

#[cfg(windows)]
mod os {
    use super::*;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ};

    const RUN: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const NAME: &str = "BHCapture";

    pub fn text_for(exe: &Path) -> String { format!("\"{}\" --autostart", exe.display()) }

    fn read() -> Result<Option<String>, ()> {
        let (key, name) = (HSTRING::from(RUN), HSTRING::from(NAME));
        let mut len: u32 = 0;
        let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, &key, &name, RRF_RT_REG_SZ, None, None, Some(&mut len)) };
        if r == ERROR_FILE_NOT_FOUND { return Ok(None); }
        if r != ERROR_SUCCESS || len == 0 || len > 64 * 1024 { return Err(()); }
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, &key, &name, RRF_RT_REG_SZ, None,
                                      Some(buf.as_mut_ptr().cast()), Some(&mut len)) };
        if r != ERROR_SUCCESS { return Err(()); }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(Some(String::from_utf16_lossy(&buf[..n])))
    }

    pub fn state_exists() -> bool { matches!(read(), Ok(Some(_))) }

    pub fn state(target: &Path) -> Entry {
        match read() {
            Ok(None) => Entry::Missing,
            Ok(Some(v)) if v == text_for(target) => Entry::Current,
            // Unreadable counts as stale (rewritten), never as "the user removed it".
            _ => Entry::Stale,
        }
    }

    pub fn write(target: &Path) -> std::io::Result<()> {
        let v: Vec<u16> = text_for(target).encode_utf16().chain([0]).collect();
        // Creates the value (and the Run key, if a fresh profile lacks it) in one call.
        let r = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(RUN), &HSTRING::from(NAME), REG_SZ.0,
                                         Some(v.as_ptr().cast()), (v.len() * 2) as u32) };
        if r != ERROR_SUCCESS { return Err(std::io::Error::from_raw_os_error(r.0 as i32)); }
        Ok(())
    }

    const APPROVED: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";

    /// Settings › Apps › Startup / Task Manager keep their own on/off per entry name: 12 bytes, the
    /// first one odd (03, 07) when the user switched it off.
    pub fn disabled_by_user() -> bool {
        let mut buf = [0u8; 16];
        let mut len = buf.len() as u32;
        let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, &HSTRING::from(APPROVED), &HSTRING::from(NAME), RRF_RT_REG_BINARY,
                                      None, Some(buf.as_mut_ptr().cast()), Some(&mut len)) };
        r == ERROR_SUCCESS && len >= 1 && buf[0] & 1 == 1
    }

    /// The user has just switched it on in OUR Settings: clear the OS's "disabled" mark too, or the
    /// entry we write stays switched off and our checkbox would say ON while nothing starts.
    pub fn clear_user_disable() {
        let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(APPROVED), &HSTRING::from(NAME)) };
    }

    pub fn remove() -> std::io::Result<()> {
        let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(RUN), &HSTRING::from(NAME)) };
        if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND { Ok(()) } else { Err(std::io::Error::from_raw_os_error(r.0 as i32)) }
    }
}

pub fn state(target: &Path) -> Entry { os::state(target) }
pub fn register(target: &Path) -> std::io::Result<()> { os::write(target) }
pub fn unregister() -> std::io::Result<()> { os::remove() }

/// Called when the user switches it on in our Settings: also lift the OS's own "off" where we may
/// (Windows Startup apps). macOS Login Items can only be changed by the user -- the banner says so.
pub fn user_enabled() {
    #[cfg(windows)]
    os::clear_user_disable();
}

/// Replace, never write through: a symlink planted at our name must not redirect the write.
#[cfg(unix)]
fn replace_file(p: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
    let tmp = p.with_extension("tmp-bhcapture");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.write_all(data)?;
    }
    std::fs::rename(&tmp, p)
}

#[cfg(unix)]
fn remove_file(p: Option<PathBuf>) -> std::io::Result<()> {
    match p.map(std::fs::remove_file) {
        Some(Err(e)) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn bhc_login_needs_approval(plist: *const std::ffi::c_char) -> i32;
    fn bhc_login_open_settings();
}

/// Where the OS is holding our entry back, as the place the user can allow it again, or None.
/// macOS: Login Items switched off, or "disallowed" after an OS upgrade. Windows: Startup apps.
pub fn blocked_where() -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        let p = os::path().filter(|p| p.exists())?;
        let c = std::ffi::CString::new(p.to_string_lossy().as_bytes()).ok()?;
        (unsafe { bhc_login_needs_approval(c.as_ptr()) } == 1).then_some("System Settings › General › Login Items")
    }
    #[cfg(windows)]
    { (os::state_exists() && os::disabled_by_user()).then_some("Settings › Apps › Startup") }
    #[cfg(target_os = "linux")]
    { None }
}

/// Open that place (macOS Login Items, Windows Startup apps).
pub fn open_settings() {
    #[cfg(target_os = "macos")]
    unsafe { bhc_login_open_settings() }
    #[cfg(windows)]
    {
        // explorer.exe is a GUI program: no console window (CREATE_NO_WINDOW is not needed).
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let _ = std::process::Command::new(root.join("explorer.exe")).arg("ms-settings:startupapps").spawn();
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn the_agent_opens_the_app_quietly_and_escapes_its_path() {
        let t = super::os::text_for(std::path::Path::new("/Applications/A & B.app"));
        assert!(t.contains("<string>/Applications/A &amp; B.app</string>"));
        assert!(t.contains("<string>/usr/bin/open</string>") && t.contains("<string>--autostart</string>"));
        assert!(t.contains("<string>com.biswashost.bhcapture.login</string>"));
    }

    #[test]
    fn only_an_installed_copy_registers() {
        use std::path::Path;
        #[cfg(target_os = "macos")]
        {
            assert!(super::is_installed_location(Path::new("/Applications/BHCapture.app")));
            assert!(!super::is_installed_location(Path::new("/Volumes/BHCapture/BHCapture.app")));
            assert!(!super::is_installed_location(Path::new("/Users/x/BHCapture/target.noindex/xmac/universal-apple-darwin/release/bundle/macos/BHCapture.app")));
        }
        #[cfg(target_os = "linux")]
        {
            assert!(super::is_installed_location(Path::new("/usr/bin/bhcapture")));
            assert!(!super::is_installed_location(Path::new("/home/x/BHCapture/target/release/bhcapture")));
        }
    }

    /// Writes into $HOME -- run only under a throwaway HOME:
    /// `HOME=<scratch> cargo test -p bhcapture-capture login -- --ignored`
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn the_entry_is_written_reported_stale_after_a_move_and_removed() {
        use super::{state, register, unregister, Entry};
        assert!(!std::env::var("HOME").unwrap().starts_with("/Users/") && !std::env::var("HOME").unwrap().starts_with("/home/"),
                "use a throwaway HOME");
        let a = std::path::Path::new(if cfg!(target_os = "macos") { "/Applications/BHCapture.app" } else { "/usr/bin/bhcapture" });
        assert_eq!(state(a), Entry::Missing);
        register(a).unwrap();
        assert_eq!(state(a), Entry::Current);
        #[cfg(target_os = "macos")]
        {
            assert_eq!(state(std::path::Path::new("/Users/x/Applications/BHCapture.app")), Entry::Stale);
            assert!(super::blocked_where().is_none(), "an entry launchd never loaded is not 'blocked'");
        }
        unregister().unwrap();
        assert_eq!(state(a), Entry::Missing);
        unregister().unwrap(); // already off is fine
    }
}
