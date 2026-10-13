import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

type Theme = "system" | "light" | "dark";

type SettingsData = {
  output_dir: string | null;
  pattern: string;
  always_ask_where_to_save: boolean;
  copy_to_clipboard: boolean;
  include_cursor: boolean;
  region_hotkey: string;
  record_hotkey: string;
  record_cursor: boolean;
  record_system_audio: boolean;
  record_microphone: boolean;
  record_noise_filter: boolean;
  record_auto_level: boolean;
  record_voice_clarity: boolean;
  record_countdown: boolean;
  record_fps: number;
  theme: Theme;
  image_format: "jpeg" | "png" | "webp" | "tiff" | "bmp" | "gif";
  jpeg_quality: number;
};

type SettingsView = {
  settings: SettingsData;
  default_dir: string;
  path: string;
  /** False when the on-disk file could not be read; saving is refused. */
  writable: boolean;
  notice: string | null;
};

const TOKENS = ["year", "month", "day", "hour", "minute", "second", "app", "title", "width", "height"];

/** Mirrors the pattern renderer closely enough to preview; the file name is Rust's decision. */
function previewName(pattern: string): string {
  const n = new Date();
  const p2 = (v: number) => String(v).padStart(2, "0");
  const map: Record<string, string> = {
    year: String(n.getFullYear()), month: p2(n.getMonth() + 1), day: p2(n.getDate()),
    hour: p2(n.getHours()), minute: p2(n.getMinutes()), second: p2(n.getSeconds()),
    app: "", title: "", width: "1280", height: "840",
  };
  const out = pattern.replace(/\{(\w+)\}/g, (m, k) => (k in map ? map[k] : m));
  // Same sanitising shape as core, so the preview cannot promise a name Rust would reject.
  return out.replace(/[/\\:*?"<>|]/g, "-").replace(/\s+/g, " ").trim().replace(/^[. ]+|[. ]+$/g, "") || "BHCapture";
}

export function Settings() {
  const [view, setView] = useState<SettingsView | null>(null);
  const [draft, setDraft] = useState<SettingsData | null>(null);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(() => {
    invoke<SettingsView>("settings_get").then((v) => { setView(v); setDraft(v.settings); });
  }, []);
  useEffect(load, [load]);

  // The titlebar belongs to the OS and is set from Rust; this handles the page tokens.
  useEffect(() => {
    if (draft) document.documentElement.setAttribute("data-theme", draft.theme);
  }, [draft?.theme]);

  if (!view || !draft) return <div className="settings" data-theme="system" />;

  const set = <K extends keyof SettingsData>(k: K, v: SettingsData[K]) => {
    setDraft({ ...draft, [k]: v });
    setSaved(false);
    setError(null);
  };

  const save = () => {
    invoke<SettingsView>("settings_save", { next: draft })
      .then((v) => { setView(v); setDraft(v.settings); setSaved(true); setError(null); })
      .catch((e) => { setError(String(e)); setSaved(false); });
  };

  const pickDir = () => {
    invoke<string | null>("settings_pick_dir").then((d) => { if (d) set("output_dir", d); });
  };

  const patternBad = !/^[^{]*(\{(\w+)\}[^{]*)*$/.test(draft.pattern)
    || (draft.pattern.match(/\{(\w+)\}/g) ?? []).some((t) => !TOKENS.includes(t.slice(1, -1)));

  return (
    <div className="settings" data-theme={draft.theme}>
      <h1>BHCapture Settings</h1>
      <p className="sub">A capture opens in the editor; it is saved only when you save it there.</p>

      {!view.writable && (
        <div className="banner">
          <b>Settings cannot be saved</b>
          The settings file exists but could not be read, so BHCapture is running on defaults and
          will not overwrite it — that would destroy whatever is in there. Fix or move the file
          below, then restart.
        </div>
      )}
      {view.notice && <div className="banner"><b>Some values were adjusted</b>{view.notice}</div>}
      {error && <div className="banner"><b>Could not save</b>{error}</div>}

      <div className="row">
        <label className="title" htmlFor="dir">Save captures to</label>
        <div className="with-button">
          <input id="dir" type="text" value={draft.output_dir ?? ""} placeholder={view.default_dir}
                 onChange={(e) => set("output_dir", e.target.value.trim() === "" ? null : e.target.value)} />
          <button onClick={pickDir}>Choose…</button>
        </div>
        <div className="help">
          Leave empty for the default, <code>{view.default_dir}</code>. Must be a full path.
          Any single capture can go elsewhere with <b>Save As</b> (<code>⇧⌘S</code>) in the editor.
        </div>
      </div>

      <div className="row">
        <label className="title" htmlFor="pat">File name</label>
        <input id="pat" type="text" value={draft.pattern}
               onChange={(e) => set("pattern", e.target.value)} />
        <div className="preview">{previewName(draft.pattern)}.{({ jpeg: "jpg" } as Record<string, string>)[draft.image_format] ?? draft.image_format}</div>
        <div className="help">
          Tokens: {TOKENS.map((t) => <code key={t}>{`{${t}}`}</code>)}.
          {patternBad && " — this pattern has an unknown token and will be reset on save."}
          {" "}Repeats in the same second get “ (2)”, never an overwrite.
        </div>
      </div>

      <div className="row">
        <label className="check">
          <input type="checkbox" checked={draft.always_ask_where_to_save}
                 onChange={(e) => set("always_ask_where_to_save", e.target.checked)} />
          <span><span className="t">Always ask where to save</span>
            <span className="d">Opens a Save dialog for every capture instead of writing straight to the folder above.</span></span>
        </label>
        <label className="check">
          <input type="checkbox" checked={draft.copy_to_clipboard}
                 onChange={(e) => set("copy_to_clipboard", e.target.checked)} />
          <span><span className="t">Also copy to the clipboard</span>
            <span className="d">The file is still written either way.</span></span>
        </label>
        <label className="check">
          <input type="checkbox" checked={draft.record_voice_clarity}
                 onChange={(e) => set("record_voice_clarity", e.target.checked)} />
          <span><span className="t">Voice clarity (recordings)</span>
            <span className="d">Takes the boom out of a laptop microphone and brings out the consonants, so a voice sounds natural instead of deep and muffled.</span></span>
        </label>
        <label className="check">
          <input type="checkbox" checked={draft.record_auto_level}
                 onChange={(e) => set("record_auto_level", e.target.checked)} />
          <span><span className="t">Automatic microphone level (recordings)</span>
            <span className="d">Brings a quiet voice up to a normal loudness, so recordings play back at normal volume. The pauses between words are never boosted.</span></span>
        </label>
        <label className="check">
          <input type="checkbox" checked={draft.include_cursor}
                 onChange={(e) => set("include_cursor", e.target.checked)} />
          <span><span className="t">Include the mouse pointer</span>
            <span className="d">Off by default — most screenshots are of content, not of where the pointer was.</span></span>
        </label>
      </div>

      <div className="row">
        <label className="title" htmlFor="fmt">Default file format</label>
        <div className="with-button">
          <select id="fmt" value={draft.image_format}
                  onChange={(e) => set("image_format", e.target.value as SettingsData["image_format"])}>
            <option value="jpeg">JPEG — smallest files (default)</option>
            <option value="png">PNG — exact pixels, larger</option>
            <option value="webp">WebP — exact pixels, smaller than PNG</option>
            <option value="tiff">TIFF</option>
            <option value="bmp">BMP</option>
            <option value="gif">GIF</option>
          </select>
          <input type="text" inputMode="numeric" style={{ maxWidth: 70 }} value={String(draft.jpeg_quality)}
                 title="JPEG quality, 1-100"
                 onChange={(e) => set("jpeg_quality", Number(e.target.value.replace(/[^0-9]/g, "")) || 0)} />
        </div>
        <div className="help">The editor starts on this format; you can pick another per capture. The number is JPEG quality (1-100, default 90). JPEG has no transparency, so transparent areas become white.</div>
      </div>

      <div className="row">
        <label className="title" htmlFor="hk">Region capture shortcut</label>
        <input id="hk" type="text" value={draft.region_hotkey}
               onChange={(e) => set("region_hotkey", e.target.value)} />
        <div className="help">
          Takes effect as soon as you save. Shortcuts macOS already uses (like <code>⇧⌘4</code>
          or <code>⌃⇧⌘4</code>) are refused because both would fire, and so is anything with
          Option, which macOS silently blocks. Type it as <code>Ctrl+Cmd+4</code> or <code>⌃⌘4</code>.
        </div>
      </div>

      <div className="row">
        <label className="title" htmlFor="rhk">Record video shortcut</label>
        <input id="rhk" type="text" value={draft.record_hotkey}
               onChange={(e) => set("record_hotkey", e.target.value)} />
        <div className="help">
          Opens the selector for a recording. Pressed again it starts the recording, and once more it
          stops it. Same rules as above; it must differ from the capture shortcut.
        </div>
      </div>

      <div className="row">
        <label className="title" htmlFor="th">Appearance</label>
        <select id="th" value={draft.theme} onChange={(e) => set("theme", e.target.value as Theme)}>
          <option value="system">Match system</option>
          <option value="light">Light</option>
          <option value="dark">Dark</option>
        </select>
      </div>

      <div className="footer">
        <span className="path">{view.path}</span>
        {saved && <span className="saved">Saved</span>}
        <button className="primary" onClick={save} disabled={!view.writable}>Save</button>
      </div>
    </div>
  );
}
