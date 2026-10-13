import { isLineKind, squared } from "./model";
import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "../events";
import { COLORS, WIDTHS, Shape, Tool, box, hitTest, moved, render } from "./model";

type Format = "jpeg" | "png" | "webp" | "tiff" | "bmp" | "gif";
type EditorInfo = { id: number; width: number; height: number; scale: number; saved_to: string | null; format: Format };
type Notice = { kind: string; message: string };

const FORMATS: { value: Format; label: string }[] = [
  { value: "jpeg", label: "JPEG" }, { value: "png", label: "PNG" }, { value: "webp", label: "WebP" },
  { value: "tiff", label: "TIFF" }, { value: "bmp", label: "BMP" }, { value: "gif", label: "GIF" },
];

/** The shapes group: one toolbar button showing the current pick, with the rest behind its ▾.
 *  Arrow first and the default -- the most used annotation. */
const SHAPES: { tool: Tool; key: string; label: string; icon: string }[] = [
  { tool: "arrow", key: "a", label: "Arrow", icon: "↗" },
  { tool: "darrow", key: "d", label: "Double arrow", icon: "↔" },
  { tool: "line", key: "l", label: "Line", icon: "╱" },
  { tool: "ellipse", key: "o", label: "Circle", icon: "◯" },
];

const TOOLS: { tool: Tool; key: string; label: string; icon: string }[] = [
  { tool: "select", key: "v", label: "Select", icon: "➚" },
  { tool: "rect", key: "r", label: "Box", icon: "▭" },
  { tool: "text", key: "t", label: "Text", icon: "T" },
  { tool: "highlight", key: "h", label: "Highlight", icon: "▬" },
  { tool: "pixelate", key: "p", label: "Pixelate", icon: "▦" },
  { tool: "step", key: "n", label: "Step", icon: "①" },
];

// History lives in a REDUCER. Event handlers dispatch what they MEAN ("add this shape", "move
// shape 4 by dx,dy"), and the reducer applies it to the CURRENT state. Handlers that built a new
// array from the `shapes` they closed over lost work: a quick drag ended before React re-rendered,
// so pointer-up saw a stale draft, and an append to a stale list dropped shapes that were on screen
// but never reached the saved file.
type Doc = { shapes: Shape[]; undo: Shape[][]; redo: Shape[][]; dirty: boolean; nextId: number };
type Act =
  | { t: "add"; shape: Omit<Shape, "id" | "n"> }
  | { t: "remove"; id: number }
  | { t: "moveLive"; id: number; to: Shape }          // during a drag: no history entry
  | { t: "moveDone"; from: Shape }                    // drag ended: one history entry
  | { t: "undo" } | { t: "redo" } | { t: "saved" };

function reduce(d: Doc, a: Act): Doc {
  switch (a.t) {
    case "add": {
      const n = a.shape.kind === "step" ? d.shapes.filter((s) => s.kind === "step").length + 1 : undefined;
      const shape: Shape = { ...a.shape, id: d.nextId, n };
      return { shapes: [...d.shapes, shape], undo: [...d.undo, d.shapes], redo: [], dirty: true, nextId: d.nextId + 1 };
    }
    case "remove":
      return { ...d, shapes: d.shapes.filter((s) => s.id !== a.id), undo: [...d.undo, d.shapes], redo: [], dirty: true };
    case "moveLive":
      return { ...d, shapes: d.shapes.map((s) => (s.id === a.id ? a.to : s)) };
    case "moveDone":
      return { ...d, undo: [...d.undo, d.shapes.map((s) => (s.id === a.from.id ? a.from : s))], redo: [], dirty: true };
    case "undo":
      if (!d.undo.length) return d;
      return { ...d, shapes: d.undo[d.undo.length - 1], undo: d.undo.slice(0, -1), redo: [...d.redo, d.shapes], dirty: true };
    case "redo":
      if (!d.redo.length) return d;
      return { ...d, shapes: d.redo[d.redo.length - 1], redo: d.redo.slice(0, -1), undo: [...d.undo, d.shapes], dirty: true };
    case "saved":
      return { ...d, dirty: false };
  }
}

export function Editor() {
  const [info, setInfo] = useState<EditorInfo | null>(null);
  const [base, setBase] = useState<ImageBitmap | null>(null);
  const [error, setError] = useState<string | null>(null);

  // A fresh capture is unsaved by definition, hence dirty from the start.
  const [doc, dispatch] = useReducer(reduce, { shapes: [], undo: [], redo: [], dirty: true, nextId: 1 });
  const { shapes, dirty } = doc;
  const [tool, setTool] = useState<Tool>("arrow");
  // The shape the group button currently stands for (Arrow until another is picked).
  const [shape, setShape] = useState<Tool>("arrow");
  const [shapeMenu, setShapeMenu] = useState(false);
  const pickShape = (t: Tool) => { setShape(t); setTool(t); setShapeMenu(false); };
  const [color, setColor] = useState(COLORS[0]);
  const [width, setWidth] = useState(WIDTHS[1]);
  const [format, setFormat] = useState<Format>("jpeg");
  const [selected, setSelected] = useState<number | null>(null);
  const [draft, setDraft] = useState<Shape | null>(null);
  const [textEdit, setTextEdit] = useState<{ x: number; y: number; value: string } | null>(null);
  const [savedTo, setSavedTo] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [askClose, setAskClose] = useState(false);

  const canvasRef = useRef<HTMLCanvasElement>(null);
  const drag = useRef<{ mode: "draw" | "move"; x: number; y: number; orig?: Shape; moved?: boolean } | null>(null);
  // The in-progress shape lives in a ref (read by pointer-up, always current) AND in state (to draw).
  const draftRef = useRef<Shape | null>(null);
  const setDraftBoth = (d: Shape | null) => { draftRef.current = d; setDraft(d); };
  const k = info?.scale ?? 1;

  // ---------------------------------------------------------------- load the capture
  useEffect(() => {
    (async () => {
      try {
        const i = await invoke<EditorInfo>("editor_info");
        const buf = await invoke<ArrayBuffer>("editor_pixels");
        const bmp = await createImageBitmap(new ImageData(new Uint8ClampedArray(buf), i.width, i.height));
        setInfo(i); setFormat(i.format); setSavedTo(i.saved_to); setBase(bmp);
      } catch (e) {
        setError(String(e));
      }
    })();
  }, []);

  // ---------------------------------------------------------------- draw
  const visible = useMemo(() => (draft ? [...shapes, draft] : shapes), [shapes, draft]);
  useEffect(() => {
    const c = canvasRef.current;
    if (!c || !base || !info) return;
    render(c.getContext("2d")!, base, info.width, info.height, visible, k, selected, false);
  }, [visible, base, info, k, selected]);

  // ---------------------------------------------------------------- history
  const doUndo = useCallback(() => { setSelected(null); dispatch({ t: "undo" }); }, []);
  const doRedo = useCallback(() => { setSelected(null); dispatch({ t: "redo" }); }, []);

  // ---------------------------------------------------------------- export
  /** Render a CLEAN copy (no selection chrome) at full resolution, as a lossless PNG. */
  const exportPng = useCallback(async (): Promise<Uint8Array> => {
    if (!base || !info) throw new Error("not loaded");
    const off = new OffscreenCanvas(info.width, info.height);
    render(off.getContext("2d") as unknown as CanvasRenderingContext2D, base, info.width, info.height, shapes, k, null, true);
    const blob = await off.convertToBlob({ type: "image/png" });
    return new Uint8Array(await blob.arrayBuffer());
  }, [base, info, shapes, k]);

  const save = useCallback(async (saveAs: boolean): Promise<boolean> => {
    if (busy) return false;
    setBusy(true);
    try {
      const png = await exportPng();
      const path = await invoke<string>("editor_save", png, { headers: { "x-save-as": saveAs ? "1" : "0", "x-format": format } });
      setSavedTo(path); dispatch({ t: "saved" });
      setNotice({ kind: "ok", message: `Saved ${path.split("/").pop()}` });
      return true;
    } catch (e) {
      const n = e as Notice;
      if (n && n.kind === "cancelled") return false;   // the user's own choice; nothing to report
      setNotice({ kind: "error", message: n?.message ?? String(e) });
      return false;
    } finally {
      setBusy(false);
    }
  }, [busy, exportPng, format]);

  const copy = useCallback(async () => {
    try {
      await invoke("editor_copy", await exportPng());
      setNotice({ kind: "ok", message: "Copied to the clipboard" });
    } catch (e) {
      setNotice({ kind: "error", message: String(e) });
    }
  }, [exportPng]);

  // Closing: ask only when there is something to lose. Our own dialog -- window.confirm in a Tauri
  // webview shows nothing and returns TRUE, which would discard work silently.
  const requestClose = useCallback(() => {
    if (dirty) setAskClose(true);
    else void invoke("editor_close");
  }, [dirty]);
  useEffect(() => {
    const un = listen("editor-close-requested", () => requestClose());
    return () => { un.then((f) => f()); };
  }, [requestClose]);

  // Save, Save As, Copy, Undo, Redo, Close and Format live in the MENU BAR (File / Edit), which owns
  // their shortcuts; the menu sends the action to whichever editor has focus.
  const menuAction = useRef<(a: string) => void>(() => {});
  menuAction.current = (a: string) => {
    const typing = document.activeElement instanceof HTMLInputElement;
    if (a === "save") void save(false);
    else if (a === "save-as") void save(true);
    else if (a === "close") requestClose();
    // While a text box is being typed into, Copy/Undo/Redo are for THAT text, not the drawing.
    else if (a === "copy") { if (typing) document.execCommand("copy"); else void copy(); }
    else if (a === "undo") { if (typing) document.execCommand("undo"); else doUndo(); }
    else if (a === "redo") { if (typing) document.execCommand("redo"); else doRedo(); }
    else if (a === "delete") { if (selected !== null) { dispatch({ t: "remove", id: selected }); setSelected(null); } }
    else if (a.startsWith("format:")) {
      const ext = a.slice(7);
      setFormat((ext === "jpg" ? "jpeg" : ext) as Format);
    }
  };
  useEffect(() => {
    const un = listen<string>("editor-menu", (e) => menuAction.current(e.payload));
    return () => { un.then((f) => f()); };
  }, []);

  // Keep File > Format ticked to THIS editor's format whenever it changes or the window is focused.
  useEffect(() => {
    void invoke("editor_format_changed", { format });
    const onFocus = () => void invoke("editor_format_changed", { format });
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [format]);

  useEffect(() => {
    if (!notice) return;
    const t = setTimeout(() => setNotice(null), notice.kind === "error" ? 7000 : 2600);
    return () => clearTimeout(t);
  }, [notice]);

  // ---------------------------------------------------------------- pointer
  const toImage = (e: React.PointerEvent) => {
    const c = canvasRef.current!;
    const r = c.getBoundingClientRect();
    return { x: ((e.clientX - r.left) / r.width) * c.width, y: ((e.clientY - r.top) / r.height) * c.height };
  };

  const onPointerDown = (e: React.PointerEvent) => {
    if (!info || textEdit) return;
    // Keep receiving the drag outside the canvas. Can throw for a pointer the browser does not
    // consider active; drawing must not depend on it.
    try { (e.target as Element).setPointerCapture(e.pointerId); } catch { /* optional */ }
    const p = toImage(e);
    if (tool === "select") {
      const hit = hitTest(shapes, p.x, p.y, k, canvasRef.current!.getContext("2d")!);
      setSelected(hit?.id ?? null);
      if (hit) drag.current = { mode: "move", x: p.x, y: p.y, orig: hit };
      return;
    }
    if (tool === "text") {
      setTextEdit({ x: p.x, y: p.y, value: "" });
      return;
    }
    if (tool === "step") {
      dispatch({ t: "add", shape: { kind: "step", x1: p.x, y1: p.y, x2: p.x, y2: p.y, color, width } });
      return;
    }
    drag.current = { mode: "draw", x: p.x, y: p.y };
    setDraftBoth({ id: -1, kind: tool, x1: p.x, y1: p.y, x2: p.x, y2: p.y,
                   color: tool === "highlight" ? "#ffd60a" : color, width });
  };

  const onPointerMove = (e: React.PointerEvent) => {
    const d = drag.current;
    if (!d) return;
    const p = toImage(e);
    if (d.mode === "draw") {
      if (draftRef.current) {
        const next = { ...draftRef.current, x2: p.x, y2: p.y };
        // Shift: a perfect circle / square.
        setDraftBoth(e.shiftKey && (next.kind === "ellipse" || next.kind === "rect") ? squared(next) : next);
      }
    } else if (d.orig) {
      d.moved = true;
      dispatch({ t: "moveLive", id: d.orig.id, to: moved(d.orig, p.x - d.x, p.y - d.y) });
    }
  };

  const onPointerUp = (e: React.PointerEvent) => {
    const d = drag.current;
    drag.current = null;
    if (!d) return;
    if (d.mode === "draw" && draftRef.current) {
      // Include the release point itself: a fast flick can end without a final move event.
      const p = toImage(e);
      const raw = { ...draftRef.current, x2: p.x, y2: p.y };
      const fin = e.shiftKey && (raw.kind === "ellipse" || raw.kind === "rect") ? squared(raw) : raw;
      const b = box(fin);
      const big = isLineKind(fin.kind) ? Math.hypot(fin.x2 - fin.x1, fin.y2 - fin.y1) > 6 * k : b.w > 4 * k && b.h > 4 * k;
      if (big) {
        const { id: _drop, ...rest } = fin;
        dispatch({ t: "add", shape: rest });
      }
      setDraftBoth(null);
    } else if (d.mode === "move" && d.moved && d.orig) {
      dispatch({ t: "moveDone", from: d.orig });
    }
  };

  const finishText = (keep: boolean) => {
    if (keep && textEdit && textEdit.value.trim()) {
      dispatch({ t: "add", shape: { kind: "text", x1: textEdit.x, y1: textEdit.y, x2: textEdit.x, y2: textEdit.y,
                                    color, width, text: textEdit.value } });
    }
    setTextEdit(null);
  };

  // ---------------------------------------------------------------- keyboard
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (textEdit) return;                       // the text box owns the keyboard while editing
      const mod = e.metaKey || e.ctrlKey;
      const key = e.key.toLowerCase();
      if (askClose) {
        if (e.key === "Escape") { e.preventDefault(); setAskClose(false); }
        if (e.key === "Enter") { e.preventDefault(); void save(false).then((ok) => ok && invoke("editor_close")); }
        if (mod && key === "d") { e.preventDefault(); void invoke("editor_close"); }
        return;
      }
      // Cmd-S/Shift-Cmd-S/Cmd-C/Cmd-Z/Cmd-W belong to the menu bar. Handling them here as well could
      // run an action twice.
      if (mod) return;
      if ((e.key === "Backspace" || e.key === "Delete") && selected !== null) {
        e.preventDefault(); dispatch({ t: "remove", id: selected }); setSelected(null); return;
      }
      if (e.key === "Escape") { setSelected(null); setDraftBoth(null); return; }
      const sh = SHAPES.find((s) => s.key === key);
      if (sh) { pickShape(sh.tool); return; }
      const t = TOOLS.find((t) => t.key === key);
      if (t) { setTool(t.tool); if (t.tool !== "select") setSelected(null); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [textEdit, askClose, save, copy, requestClose, doUndo, doRedo, selected]);

  // ---------------------------------------------------------------- view
  if (error) return <div className="editor-error">Could not open this capture: {error}</div>;
  if (!info || !base) return <div className="editor-loading">Opening capture…</div>;

  const c = canvasRef.current;
  const r = c?.getBoundingClientRect();
  const textStyle = textEdit && r ? {
    left: (textEdit.x / info.width) * r.width, top: (textEdit.y / info.height) * r.height,
    color, fontSize: `${((14 + width * 3) * k * r.width) / info.width}px`,
  } : undefined;

  return (
    <div className="editor">
      <div className="ed-toolbar">
        <div className="ed-group">
          {TOOLS.map((t, i) => (
            <span key={t.tool} style={{ display: "contents" }}>
              <button className={`ed-tool ${tool === t.tool ? "on" : ""}`}
                      title={`${t.label} (${t.key.toUpperCase()})`} onClick={() => setTool(t.tool)}>
                <span className="ed-icon">{t.icon}</span><span className="ed-label">{t.label}</span>
              </button>
              {i === 0 && (() => {
                const cur = SHAPES.find((s) => s.tool === shape) ?? SHAPES[0];
                return (
                  <span className="ed-shapes">
                    <button className={`ed-tool ${tool === cur.tool ? "on" : ""}`}
                            title={`${cur.label} (${cur.key.toUpperCase()})`} onClick={() => setTool(cur.tool)}>
                      <span className="ed-icon">{cur.icon}</span><span className="ed-label">{cur.label}</span>
                    </button>
                    <button className={`ed-more ${shapeMenu ? "open" : ""}`} title="More shapes"
                            onClick={() => setShapeMenu((v) => !v)}>▾</button>
                    {shapeMenu && (
                      <div className="ed-shape-menu" role="menu" onMouseLeave={() => setShapeMenu(false)}>
                        {SHAPES.map((s) => (
                          <button key={s.tool} role="menuitem" className={s.tool === shape ? "on" : ""}
                                  onClick={() => pickShape(s.tool)}>
                            <span className="ed-icon">{s.icon}</span>{s.label}<kbd>{s.key.toUpperCase()}</kbd>
                          </button>
                        ))}
                      </div>
                    )}
                  </span>
                );
              })()}
            </span>
          ))}
        </div>
        <div className="ed-group">
          {COLORS.map((cl) => (
            <button key={cl} className={`ed-swatch ${color === cl ? "on" : ""}`} style={{ background: cl }}
                    title={cl} onClick={() => setColor(cl)} />
          ))}
          {WIDTHS.map((w) => (
            <button key={w} className={`ed-width ${width === w ? "on" : ""}`} title={`Width ${w}`} onClick={() => setWidth(w)}>
              <svg width="16" height="12" aria-hidden="true">
                <line x1="2" y1="6" x2="14" y2="6" stroke="currentColor" strokeWidth={w} strokeLinecap="round" />
              </svg>
            </button>
          ))}
        </div>
      </div>

      <div className="ed-stage">
        <div className="ed-canvas-wrap">
          <canvas ref={canvasRef} width={info.width} height={info.height}
                  className={`ed-canvas tool-${tool}`}
                  style={{ aspectRatio: `${info.width} / ${info.height}`, maxWidth: `${info.width / k}px` }}
                  onPointerDown={onPointerDown} onPointerMove={onPointerMove} onPointerUp={onPointerUp} />
          {textEdit && textStyle && (
            <input className="ed-text-input" autoFocus style={textStyle} value={textEdit.value}
                   placeholder="Type, then Enter"
                   onChange={(e) => setTextEdit({ ...textEdit, value: e.target.value })}
                   onKeyDown={(e) => { if (e.key === "Enter") finishText(true); if (e.key === "Escape") finishText(false); }}
                   onBlur={() => finishText(true)} />
          )}
        </div>
      </div>

      <div className="ed-status">
        <span>{info.width} × {info.height} px</span>
        <span className="ed-dim">{FORMATS.find((f) => f.value === format)?.label}</span>
        <span className="ed-dim">{savedTo ? (dirty ? "Edited since last save" : `Saved · ${savedTo.split("/").pop()}`) : "Not saved yet"}</span>
        <span className="ed-dim ed-keys">File › Save ⌘S · Save As ⇧⌘S · Image Format · Edit › Copy ⌘C · Undo ⌘Z</span>
      </div>

      {notice && <div className={`ed-notice ${notice.kind}`}>{notice.message}</div>}

      {askClose && (
        <div className="ed-modal-backdrop">
          <div className="ed-modal" role="dialog" aria-modal="true">
            <div className="ed-modal-title">Save this capture before closing?</div>
            <div className="ed-modal-text">It has not been saved anywhere. If you don't save it, it is gone.</div>
            <div className="ed-modal-actions">
              <button className="ed-btn" onClick={() => void invoke("editor_close")}>Don't Save <kbd>⌘D</kbd></button>
              <div className="ed-spacer" />
              <button className="ed-btn" onClick={() => setAskClose(false)}>Cancel <kbd>Esc</kbd></button>
              <button className="ed-btn primary" autoFocus
                      onClick={() => void save(false).then((ok) => ok && invoke("editor_close"))}>Save <kbd>↩</kbd></button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
