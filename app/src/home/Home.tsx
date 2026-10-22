import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/** One of the latest saved captures, as Rust lists it. It is opened by its position, never a path. */
type Recent = { name: string; width: number; height: number; thumb: string };

/**
 * The start window (decision 40, Snagit's model): what clicking the BHCapture icon shows when no
 * capture or recording is open -- a new capture, an image to open, or one of the latest captures.
 */
export function Home() {
  const [recent, setRecent] = useState<Recent[] | null>(null);
  const [busy, setBusy] = useState<number | "file" | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<Recent[]>("home_recent").then(setRecent).catch((e) => { setRecent([]); setError(String(e)); });
  }, []);

  const openRecent = (i: number) => {
    if (busy !== null) return;
    setBusy(i); setError(null);
    invoke("home_open_recent", { index: i }).catch((e) => setError(String(e))).finally(() => setBusy(null));
  };
  const openFile = () => {
    if (busy !== null) return;
    setBusy("file"); setError(null);
    invoke("home_open_file").catch((e) => setError(String(e))).finally(() => setBusy(null));
  };

  return (
    <div className="home">
      <div className="home-actions">
        <button className="home-big primary" onClick={() => void invoke("home_capture", { video: false })}>
          <span className="home-ico">📷</span><span>New Screenshot</span>
        </button>
        <button className="home-big" onClick={() => void invoke("home_capture", { video: true })}>
          <span className="home-ico">🎬</span><span>Record Video</span>
        </button>
        <button className="home-big" onClick={openFile} disabled={busy !== null}>
          <span className="home-ico">🖼️</span><span>{busy === "file" ? "Opening…" : "Open Image…"}</span>
        </button>
      </div>

      <div className="home-head">Recent captures</div>
      {error && <div className="home-error" role="alert">{error}</div>}
      {recent === null && <div className="home-empty">Loading…</div>}
      {recent !== null && recent.length === 0 && (
        <div className="home-empty">No saved captures yet. Take a screenshot, save it, and it will appear here.</div>
      )}
      {recent !== null && recent.length > 0 && (
        <div className="home-grid">
          {recent.map((r, i) => (
            <button key={`${i}-${r.name}`} className="home-card" title={r.name}
                    onClick={() => openRecent(i)} disabled={busy !== null}>
              <span className="home-thumb"><img src={r.thumb} alt="" draggable={false} /></span>
              <span className="home-name">{busy === i ? "Opening…" : r.name}</span>
              <span className="home-size">{r.width} × {r.height}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
