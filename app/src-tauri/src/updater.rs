//! Checking for, downloading and starting a newer BHCapture -- BHServe's model for a menu-bar app
//! (a check at launch and every few hours, a Settings section, an announcement when a new version
//! appears) with BHNotepad/BHUninstaller's guards (GitHub-only downloads into a private folder, only
//! the file this process downloaded is ever started, nothing installs without the user's click).
//!
//! The network goes through the system's own `curl` (macOS, Windows 10 1803+, and a declared
//! package dependency on Linux) rather than an HTTP client compiled in: a TLS stack would add over
//! a megabyte for one small request a few times a day (decision 25: small downloads). Every value
//! reaches curl as its own argument -- no shell is involved.
//!
//! Install: macOS opens the .pkg in Installer, which quits BHCapture and reopens it (same signing
//! identity, so the Screen Recording permission carries over). Windows starts the per-user setup
//! and quits so the files can be replaced. Linux hands the .deb/.rpm to the software installer.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use bhcapture_core::update::{self, Available};
use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::AppState;

#[derive(Clone, Serialize, Default)]
pub(crate) struct UpdateView {
    current: String,
    auto: bool,
    /// "idle" | "checking" | "uptodate" | "available" | "downloading" | "ready" | "failed"
    status: String,
    available: Option<Available>,
    error: Option<String>,
}

static VIEW: Mutex<Option<UpdateView>> = Mutex::new(None);
/// The installer this process downloaded. `start` runs this exact file and no other.
static DOWNLOADED: Mutex<Option<PathBuf>> = Mutex::new(None);
static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn set_view(app: &tauri::AppHandle, f: impl FnOnce(&mut UpdateView)) {
    let v = {
        let mut g = VIEW.lock().unwrap();
        let v = g.get_or_insert_with(|| UpdateView { current: env!("CARGO_PKG_VERSION").into(), status: "idle".into(), ..Default::default() });
        f(v);
        v.clone()
    };
    let _ = app.emit_to(crate::SETTINGS_WINDOW, "update-state", v);
}

fn view(state: &AppState) -> UpdateView {
    let mut v = VIEW.lock().unwrap().clone().unwrap_or_else(|| UpdateView {
        current: env!("CARGO_PKG_VERSION").into(), status: "idle".into(), ..Default::default() });
    v.auto = state.settings.lock().unwrap().get().auto_update_check;
    v
}

/// The system curl, by absolute path (never a `curl` found on PATH).
fn curl() -> std::process::Command {
    #[cfg(windows)]
    let path = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32").join("curl.exe");
    #[cfg(not(windows))]
    let path = PathBuf::from("/usr/bin/curl");
    #[allow(unused_mut)]
    let mut c = std::process::Command::new(path);
    // A console program started by a GUI app on Windows gets its own console WINDOW unless told not to.
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; c.creation_flags(0x0800_0000); }
    // -q FIRST: ignore the user's .curlrc / _curlrc, which could quietly add --insecure or a proxy.
    c.args(["-q", "--silent", "--show-error", "--location", "--proto", "=https", "--proto-redir", "=https",
            "--connect-timeout", "15", "--user-agent", concat!("BHCapture/", env!("CARGO_PKG_VERSION"))]);
    c.stdin(std::process::Stdio::null());
    c
}

/// The version compared against GitHub. Test builds can pretend to be older, to exercise the flow.
fn running_version() -> String {
    #[cfg(feature = "demo")]
    if let Ok(v) = std::env::var("BHC_UPDATE_AS_VERSION") { return v; }
    env!("CARGO_PKG_VERSION").to_string()
}

/// Whether this Linux system installs .rpm rather than .deb.
fn linux_rpm() -> bool {
    cfg!(target_os = "linux") && !std::path::Path::new("/usr/bin/dpkg").exists() && std::path::Path::new("/usr/bin/rpm").exists()
}

/// Ask GitHub. `Ok(None)` = nothing newer; `Err` = the check did not complete (never "up to date").
fn fetch() -> Result<Option<Available>, String> {
    let out = curl().args(["--max-time", "30", "--max-filesize", "2000000", "--header", "Accept: application/vnd.github+json",
                           "--write-out", "\n%{http_code}", update::RELEASES_API])
        .output().map_err(|e| format!("could not run curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("could not reach GitHub ({})", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, ""));
    match code.trim() {
        "200" => update::evaluate(body, &running_version(), std::env::consts::OS, std::env::consts::ARCH, linux_rpm()),
        "404" => Ok(None), // no release published yet: an answer, not a failure
        "403" | "429" => Err("GitHub rate-limited the check -- try again in a few minutes".into()),
        other => Err(format!("GitHub answered HTTP {other}")),
    }
}

/// Run one check. Automatic checks respect the setting and the throttle and stay silent unless
/// they find something; a manual check always runs and always reports.
pub(crate) fn check(app: &tauri::AppHandle, manual: bool) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    if !manual {
        let mut st = state.settings.lock().unwrap();
        let s = st.get().clone();
        if !s.auto_update_check || now_secs() - s.last_update_check < update::AUTO_CHECK_MIN_SECS { return; }
        // Stamped BEFORE asking, so a failure backs off too.
        st.set(bhcapture_core::settings::Settings { last_update_check: now_secs(), ..s });
        let _ = st.save();
    }
    if VIEW.lock().unwrap().as_ref().map_or(false, |v| matches!(v.status.as_str(), "downloading" | "ready")) { return; }
    if manual { set_view(app, |v| { v.status = "checking".into(); v.error = None; }); }
    match fetch() {
        Ok(Some(a)) => {
            println!("[update] {} is available ({:?})", a.version, a.asset_name);
            let version = a.version.clone();
            set_view(app, |v| { v.status = "available".into(); v.available = Some(a); v.error = None; });
            set_tray_label(app, Some(&version));
            if !manual { announce(app, &state, &version); }
        }
        Ok(None) => if manual { set_view(app, |v| { v.status = "uptodate".into(); v.available = None; }) },
        Err(e) => {
            eprintln!("[update] check failed: {e}");
            if manual { set_view(app, |v| { v.status = "failed".into(); v.error = Some(e); }) }
        }
    }
}

/// A new version found in the background is shown ONCE: Settings opens on its Updates section.
fn announce(app: &tauri::AppHandle, state: &AppState, version: &str) {
    {
        let mut st = state.settings.lock().unwrap();
        let s = st.get().clone();
        if s.notified_update == version { return; }
        st.set(bhcapture_core::settings::Settings { notified_update: version.to_string(), ..s });
        let _ = st.save();
    }
    let a = app.clone();
    let _ = app.run_on_main_thread(move || { let _ = crate::open_settings(a); });
}

static TRAY_UPDATE_ITEM: Mutex<Option<crate::MainThreadOnly<tauri::menu::MenuItem<tauri::Wry>>>> = Mutex::new(None);

pub(crate) fn set_tray_item(item: tauri::menu::MenuItem<tauri::Wry>) {
    *TRAY_UPDATE_ITEM.lock().unwrap() = Some(crate::MainThreadOnly(item));
}

fn set_tray_label(app: &tauri::AppHandle, version: Option<&str>) {
    let text = match version { Some(v) => format!("Update Available (v{v})…"), None => "Check for Updates…".into() };
    let _ = app.run_on_main_thread(move || {
        if let Some(it) = TRAY_UPDATE_ITEM.lock().unwrap().as_ref() { let _ = it.0.set_text(text); }
    });
}

/// Launch check (after a short delay, so start-up is not slowed) and a re-check every few hours.
pub(crate) fn start_background(app: &tauri::AppHandle) {
    let a = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(8));
        loop {
            check(&a, false);
            std::thread::sleep(Duration::from_secs(update::POLL_SECS));
        }
    });
}

use std::sync::Arc;

#[tauri::command]
pub(crate) fn update_info(state: tauri::State<'_, Arc<AppState>>) -> UpdateView { view(&state) }

#[tauri::command]
pub(crate) async fn update_check(app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || check(&app, true)).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn update_set_auto(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>, enabled: bool) -> Result<(), String> {
    {
        let mut st = state.settings.lock().unwrap();
        let s = st.get().clone();
        st.set(bhcapture_core::settings::Settings { auto_update_check: enabled, ..s });
        st.save().map_err(|e| e.to_string())?;
    }
    // Turned on: look now (BHServe does the same), still within the throttle.
    if enabled { let a = app.clone(); std::thread::spawn(move || check(&a, false)); }
    Ok(())
}

/// A scratch folder only this process could have created (unpredictable name, `create_dir`
/// refuses anything already there, a symlink included), private to this user.
fn private_temp_dir() -> Result<PathBuf, String> {
    let base = std::env::temp_dir();
    for attempt in 0..8u32 {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let dir = base.join(format!("BHCapture-update-{}-{nanos:08x}{attempt:x}", std::process::id()));
        #[allow(unused_mut)]
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        { use std::os::unix::fs::DirBuilderExt; b.mode(0o700); }
        if b.create(&dir).is_ok() {
            let m = std::fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
            if m.file_type().is_symlink() || !m.is_dir() { return Err("the scratch folder was not what we created".into()); }
            return Ok(dir);
        }
    }
    Err("could not create a scratch folder for the download".into())
}

/// Download the installer for this machine, then start it.
#[tauri::command]
pub(crate) async fn update_install(app: tauri::AppHandle) -> Result<(), String> {
    if BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) { return Err("an update is already being downloaded".into()); }
    let a = app.clone();
    let r = tauri::async_runtime::spawn_blocking(move || install(&a)).await.map_err(|e| e.to_string()).and_then(|r| r);
    BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
    if let Err(e) = &r { set_view(&app, |v| { v.status = "failed".into(); v.error = Some(e.clone()); }); }
    r
}

fn install(app: &tauri::AppHandle) -> Result<(), String> {
    let avail = VIEW.lock().unwrap().as_ref().and_then(|v| v.available.clone()).ok_or("no update is waiting")?;
    let (Some(url), Some(name)) = (avail.asset_url.clone(), avail.asset_name.clone()) else {
        return Err("this release has no installer for this computer -- use What's New to download it".into());
    };
    // Checked again here: this is the call that acts on the URL.
    if !update::is_official_asset(&url) { return Err("that download is not a BHCapture release -- refusing it".into()); }
    set_view(app, |v| { v.status = "downloading".into(); v.error = None; });
    let dir = private_temp_dir()?;
    let path = dir.join(update::safe_file_name(&name));
    let out = curl().args(["--fail", "--max-time", "900", "--max-filesize", "300000000", "--output"]).arg(&path).arg(&url)
        .output().map_err(|e| format!("could not run curl: {e}"))?;
    if !out.status.success() || !path.is_file() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(format!("download failed ({})", String::from_utf8_lossy(&out.stderr).trim()));
    }
    // A short read would hand the user a broken installer.
    if avail.asset_size > 0 && std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) != avail.asset_size {
        let _ = std::fs::remove_dir_all(&dir);
        return Err("the download was incomplete -- try again".into());
    }
    *DOWNLOADED.lock().unwrap() = Some(path.clone());
    set_view(app, |v| v.status = "ready".into());
    start(app, &path)?;
    // macOS/Linux keep running while the installer is open; if it is cancelled, the update must still
    // be offered (and checks must run again), so go back to "available" after a moment.
    #[cfg(not(windows))]
    {
        let a = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            set_view(&a, |v| if v.status == "ready" { v.status = "available".into() });
        });
    }
    Ok(())
}

/// Hand the downloaded installer to the OS. Only ever the exact file `install` just saved.
fn start(app: &tauri::AppHandle, path: &std::path::Path) -> Result<(), String> {
    if DOWNLOADED.lock().unwrap().as_deref() != Some(path) {
        return Err("that file was not downloaded by BHCapture -- refusing to run it".into());
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if !matches!(ext.as_str(), "pkg" | "exe" | "deb" | "rpm") { return Err(format!("{ext} is not an installer")); }
    println!("[update] starting {}", path.display());
    #[cfg(feature = "demo")]
    if std::env::var_os("BHC_UPDATE_NO_START").is_some() { println!("[update] test build: not starting it"); return Ok(()); }
    #[cfg(target_os = "macos")]
    {
        // Installer asks for the password; the package's own scripts quit BHCapture and reopen it.
        // BHCapture keeps running meanwhile, so a cancelled install leaves it as it was.
        std::process::Command::new("/usr/bin/open").arg(path).spawn().map_err(|e| format!("could not open the installer: {e}"))?;
        let _ = app;
    }
    #[cfg(windows)]
    {
        // The per-user setup replaces the files, so BHCapture must be gone: quit a moment after it
        // appears (the setup offers to start the new version when it finishes).
        std::process::Command::new(path).spawn().map_err(|e| format!("could not start the installer: {e}"))?;
        let a = app.clone();
        std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(1500)); a.exit(0); });
    }
    #[cfg(target_os = "linux")]
    {
        // The software installer takes it from here; restart BHCapture after it finishes.
        std::process::Command::new("/usr/bin/xdg-open").arg(path).spawn().map_err(|e| format!("could not open the installer: {e}"))?;
        let _ = app;
    }
    Ok(())
}

/// What's New: the release page, on github.com only.
#[tauri::command]
pub(crate) fn update_open_page() -> Result<(), String> {
    let url = VIEW.lock().unwrap().as_ref().and_then(|v| v.available.as_ref().map(|a| a.page_url.clone()))
        .unwrap_or_else(|| update::RELEASES_PAGE.to_string());
    if !url.starts_with("https://github.com/") || !update::is_trusted_download(&url) { return Err("that link is not on github.com".into()); }
    open_url(&url)
}

fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("/usr/bin/open").arg(url).spawn();
    #[cfg(windows)]
    let r = {
        use std::os::windows::process::CommandExt;
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        // rundll32 url.dll opens a URL in the default browser without a shell parsing it.
        std::process::Command::new(root.join("System32").join("rundll32.exe"))
            .args(["url.dll,FileProtocolHandler", url]).creation_flags(0x0800_0000).spawn()
    };
    #[cfg(target_os = "linux")]
    let r = std::process::Command::new("/usr/bin/xdg-open").arg(url).spawn();
    r.map(|_| ()).map_err(|e| format!("could not open the browser: {e}"))
}
