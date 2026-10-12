import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
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

/** Global logical rect: [x, y, w, h]. */
type Rect = [number, number, number, number];

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

  const dragRef = useRef(false);
  const downAt = useRef<{ x: number; y: number } | null>(null);
  const snapRef = useRef<Rect | null>(null);
  snapRef.current = snap;

  useEffect(() => {
    invoke<OverlayInfo>("overlay_info").then(setInfo).catch(() => setInfo(null));
  }, []);

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

  const toGlobal = useCallback(
    (cx: number, cy: number) => (info ? { x: info.origin_x + cx, y: info.origin_y + cy } : null),
    [info]
  );

  // Pointer queries go to Rust, so they are coalesced: one in flight, newest position wins.
  // Without this a fast mouse queues hundreds of stale requests and the loupe lags behind.
  const pending = useRef<{ x: number; y: number } | null>(null);
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

  const onMouseDown = useCallback((e: React.MouseEvent) => {
    const g = toGlobal(e.clientX, e.clientY);
    if (!g) return;
    dragRef.current = true;
    setDragging(true);
    downAt.current = { x: e.clientX, y: e.clientY };
    void invoke("selection_begin", g);
  }, [toGlobal]);

  const onMouseMove = useCallback((e: React.MouseEvent) => {
    setCursor({ x: e.clientX, y: e.clientY });
    const g = toGlobal(e.clientX, e.clientY);
    if (!g) return;
    if (dragRef.current) void invoke("selection_update", g);
    pending.current = g;
    void pump();
  }, [toGlobal, pump]);

  // Releasing ENDS THE DRAG but keeps the selection, so it can still be nudged with the arrow
  // keys for pixel-accurate edges. Enter is what commits.
  const onMouseUp = useCallback((e: React.MouseEvent) => {
    const start = downAt.current;
    dragRef.current = false;
    setDragging(false);
    downAt.current = null;
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
      if (e.key === "Escape") { e.preventDefault(); void invoke("selection_cancel"); return; }
      // Shift+Enter routes this one capture through a native Save dialog, so the default
      // folder never becomes a cage.
      if (e.key === "Enter") {
        e.preventDefault();
        void invoke("selection_commit", { saveAs: e.shiftKey }).catch(() => {});
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

  if (!info) return <div className="scrim-idle" />;

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
          <div className="selection"
               style={{ left: local.x, top: local.y, width: local.w, height: local.h }} />
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

      <div className="hint" style={{ bottom: 28 }}>
        {local
          ? <><kbd>Enter</kbd> save <kbd>&#8679;Enter</kbd> save as&hellip; <kbd>&larr;&uarr;&rarr;&darr;</kbd> nudge <kbd>Esc</kbd> cancel</>
          : <>Click a window or drag any size &nbsp;<kbd>Esc</kbd> cancel</>}
      </div>
    </div>
  );
}
