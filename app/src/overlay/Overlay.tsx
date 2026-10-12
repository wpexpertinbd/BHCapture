import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

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

export function Overlay() {
  const [info, setInfo] = useState<OverlayInfo | null>(null);
  const [sel, setSel] = useState<Rect | null>(null);
  const [cursor, setCursor] = useState<{ x: number; y: number } | null>(null);
  const dragging = useRef(false);

  useEffect(() => {
    invoke<OverlayInfo>("overlay_info").then(setInfo).catch(() => setInfo(null));
  }, []);

  // The selection is owned by Rust and broadcast to every overlay, because a drag that crosses
  // a display boundary is still ONE selection and no single webview can own it.
  useEffect(() => {
    const un = listen<{ rect: Rect | null }>("selection", (e) => setSel(e.payload.rect));
    return () => { un.then((f) => f()); };
  }, []);

  const toGlobal = useCallback(
    (e: { clientX: number; clientY: number }) =>
      info ? { x: info.origin_x + e.clientX, y: info.origin_y + e.clientY } : null,
    [info]
  );

  const onMouseDown = useCallback((e: React.MouseEvent) => {
    const g = toGlobal(e);
    if (!g) return;
    dragging.current = true;
    void invoke("selection_begin", g);
  }, [toGlobal]);

  const onMouseMove = useCallback((e: React.MouseEvent) => {
    setCursor({ x: e.clientX, y: e.clientY });
    if (!dragging.current) return;
    const g = toGlobal(e);
    if (g) void invoke("selection_update", g);
  }, [toGlobal]);

  // Releasing the mouse ENDS THE DRAG but keeps the selection, so it can still be nudged with
  // the arrow keys for pixel-accurate edges. Enter is what commits.
  const onMouseUp = useCallback(() => { dragging.current = false; }, []);

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
  const local = sel
    ? { x: sel[0] - info.origin_x, y: sel[1] - info.origin_y, w: sel[2], h: sel[3] }
    : null;

  // Physical pixels are what the file will actually contain, so show both. On a Retina panel
  // "400 x 300" and "800 x 600" are both true and only one of them is the image size.
  const px = sel ? `${Math.round(sel[2] * info.scale)}x${Math.round(sel[3] * info.scale)}` : "";

  const readoutTop = local ? (local.y > 34 ? local.y - 30 : local.y + local.h + 8) : 0;

  return (
    <div
      onMouseDown={onMouseDown}
      onMouseMove={onMouseMove}
      onMouseUp={onMouseUp}
      style={{ position: "fixed", inset: 0 }}
    >
      {!local && <div className="scrim-idle" />}

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

      {cursor && !local && (
        <>
          <div className="crosshair-h" style={{ top: cursor.y }} />
          <div className="crosshair-v" style={{ left: cursor.x }} />
        </>
      )}

      <div className="hint" style={{ bottom: 28 }}>
        {local
          ? <><kbd>Enter</kbd> save <kbd>&#8679;Enter</kbd> save as&hellip; <kbd>&larr;&uarr;&rarr;&darr;</kbd> nudge <kbd>Esc</kbd> cancel</>
          : <>Drag to select &nbsp;<kbd>Esc</kbd> cancel</>}
      </div>
    </div>
  );
}
