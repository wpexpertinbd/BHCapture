import { useEffect, useRef } from "react";

export type LoupeData = {
  size: number;   // side, physical pixels
  scale: number;  // physical pixels per logical point on this display
  rgba: string;   // base64, tightly packed RGBA8
};

const ZOOM = 8;

function decode(b64: string): Uint8ClampedArray {
  const bin = atob(b64);
  const out = new Uint8ClampedArray(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** Colour of the centre pixel, which is the one the selection edge will land on. */
export function centreHex(d: LoupeData): string {
  const px = decode(d.rgba);
  const mid = (Math.floor(d.size / 2) * d.size + Math.floor(d.size / 2)) * 4;
  if (px[mid + 3] === 0) return "outside display";
  const h = (v: number) => v.toString(16).padStart(2, "0");
  return `#${h(px[mid])}${h(px[mid + 1])}${h(px[mid + 2])}`.toUpperCase();
}

/** The magnifier: real screen pixels around the pointer, enlarged with NO smoothing, so each
 *  physical pixel stays a crisp square and an edge can be placed on exactly the one intended. */
export function Loupe({ data, gx, gy, left, top }: {
  data: LoupeData; gx: number; gy: number; left: number; top: number;
}) {
  const ref = useRef<HTMLCanvasElement>(null);
  const side = data.size * ZOOM;

  useEffect(() => {
    const c = ref.current;
    if (!c) return;
    const ctx = c.getContext("2d");
    if (!ctx) return;
    const src = document.createElement("canvas");
    src.width = data.size; src.height = data.size;
    src.getContext("2d")!.putImageData(new ImageData(decode(data.rgba), data.size, data.size), 0, 0);

    ctx.imageSmoothingEnabled = false; // the whole point: no blur between pixels
    ctx.fillStyle = "#16161a";
    ctx.fillRect(0, 0, side, side);
    ctx.drawImage(src, 0, 0, side, side);

    // Frame the centre pixel in black and white so it reads on any background.
    const m = Math.floor(data.size / 2) * ZOOM;
    ctx.lineWidth = 1;
    ctx.strokeStyle = "#000"; ctx.strokeRect(m - 0.5, m - 0.5, ZOOM + 1, ZOOM + 1);
    ctx.strokeStyle = "#fff"; ctx.strokeRect(m + 0.5, m + 0.5, ZOOM - 1, ZOOM - 1);
  }, [data, side]);

  const phys = `${Math.round(gx * data.scale)}, ${Math.round(gy * data.scale)}`;
  return (
    <div className="loupe" style={{ left, top, width: side }}>
      <canvas ref={ref} width={side} height={side} />
      <div className="loupe-meta">
        <span>{Math.round(gx)}, {Math.round(gy)}</span>
        <span className="px">{phys} px</span>
        <span>{centreHex(data)}</span>
      </div>
    </div>
  );
}
