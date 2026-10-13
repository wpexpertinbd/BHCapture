//! Deciding whether a GitHub release is an update, and which installer to offer.
//!
//! The same rules as BHUninstaller / BHNotepad / BHServe:
//!
//! - **Only GitHub.** A download URL must be HTTPS on exactly `github.com` or
//!   `objects.githubusercontent.com` -- an exact match, never a prefix (`github.com.evil.example`),
//!   and anything carrying userinfo (`https://github.com@evil.example/`) is refused outright.
//! - **Numbers, not strings.** `1.10.0` is newer than `1.9.3`.
//! - **A failed check is never "you are up to date".** That is the caller's to keep apart; here a
//!   reply that cannot be read is an error, never `Ok(None)`.
//! - **Nothing is installed behind the user's back.** The shell downloads on request and hands the
//!   installer to the OS; the user approves it there.
//!
//! Pure: the shell does the network and passes the reply in, so all of this is testable offline.

use serde::{Deserialize, Serialize};

/// The repository the check reads. Not a setting: a settable update source is a way to redirect
/// someone's installer.
pub const REPO: &str = "wpexpertinbd/BHCapture";
pub const RELEASES_API: &str = "https://api.github.com/repos/wpexpertinbd/BHCapture/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/wpexpertinbd/BHCapture/releases/latest";

/// Automatic checks at most this often (GitHub allows 60 unauthenticated calls an hour per IP,
/// shared by everyone behind it). A check the user asks for always runs.
pub const AUTO_CHECK_MIN_SECS: i64 = 30 * 60;
/// A menu-bar app runs for days: re-check this often while running (BHServe does the same).
pub const POLL_SECS: u64 = 6 * 60 * 60;

#[derive(Debug, Clone, Deserialize)]
struct Asset {
    name: String,
    #[serde(rename = "browser_download_url")]
    url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Available {
    pub version: String,
    /// The installer for this machine, when the release has one.
    pub asset_name: Option<String>,
    pub asset_url: Option<String>,
    pub asset_size: u64,
    /// Always set (and always on github.com): where to send someone who wants to look first.
    pub page_url: String,
}

/// Numeric, component-wise; a pre-release suffix is ignored; rubbish counts as zero (no update).
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> {
        s.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next().unwrap_or("")
            .split('.').map(|p| p.trim().parse().unwrap_or(0)).collect()
    };
    let (a, b) = (parts(candidate), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y { return x > y; }
    }
    false
}

/// HTTPS on one of GitHub's download hosts, exactly.
pub fn is_trusted_download(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') || authority.contains(':') { return false; }
    ["github.com", "objects.githubusercontent.com"].iter().any(|h| authority.eq_ignore_ascii_case(h))
}

/// Where every official installer lives. An asset URL must start with this: being on GitHub is
/// not enough, since anyone can publish a release in their own repository (audit 2026-10-13).
pub const ASSET_PREFIX: &str = "https://github.com/wpexpertinbd/BHCapture/releases/download/";

/// An installer download: on a trusted host AND under this repository's releases.
pub fn is_official_asset(url: &str) -> bool {
    is_trusted_download(url) && url.starts_with(ASSET_PREFIX) && !url[ASSET_PREFIX.len()..].contains("..")
}

/// Installer suffixes to accept, in order of preference, for `os` (`std::env::consts::OS`).
/// `linux_rpm`: the machine uses rpm rather than dpkg (Fedora, openSUSE), so offer the .rpm.
///
/// ⚠️ Windows: the per-user NSIS `-setup.exe` ONLY, never an `.msi` (decision 31). An MSI makes
/// msiexec elevate and start the app as SYSTEM, which breaks WebView2 (BHNotepad hit exactly that).
/// macOS: the `.pkg` -- it quits the running copy and reopens it; a `.dmg` cannot.
pub fn wanted_suffixes(os: &str, linux_rpm: bool) -> &'static [&'static str] {
    match os {
        "macos" => &[".pkg"],
        "windows" => &["-setup.exe"],
        _ if linux_rpm => &[".rpm"],
        _ => &[".deb"],
    }
}

/// Release files name one architecture several ways: `aarch64` (rpm), `arm64` (deb, exe),
/// `x86_64` (rpm), `amd64` (deb), `x64` (exe). The macOS build is `universal`.
fn arch_matches(name: &str, arch: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n.contains("universal") { return true; }
    match arch {
        "aarch64" | "arm64" => n.contains("aarch64") || n.contains("arm64"),
        _ => n.contains("x86_64") || n.contains("amd64") || n.contains("x64"),
    }
}

/// Read a `releases/latest` reply. `Ok(None)` = nothing newer; `Err` = the reply could not be read
/// (which the caller must never present as "up to date").
pub fn evaluate(body: &str, current: &str, os: &str, arch: &str, linux_rpm: bool) -> Result<Option<Available>, String> {
    let rel: GhRelease = serde_json::from_str(body).map_err(|_| "GitHub's reply was not a release".to_string())?;
    let version = rel.tag_name.trim().trim_start_matches(['v', 'V']).to_string();
    if version.is_empty() { return Err("GitHub's reply named no version".into()); }
    if !is_newer(&version, current) { return Ok(None); }
    let asset = wanted_suffixes(os, linux_rpm).iter().find_map(|suffix| {
        rel.assets.iter().find(|a| {
            a.name.to_ascii_lowercase().ends_with(suffix) && arch_matches(&a.name, arch) && is_official_asset(&a.url)
        })
    });
    // A tampered page link is replaced, not followed.
    let page_url = if rel.html_url.starts_with("https://github.com/") && is_trusted_download(&rel.html_url) {
        rel.html_url.clone()
    } else {
        RELEASES_PAGE.into()
    };
    Ok(Some(Available {
        version,
        asset_name: asset.map(|a| a.name.clone()),
        asset_url: asset.map(|a| a.url.clone()),
        asset_size: asset.map(|a| a.size).unwrap_or(0),
        page_url,
    }))
}

/// A downloaded file's name comes from GitHub, so it is reduced to plain characters before it
/// becomes part of a path.
pub fn safe_file_name(name: &str) -> String {
    let s: String = name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    let s = s.trim_start_matches('.').to_string();
    if s.is_empty() { "BHCapture-update".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REL: &str = r#"{
      "tag_name": "v1.2.0",
      "html_url": "https://github.com/wpexpertinbd/BHCapture/releases/tag/v1.2.0",
      "assets": [
        {"name":"BHCapture-1.2.0-universal.pkg","browser_download_url":"https://github.com/wpexpertinbd/BHCapture/releases/download/v1.2.0/BHCapture-1.2.0-universal.pkg","size":7000000},
        {"name":"BHCapture-1.2.0-universal.dmg","browser_download_url":"https://github.com/wpexpertinbd/BHCapture/releases/download/v1.2.0/BHCapture-1.2.0-universal.dmg","size":9000000},
        {"name":"BHCapture_1.2.0_x64-setup.exe","browser_download_url":"https://github.com/wpexpertinbd/BHCapture/releases/download/v1.2.0/BHCapture_1.2.0_x64-setup.exe","size":2600000},
        {"name":"BHCapture_1.2.0_arm64-setup.exe","browser_download_url":"https://github.com/wpexpertinbd/BHCapture/releases/download/v1.2.0/BHCapture_1.2.0_arm64-setup.exe","size":2200000},
        {"name":"BHCapture_1.2.0_amd64.deb","browser_download_url":"https://github.com/wpexpertinbd/BHCapture/releases/download/v1.2.0/BHCapture_1.2.0_amd64.deb","size":3600000},
        {"name":"BHCapture_1.2.0_arm64.deb","browser_download_url":"https://github.com/wpexpertinbd/BHCapture/releases/download/v1.2.0/BHCapture_1.2.0_arm64.deb","size":3400000},
        {"name":"BHCapture-1.2.0-1.x86_64.rpm","browser_download_url":"https://github.com/wpexpertinbd/BHCapture/releases/download/v1.2.0/BHCapture-1.2.0-1.x86_64.rpm","size":3600000},
        {"name":"BHCapture-1.2.0-1.aarch64.rpm","browser_download_url":"https://github.com/wpexpertinbd/BHCapture/releases/download/v1.2.0/BHCapture-1.2.0-1.aarch64.rpm","size":3400000},
        {"name":"BHCapture_1.2.0_x64_en-US.msi","browser_download_url":"https://github.com/wpexpertinbd/BHCapture/releases/download/v1.2.0/x.msi","size":1}
      ]}"#;

    fn pick(os: &str, arch: &str, rpm: bool) -> String {
        evaluate(REL, "1.0.1", os, arch, rpm).unwrap().unwrap().asset_name.unwrap_or_default()
    }

    #[test]
    fn every_platform_and_cpu_gets_its_own_installer() {
        assert_eq!(pick("macos", "aarch64", false), "BHCapture-1.2.0-universal.pkg");
        assert_eq!(pick("macos", "x86_64", false), "BHCapture-1.2.0-universal.pkg");
        assert_eq!(pick("windows", "x86_64", false), "BHCapture_1.2.0_x64-setup.exe");
        assert_eq!(pick("windows", "aarch64", false), "BHCapture_1.2.0_arm64-setup.exe");
        assert_eq!(pick("linux", "x86_64", false), "BHCapture_1.2.0_amd64.deb");
        assert_eq!(pick("linux", "aarch64", false), "BHCapture_1.2.0_arm64.deb");
        assert_eq!(pick("linux", "x86_64", true), "BHCapture-1.2.0-1.x86_64.rpm");
        assert_eq!(pick("linux", "aarch64", true), "BHCapture-1.2.0-1.aarch64.rpm");
    }

    #[test]
    fn windows_never_gets_an_msi() {
        let only_msi = r#"{"tag_name":"v9.0.0","assets":[{"name":"BHCapture_9_x64_en-US.msi",
            "browser_download_url":"https://github.com/a/b/x64.msi"}]}"#;
        let a = evaluate(only_msi, "1.0.0", "windows", "x86_64", false).unwrap().unwrap();
        assert!(a.asset_url.is_none(), "an MSI must never be offered: {a:?}");
        assert!(wanted_suffixes("windows", false).iter().all(|s| !s.ends_with(".msi")));
    }

    #[test]
    fn same_or_older_is_not_an_update_and_rubbish_is_an_error() {
        assert_eq!(evaluate(REL, "1.2.0", "macos", "aarch64", false).unwrap(), None);
        assert_eq!(evaluate(REL, "1.10.0", "macos", "aarch64", false).unwrap(), None);
        assert!(evaluate("<html>rate limited</html>", "1.0.0", "macos", "aarch64", false).is_err());
        assert!(evaluate(r#"{"message":"Not Found"}"#, "1.0.0", "macos", "aarch64", false).is_err());
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(is_newer("1.10.0", "1.9.3"));
        assert!(is_newer("v1.0.1", "1.0.0"));
        assert!(!is_newer("1.0", "1.0.0"));
        assert!(!is_newer("1.0.0-beta", "1.0.0"));
        assert!(!is_newer("latest", "1.0.0"));
    }

    #[test]
    fn only_exact_github_hosts_over_https() {
        for good in ["https://github.com/a/b.pkg", "https://objects.githubusercontent.com/x", "https://GitHub.com/x"] {
            assert!(is_trusted_download(good), "{good}");
        }
        for bad in ["https://github.com.evil.example/x.pkg", "https://notgithub.com/x", "https://evil.example/github.com/x",
                    "https://github.com@evil.example/x", "http://github.com/x", "https://github.com:8443/x", ""] {
            assert!(!is_trusted_download(bad), "{bad}");
        }
    }

    #[test]
    fn a_tampered_asset_or_page_link_is_not_followed() {
        let evil = r#"{"tag_name":"v9.0.0","html_url":"https://evil.example/r",
            "assets":[{"name":"BHCapture-9-universal.pkg","browser_download_url":"https://github.com.evil.example/x.pkg"}]}"#;
        let a = evaluate(evil, "1.0.0", "macos", "aarch64", false).unwrap().unwrap();
        assert!(a.asset_url.is_none());
        assert_eq!(a.page_url, RELEASES_PAGE);
    }

    #[test]
    fn an_installer_from_another_repository_is_refused() {
        let other = r#"{"tag_name":"v9.0.0","assets":[
            {"name":"BHCapture-9-universal.pkg","browser_download_url":"https://github.com/attacker/x/releases/download/v9/BHCapture-9-universal.pkg"},
            {"name":"BHCapture_9_x64-setup.exe","browser_download_url":"https://objects.githubusercontent.com/github-production-release-asset/1/x.exe"}]}"#;
        assert!(evaluate(other, "1.0.0", "macos", "aarch64", false).unwrap().unwrap().asset_url.is_none());
        assert!(evaluate(other, "1.0.0", "windows", "x86_64", false).unwrap().unwrap().asset_url.is_none());
        assert!(!is_official_asset("https://github.com/wpexpertinbd/BHCapture/releases/download/../../attacker/x.pkg"));
        assert!(is_official_asset("https://github.com/wpexpertinbd/BHCapture/releases/download/v1.0.0/BHCapture-1.0.0-universal.pkg"));
    }

    #[test]
    fn downloaded_names_cannot_escape_their_folder() {
        assert_eq!(safe_file_name("../../x.pkg"), "_.._x.pkg");
        assert!(!safe_file_name("a/b\\c.exe").contains(['/', '\\']));
        assert_eq!(safe_file_name(""), "BHCapture-update");
    }
}
