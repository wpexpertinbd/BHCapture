import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "../events";
import { Loupe, LoupeData } from "./Loupe";

/** What this overlay needs to turn local mouse coordinates into global desktop ones. */
type OverlayInfo = {
  display_id: number;
  origin_x: number;
  origin_y: number;
  width: number;
  height: number;
  scale: number;
};

import { GRIPS, Grip, Rect, reshape } from "./reshape";

/** Why a capture did not happen, as the Rust side reports it. */
type Notice = { kind: "permission" | "empty" | "cancelled" | "failed"; message: string };

function asNotice(e: unknown): Notice | null {
  if (e && typeof e === "object" && "kind" in e && "message" in e) {
    const n = e as Notice;
    // A dismissed Save dialog is the user's own choice, not something to report.
    return n.kind === "cancelled" ? null : n;
  }
  return { kind: "failed", message: String(e) };
}

/** Movement under this many points between press and release is a click, not a drag. */
const CLICK_SLOP = 4;
const LOUPE_OFFSET = 26;
const LOUPE_SIDE = 21 * 8;

export function Overlay() {
  const [info, setInfo] = useState<OverlayInfo | null>(null);
  const [sel, setSel] = useState<Rect | null>(null);
  const [snap, setSnap] = useState<Rect | null>(null);
  const [loupe, setLoupe] = useState<LoupeData | null>(null);
  const [cursor, setCursor] = useState<{ x: number; y: number } | null>(null);
  const [dragging, setDragging] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  // What Enter starts: an image for the editor, or a recording.
  const [mode, setMode] = useState<"image" | "video">("image");
  // All-in-One: the user can flip between the two before pressing Enter.
  const [switchable, setSwitchable] = useState(false);
  // While recording, this overlay only outlines the region (the mouse passes through it).
  const [frame, setFrame] = useState<Rect | null>(null);
  // Click rings while recording (global points), each removed when its animation ends.
  const [rings, setRings] = useState<{ id: number; x: number; y: number; right: boolean }[]>([]);
  const ringId = useRef(0);

  const dragRef = useRef(false);
  const downAt = useRef<{ x: number; y: number } | null>(null);
  const snapRef = useRef<Rect | null>(null);
  const infoRef = useRef<OverlayInfo | null>(null);
  // Resizing or moving the existing selection: which sides, from what rect, from which point.
  const reshapeRef = useRef<{ grip: Grip; start: Rect; from: { x: number; y: number } } | null>(null);
  const selRef = useRef<Rect | null>(null);
  snapRef.current = snap;
  selRef.current = sel;

  const loadInfo = useCallback(() => {
    invoke<OverlayInfo>("overlay_info").then(setInfo).catch(() => setInfo(null));
  }, []);
  useEffect(loadInfo, [loadInfo]);

  // A monitor was plugged in, removed or resized: this overlay's origin/size/scale may be stale.
  useEffect(() => {
    const un = listen("layout-changed", loadInfo);
    return () => { un.then((f) => f()); };
  }, [loadInfo]);

  // The selection is owned by Rust and broadcast to every overlay, because a drag that crosses
  // a display boundary is still ONE selection and no single webview can own it.
  useEffect(() => {
    const un = listen<{ rect: Rect | null }>("selection", (e) => setSel(e.payload.rect));
    return () => { un.then((f) => f()); };
  }, []);

  // A new selector session starts clean: no stale highlight or loupe from the last one.
  useEffect(() => {
    const un = listen("selection", (e) => {
      if ((e.payload as { rect: Rect | null }).rect === null && !dragRef.current) setSnap(null);
    });
    return () => { un.then((f) => f()); };
  }, []);

  // A drag can end without its mouseup reaching us: Enter/F/Esc are global keys, so the selector can
  // close (or turn into the click-through recording frame) while the button is still held. The
  // overlays are reused, so that drag must not survive into the next session -- a stale resize would
  // otherwise replay the old selection under the pointer on the very first mouse move.
  const endDrag = useCallback(() => {
    dragRef.current = false;
    reshapeRef.current = null;
    downAt.current = null;
    setDragging(false);
  }, []);

  useEffect(() => {
    const a = listen<{ mode: "image" | "video"; switchable: boolean }>("selector-mode", (e) => {
      endDrag();
      setMode(e.payload.mode); setSwitchable(e.payload.switchable); setFrame(null);
    });
    const b = listen<Rect | null>("record-frame", (e) => { endDrag(); setFrame(e.payload); setRings([]); });
    const c = listen<[number, number, boolean]>("record-click", (e) => {
      const [x, y, right] = e.payload;
      const id = ++ringId.current;
      setRings((r) => [...r.slice(-8), { id, x, y, right }]);
      setTimeout(() => setRings((r) => r.filter((q) => q.id !== id)), 650);
    });
    return () => { a.then((f) => f()); b.then((f) => f()); c.then((f) => f()); };
  }, [endDrag]);

  // A capture started by a global key (Enter pressed while another app is in front) has no promise
  // to reject, so its failure arrives as an event instead.
  useEffect(() => {
    const un = listen<Notice>("commit-error", (e) => setNotice(asNotice(e.payload)));
    return () => { un.then((f) => f()); };
  }, []);

  useEffect(() => {
    if (!notice || notice.kind === "permission") return;
    const t = setTimeout(() => setNotice(null), 6000);
    return () => clearTimeout(t);
  }, [notice]);

  const toGlobal = useCallback(
    (cx: number, cy: number) => (info ? { x: info.origin_x + cx, y: info.origin_y + cy } : null),
    [info]
  );

  // Pointer queries go to Rust, so they are coalesced: one in flight, newest position wins.
  // Without this a fast mouse queues hundreds of stale requests and the loupe lags behind.
  const pending = useRef<{ x: number; y: number } | null>(null);
  const lastPointer = useRef<{ x: number; y: number } | null>(null);
  const busy = useRef(false);
  const pump = useCallback(async () => {
    if (busy.current) return;
    busy.current = true;
    try {
      while (pending.current) {
        const p = pending.current;
        pending.current = null;
        const [s, l] = await Promise.all([
          dragRef.current ? Promise.resolve(null) : invoke<Rect | null>("snap_at", p),
          invoke<LoupeData | null>("loupe", p),
        ]);
        setSnap(s);
        setLoupe(l);
      }
    } catch {
      /* a failed query just means no highlight/loupe this frame */
    } finally {
      busy.current = false;
    }
  }, []);

  // The frozen frame lands ~100 ms after the overlay opens. If the pointer has not moved since,
  // ask again now so the magnifier appears without waiting for the next movement.
  useEffect(() => {
    const un = listen("frozen-ready", () => {
      if (lastPointer.current) { pending.current = lastPointer.current; void pump(); }
    });
    return () => { un.then((f) => f()); };
  }, [pump]);

  const onMouseDown = useCallback((e: React.MouseEvent) => {
    const g = toGlobal(e.clientX, e.clientY);
    if (!g) return;
    dragRef.current = true;
    setDragging(true);
    setNotice(null);
    // A press on a handle, an edge or inside the selection adjusts THAT selection, so a region
    // that came out a little short is fixed by dragging its side -- not by drawing it again.
    const grip = GRIPS[(e.target as HTMLElement).dataset?.grip ?? ""];
    const cur = selRef.current;
    if (grip && cur) {
      reshapeRef.current = { grip, start: cur, from: g };
      // A press inside that never moves is still a CLICK, which can pick a highlighted window
      // inside the selection (onMouseUp). Handles and edges are always a resize.
      downAt.current = grip.move ? { x: e.clientX, y: e.clientY } : null;
      return;
    }
    reshapeRef.current = null;
    downAt.current = { x: e.clientX, y: e.clientY };
    void invoke("selection_begin", g);
  }, [toGlobal]);

  const onMouseMove = useCallback((e: React.MouseEvent) => {
    setCursor({ x: e.clientX, y: e.clientY });
    const g = toGlobal(e.clientX, e.clientY);
    if (!g) return;
    // No button held means the press ended somewhere we never heard about: the drag is over.
    if (dragRef.current && e.buttons === 0) endDrag();
    const rs = reshapeRef.current;
    const d = downAt.current;
    if (dragRef.current && rs) {
      // A move starts only past the click slop, so a click inside does not nudge the selection;
      // once past it, the press is a drag for good (no window pick on release).
      if (rs.grip.move && d && Math.hypot(e.clientX - d.x, e.clientY - d.y) >= CLICK_SLOP) downAt.current = null;
      if (!(rs.grip.move && downAt.current)) {
        const [x, y, w, h] = reshape(rs.start, rs.grip, g.x - rs.from.x, g.y - rs.from.y);
        void invoke("selection_set_rect", { x, y, w, h });
      }
    } else if (dragRef.current) void invoke("selection_update", g);
    lastPointer.current = g;
    pending.current = g;
    void pump();
  }, [toGlobal, pump, endDrag]);

  // Releasing ENDS THE DRAG but keeps the selection, so it can still be nudged with the arrow
  // keys for pixel-accurate edges. Enter is what commits.
  const onMouseUp = useCallback((e: React.MouseEvent) => {
    const start = downAt.current;
    dragRef.current = false;
    setDragging(false);
    downAt.current = null;
    reshapeRef.current = null;
    if (!start) return;

    const moved = Math.hypot(e.clientX - start.x, e.clientY - start.y);
    const target = snapRef.current;
    // A click (not a drag) on a highlighted window selects that whole window.
    if (moved < CLICK_SLOP && target) {
      void invoke("selection_set_rect", { x: target[0], y: target[1], w: target[2], h: target[3] });
    }
  }, []);

  const onMouseLeave = useCallback(() => {
    setCursor(null);
    setLoupe(null);
    setSnap(null);
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); setNotice(null); void invoke("selection_cancel"); return; }
      if ((e.key === "f" || e.key === "F") && !e.metaKey && !e.ctrlKey && !e.altKey && infoRef.current) {
        e.preventDefault();
        void invoke("selection_fullscreen", { displayId: infoRef.current.display_id });
        return;
      }
      // Shift+Enter routes this one capture through a native Save dialog, so the default
      // folder never becomes a cage.
      if (e.key === "Enter") {
        e.preventDefault();
        // Failures used to be swallowed here, so a missing Screen Recording grant looked like
        // "Enter does nothing". Show them where the user is looking.
        invoke("selection_commit", { saveAs: e.shiftKey })
          .then(() => setNotice(null))
          .catch((err) => setNotice(asNotice(err)));
        return;
      }
      const step = e.shiftKey ? 10 : 1;
      const d: Record<string, [number, number]> = {
        ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step],
      };
      const v = d[e.key];
      if (v) { e.preventDefault(); void invoke("selection_nudge", { dx: v[0], dy: v[1] }); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  infoRef.current = info;
  if (!info) return frame ? <div /> : <div className="scrim-idle" />;

  if (frame) {
    const f = { x: frame[0] - info.origin_x, y: frame[1] - info.origin_y, w: frame[2], h: frame[3] };
    return (
      <>
        <div className="rec-frame" style={{ left: f.x - 3, top: f.y - 3, width: f.w + 6, height: f.h + 6 }} />
        {rings.map((r) => (
          <div key={r.id} className={`click-ring${r.right ? " right" : ""}`}
               style={{ left: r.x - info.origin_x, top: r.y - info.origin_y }} />
        ))}
      </>
    );
  }

  // Global -> this overlay's local coordinates.
  const toLocal = (r: Rect) => ({ x: r[0] - info.origin_x, y: r[1] - info.origin_y, w: r[2], h: r[3] });
  const local = sel ? toLocal(sel) : null;
  const snapLocal = snap && !dragging ? toLocal(snap) : null;

  // Physical pixels are what the file will actually contain, so show both. On a Retina panel
  // "400 x 300" and "800 x 600" are both true and only one of them is the image size.
  const px = sel ? `${Math.round(sel[2] * info.scale)}x${Math.round(sel[3] * info.scale)}` : "";
  const readoutTop = local ? (local.y > 34 ? local.y - 30 : local.y + local.h + 8) : 0;

  // Keep the loupe on screen: flip to the other side of the pointer near the right/bottom edge.
  let loupeLeft = 0, loupeTop = 0;
  if (cursor) {
    loupeLeft = cursor.x + LOUPE_OFFSET + LOUPE_SIDE > info.width ? cursor.x - LOUPE_OFFSET - LOUPE_SIDE : cursor.x + LOUPE_OFFSET;
    loupeTop = cursor.y + LOUPE_OFFSET + LOUPE_SIDE + 40 > info.height ? cursor.y - LOUPE_OFFSET - LOUPE_SIDE - 40 : cursor.y + LOUPE_OFFSET;
  }

  return (
    <div
      onMouseDown={onMouseDown}
      onMouseMove={onMouseMove}
      onMouseUp={onMouseUp}
      onMouseLeave={onMouseLeave}
      style={{ position: "fixed", inset: 0 }}
    >
      {!local && <div className="scrim-idle" />}

      {snapLocal && (
        <>
          <div className="snap" style={{ left: snapLocal.x, top: snapLocal.y, width: snapLocal.w, height: snapLocal.h }} />
          <div className="readout snap-readout"
               style={{ left: Math.max(4, snapLocal.x), top: snapLocal.y > 34 ? snapLocal.y - 30 : snapLocal.y + 8 }}>
            Window &nbsp;{Math.round(snapLocal.w)} &times; {Math.round(snapLocal.h)}
          </div>
        </>
      )}

      {local && local.w > 0 && local.h > 0 && (
        <>
          <div className="selection" data-grip="move"
               style={{ left: local.x, top: local.y, width: local.w, height: local.h }} />
          {/* Edges (invisible, wide enough to catch) and eight visible handles. */}
          {(["n", "s", "e", "w"] as const).map((k) => (
            <div key={k} data-grip={k} className={`sel-edge sel-edge-${k}`}
                 style={k === "n" || k === "s"
                   ? { left: local.x, width: local.w, top: (k === "n" ? local.y : local.y + local.h) - 5 }
                   : { top: local.y, height: local.h, left: (k === "w" ? local.x : local.x + local.w) - 5 }} />
          ))}
          {(["nw", "n", "ne", "e", "se", "s", "sw", "w"] as const).map((k) => {
            const hx = k.includes("w") ? local.x : k.includes("e") ? local.x + local.w : local.x + local.w / 2;
            const hy = k.includes("n") ? local.y : k.includes("s") ? local.y + local.h : local.y + local.h / 2;
            return <div key={`h-${k}`} data-grip={k} className={`sel-handle sel-cur-${k}`}
                        style={{ left: hx - 5, top: hy - 5 }} />;
          })}
          <div className="readout" style={{ left: Math.max(4, local.x), top: readoutTop }}>
            {Math.round(local.w)} &times; {Math.round(local.h)}
            <span className="px">&nbsp;&nbsp;{px} px</span>
          </div>
        </>
      )}

      {cursor && !local && !snapLocal && (
        <>
          <div className="crosshair-h" style={{ top: cursor.y }} />
          <div className="crosshair-v" style={{ left: cursor.x }} />
        </>
      )}

      {cursor && loupe && info && (
        <Loupe data={loupe} gx={info.origin_x + cursor.x} gy={info.origin_y + cursor.y}
               left={loupeLeft} top={loupeTop} />
      )}

      {notice && (
        <div className={`notice notice-${notice.kind}`} role="alert" onMouseDown={(e) => e.stopPropagation()}>
          <div className="notice-text">{notice.message}</div>
          {notice.kind === "permission" && (
            <div className="notice-actions">
              <button onClick={() => void invoke("open_privacy_settings")}>Open System Settings</button>
              <button className="primary" onClick={() => void invoke("relaunch_app")}>Relaunch BHCapture</button>
            </div>
          )}
        </div>
      )}

      {/* All-in-One: Image ⇄ Video, chosen before Enter. Tab flips it. */}
      {switchable && (
        <div className="mode-switch" style={{ bottom: 116 }}
             onMouseDown={(e) => e.stopPropagation()} onMouseUp={(e) => e.stopPropagation()}>
          <button className={mode === "image" ? "on" : ""} onClick={() => void invoke("selector_set_mode", { video: false })}>📷 Image</button>
          <button className={mode === "video" ? "on" : ""} onClick={() => void invoke("selector_set_mode", { video: true })}>🎬 Video</button>
          <span className="mode-key"><kbd>Tab</kbd></span>
        </div>
      )}

      {/* One click: the whole of THIS screen, then straight on (editor, or the recording bar). */}
      <button className="fs-btn" style={{ bottom: 70 }}
              onMouseDown={(e) => e.stopPropagation()}
              onMouseUp={(e) => e.stopPropagation()}
              onClick={(e) => { e.stopPropagation(); void invoke("selection_fullscreen", { displayId: info.display_id }); }}>
        <span className="fs-icon">⛶</span> {mode === "video" ? "Record full screen" : "Full screen"} <kbd>F</kbd>
      </button>

      <div className="hint" style={{ bottom: 28 }}>
        {local
          ? <><kbd>Enter</kbd> {mode === "video" ? "record this area" : "edit"} &nbsp;drag edges to resize &nbsp;<kbd>&larr;&uarr;&rarr;&darr;</kbd> nudge <kbd>Esc</kbd> cancel</>
          : <>{mode === "video" ? "Record: click a window or drag any size" : "Click a window or drag any size"} &nbsp;<kbd>Esc</kbd> cancel</>}
      </div>
    </div>
  );
}
