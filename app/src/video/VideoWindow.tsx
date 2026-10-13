import { useCallback, useEffect, useRef, useState } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "../events";
import { getCurrentWindow } from "@tauri-apps/api/window";

type Info = { file: string; saved_to: string | null; note: string | null };
type Failure = { kind: string; message: string };

/**
 * A finished recording: play it, then File › Save (⌘S) / Save As (⇧⌘S), or close. Like the editor,
 * closing an unsaved recording asks first -- with our own dialog, since window.confirm in a Tauri
 * web view returns true without showing anything.
 */
export function VideoWindow() {
  const [info, setInfo] = useState<Info | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const [asking, setAsking] = useState(false);
  const [size, setSize] = useState<{ w: number; h: number; d: number } | null>(null);
  const [exporting, setExporting] = useState<{ kind: string; percent: number } | null>(null);
  const saving = useRef(false);
  const infoRef = useRef<Info | null>(null);
  infoRef.current = info;

  useEffect(() => {
    invoke<Info>("video_info").then(setInfo).catch((e) => setError(String(e)));
  }, []);

  const save = useCallback(async (saveAs: boolean): Promise<boolean> => {
    if (saving.current) return false;
    saving.current = true;
    try {
      const path = await invoke<string>("video_save", { saveAs });
      setInfo((i) => (i ? { ...i, saved_to: path } : i));
      setStatus(`Saved to ${path}`);
      return true;
    } catch (e) {
      const f = e as Failure;
      if (f && f.kind === "cancelled") return false;
      setStatus(f && f.message ? f.message : String(e));
      return false;
    } finally {
      saving.current = false;
    }
  }, []);

  const NAMES: Record<string, string> = { mp4: "MP4", hevc: "MP4 (HEVC)", mov: "MOV", gif: "GIF" };
  const exportAs = useCallback(async (kind: string) => {
    if (saving.current) return;
    saving.current = true;
    setExporting({ kind, percent: -1 }); // -1 = the Save dialog is still open
    try {
      const path = await invoke<string>("video_export", { kind });
      setInfo((i) => (i ? { ...i, saved_to: path } : i));
      setStatus(`Exported ${NAMES[kind] ?? kind} to ${path}`);
    } catch (e) {
      const f = e as Failure;
      if (!(f && f.kind === "cancelled")) setStatus(f && f.message ? f.message : String(e));
    } finally {
      saving.current = false;
      setExporting(null);
    }
  }, []);

  useEffect(() => {
    const un = listen<{ percent: number; kind: string }>("video-export-progress", (e) => setExporting(e.payload));
    return () => { un.then((f) => f()); };
  }, []);

  const close = useCallback(() => {
    if (infoRef.current && !infoRef.current.saved_to) { setAsking(true); return; }
    void invoke("video_close");
  }, []);

  const menu = useRef<(a: string) => void>(() => {});
  menu.current = (a: string) => {
    if (asking) return;
    if (a === "save") void save(false);
    else if (a === "save-as") void save(true);
    else if (a === "close") close();
    else if (a.startsWith("export:")) void exportAs(a.slice(7));
  };
  useEffect(() => {
    const a = listen<string>("editor-menu", (e) => menu.current(e.payload));
    const b = listen("editor-close-requested", () => close());
    return () => { a.then((f) => f()); b.then((f) => f()); };
  }, [close]);

  // The close dialog's keys: Return saves, Esc cancels, ⌘D discards.
  useEffect(() => {
    if (!asking) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); setAsking(false); }
      else if (e.key === "Enter") { e.preventDefault(); void save(false).then((ok) => { if (ok) void invoke("video_close"); }); }
      else if (e.key.toLowerCase() === "d" && e.metaKey) { e.preventDefault(); void invoke("video_close"); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [asking, save]);

  useEffect(() => { void getCurrentWindow().setTitle(info?.saved_to ? `BHCapture — ${info.saved_to.split(/[\\/]/).pop()}` : "BHCapture — Recording"); }, [info]);

  if (error) return <div className="video"><div className="vd-error">{error}</div></div>;
  if (!info) return <div className="video" />;

  return (
    <div className="video">
      {info.note && <div className="vd-note">{info.note}</div>}
      <div className="vd-stage">
        <video className="vd-player" src={convertFileSrc(info.file)} controls autoPlay={false}
               onLoadedMetadata={(e) => {
                 const v = e.currentTarget;
                 setSize({ w: v.videoWidth, h: v.videoHeight, d: v.duration });
               }}
               onError={() => setStatus("This recording could not be played here. It can still be saved.")} />
      </div>
      <div className="vd-status">
        <span>{size ? `${size.w} × ${size.h} px · ${isFinite(size.d) ? size.d.toFixed(1) + " s" : ""} · MP4` : "MP4"}</span>
        <span className="vd-msg">{exporting
          ? (exporting.percent < 0 ? "Choose where to save…" : `Exporting ${NAMES[exporting.kind] ?? exporting.kind}… ${exporting.percent}%`)
          : (status ?? (info.saved_to ? `Saved to ${info.saved_to}` : "Not saved yet"))}</span>
        <span className="vd-hint">File › Save ⌘S · Export Recording As (MP4, HEVC, MOV, GIF ⇧⌘E) · Close ⌘W</span>
      </div>
      {asking && (
        <div className="ed-modal-backdrop">
          <div className="ed-modal vd-modal" role="dialog" aria-modal="true">
            <div className="ed-modal-title">Save this recording before closing?</div>
            <div className="ed-modal-text">It has not been saved anywhere. If you don't save it, it is gone.</div>
            <div className="ed-modal-actions">
              <button className="ed-btn" onClick={() => void invoke("video_close")}>Don't Save <kbd>⌘D</kbd></button>
              <span className="ed-spacer" />
              <button className="ed-btn" onClick={() => setAsking(false)}>Cancel <kbd>Esc</kbd></button>
              <button className="ed-btn primary"
                      onClick={() => void save(false).then((ok) => { if (ok) void invoke("video_close"); })}>
                Save <kbd>↩</kbd>
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
