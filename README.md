<h1 align="center">BHCapture</h1>

<p align="center">
  A free, open-source <b>screenshot and screen recorder</b> for <b>macOS, Windows, and Linux</b>.<br>
  A Snagit/Camtasia-style capture tool — select any area, annotate it, record it with your voice,<br>
  and save it in the format you need. Small download, no account, nothing uploaded.
</p>

<p align="center"><i>Built and maintained by <a href="https://www.biswashost.com">BiswasHost</a> 🇧🇩</i></p>

---

## 📦 Platforms

| OS | Download | Notes |
|----|----------|-------|
| 🍎 **macOS** 14+ | `.pkg` / `.dmg` | Signed with a BiswasHost certificate, not notarised — see [first launch](#-first-launch-on-macos) |
| 🪟 **Windows** 10 (2004) / 11 | `-setup.exe` | Installer only |
| 🐧 **Linux** | `.deb` / `.rpm` | X11 and Wayland |

Everything is on the [**latest release**](https://github.com/wpexpertinbd/BHCapture/releases/latest).

## ✨ Features

**Capture**
- **All-in-One** — one shortcut opens the selector with an **Image ⇄ Video** switch (Tab flips it).
- **Any size** — drag any area, click a window to snap to it, or **Full screen** with one key (F).
- A magnifier for pixel-exact edges, and the arrow keys to nudge them.

**Editor**
- Arrow, double arrow, line, circle, box, text, highlight, **pixelate** (baked into the saved pixels —
  never just covered) and numbered steps.
- Save as **JPEG** (default), PNG, WebP, TIFF, BMP or GIF. Save, Save As, copy to the clipboard.
- Nothing is saved until you save it.

**Recording**
- MP4 using your system's own hardware encoder — nothing bundled, so the download stays small.
- **System sound and microphone**, mixed into one track that every player and upload site plays.
- **Noise filter** (RNNoise), **voice clarity** and **automatic level** for the microphone — clear voice
  from a laptop mic, no fan hum, normal loudness.
- **Click highlight**, countdown, pause/resume, restart, and a control bar you can hide.
- Export as **MP4 (H.264)**, **MP4 (HEVC)** for smaller files, **MOV** (macOS), or **animated GIF**.
- A recording interrupted by a crash or quit is kept and reopened at the next launch.

**Everywhere**
- Every shortcut can be changed — click the box in Settings and press your keys.
- Menu-bar / tray app: no Dock icon until you open a window.

## ⌨️ Default shortcuts

| | macOS | Windows / Linux |
|---|---|---|
| All-in-One | ⌃⌘3 | Print Screen |
| Screenshot | ⌃⌘4 | Ctrl+Shift+4 |
| Record video | ⌃⌘5 | Ctrl+Shift+5 |
| Pause / resume a recording | ⌃⌘6 | Ctrl+Shift+6 |

On Windows 11, Print Screen opens Snipping Tool by default. To give it to BHCapture, turn off
*Settings › Accessibility › Keyboard › "Use the Print screen key to open screen capture"*.

## 🍎 First launch on macOS

The app is signed, but **not notarised** — that needs a paid Apple account, and this is free software.
Open it, click **Done** on the warning, then go to **System Settings › Privacy & Security** and click
**Open Anyway**. You only do this once.

macOS then asks for **Screen & System Audio Recording** permission (and **Microphone** the first time
a recording uses it). After allowing Screen Recording, quit and reopen BHCapture — a running app never
sees a new grant.

## 🛠 How it works

- **[`crates/bhcapture-core`](crates/bhcapture-core)** — everything portable: geometry, file naming,
  settings, shortcuts, image formats, GIF, and the microphone chain (noise filter, voice EQ, auto level,
  resampler). No UI and no platform code. Most of the tests live here.
- **[`crates/bhcapture-capture`](crates/bhcapture-capture)** — one capture/record interface with a
  backend per OS: ScreenCaptureKit + AVFoundation on macOS, GDI + Media Foundation + WASAPI on Windows.
- **[`app/`](app)** — the Tauri shell (selector overlays, tray, shortcuts, recording control) and a
  React front end (selector, editor, recording bar, player, settings).

## 📁 Repository layout
```
.
├── crates/bhcapture-core/     # portable logic + tests
├── crates/bhcapture-capture/  # capture and recording, one backend per OS
├── app/                       # Tauri shell (Rust) + React front end
├── scripts/                   # build helpers (icon, Windows cross-build)
├── LICENSE                    # MIT
├── SECURITY.md
└── DISCLAIMER.md              # not affiliated with Snagit / Camtasia
```

## 📄 License
MIT — see [`LICENSE`](LICENSE). Not affiliated with, endorsed by, or derived from Snagit, Camtasia or
TechSmith — see [`DISCLAIMER.md`](DISCLAIMER.md).

## ☕ Support

This project is free and open-source. If it made your work easier, you can **buy me a coffee** — it
genuinely helps me keep building and maintaining free tools like this. 🙏

- **bKash/Nagad** (Personal · *Send Money*): **`01710378396`**

ধন্যবাদ! / Thank you!

<p align="center">Made with care by <a href="https://www.biswashost.com">BiswasHost</a> 🇧🇩</p>
