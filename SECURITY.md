# Security

BHCapture can see everything on your screen and hear your microphone, so it is built to keep what it
captures on your computer and private.

## Reporting a vulnerability

Please **do not** open a public issue for security problems. Email
**benjamin dot biswas at gmail dot com** (or contact [BiswasHost](https://www.biswashost.com)) with
details and steps to reproduce. You'll get a response as soon as possible.

## How BHCapture protects your captures

- **Nothing leaves your computer.** No account, no upload, no telemetry, no network calls.
- **Nothing is written until you save.** A screenshot stays in memory in the editor; a recording is
  written only to BHCapture's private working folder (owner-only, `0700`) until you save or export it.
- **Saved files are private by default** (`0600` on macOS/Linux): a capture saved to a shared folder is
  not readable by other accounts unless you share it deliberately.
- **Pixelate is baked into the saved pixels.** A redaction is never an overlay that can be removed.
- **BHCapture's own windows are left out of every capture** (the selector, the recording bar).
- **The web view never chooses a path.** Every file location comes from BHCapture's own code or a
  native Save dialog; the editor and player only say "save" or "export".
- **Settings are a private file, never overwritten when unreadable** — a damaged settings file is left
  alone rather than replaced with defaults.
- **macOS:** the app is signed with one persistent certificate, so the Screen Recording permission you
  grant survives updates instead of silently breaking.
