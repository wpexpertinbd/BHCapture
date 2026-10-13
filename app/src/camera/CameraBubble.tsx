import { useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "../events";

interface Style { shape: "circle" | "rounded" | "square" | "wide"; width: number; height: number }

/** The camera bubble (decision 34). Frames arrive already cropped, mirrored, enhanced and with the
 *  background replaced (Rust); this page only paints them and gives the bubble its shape. Drag it
 *  anywhere -- it is recorded where it sits. */
export function CameraBubble() {
  const canvas = useRef<HTMLCanvasElement>(null);
  const [style, setStyle] = useState<Style | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [live, setLive] = useState(false);

  useEffect(() => {
    document.documentElement.classList.add("cam-page");
    const ch = new Channel<ArrayBuffer>();
    ch.onmessage = (buf) => {
      const c = canvas.current;
      if (!c || !(buf instanceof ArrayBuffer) || buf.byteLength < 8) return;
      const head = new DataView(buf, 0, 8);
      const w = head.getUint32(0, true), h = head.getUint32(4, true);
      if (buf.byteLength !== 8 + w * h * 4) return;
      if (c.width !== w || c.height !== h) { c.width = w; c.height = h; }
      c.getContext("2d")!.putImageData(new ImageData(new Uint8ClampedArray(buf, 8), w, h), 0, 0);
      setLive(true);
    };
    invoke<{ style: Style }>("camera_attach", { onFrame: ch })
      .then((r) => setStyle(r.style))
      .catch((e) => setError(String(e)));
    const a = listen<Style>("camera-style", (e) => setStyle(e.payload));
    const b = listen<string>("camera-error", (e) => setError(e.payload));
    return () => { void a.then((f) => f()); void b.then((f) => f()); };
  }, []);

  const shape = style?.shape ?? "circle";
  return (
    <div className={`cam cam-${shape}`}
         onMouseDown={(e) => { if (e.button === 0) void getCurrentWindow().startDragging(); }}
         title="Drag to move the camera">
      <canvas ref={canvas} className="cam-canvas" />
      {!live && <div className="cam-msg">{error ?? "Starting camera…"}</div>}
    </div>
  );
}
