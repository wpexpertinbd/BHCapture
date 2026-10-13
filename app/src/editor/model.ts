// The editor's annotation model and renderer. Pure functions over a canvas, so the SAME code draws
// the on-screen view and the exported file -- what you see is exactly what gets saved.

export type Tool = "select" | "arrow" | "darrow" | "line" | "ellipse" | "rect" | "text" | "highlight" | "pixelate" | "step";

/** Drawn from a start point to an end point (hit and selected along the line, not as a box). */
export const isLineKind = (k: Tool) => k === "arrow" || k === "darrow" || k === "line";

/** A perfect square/circle from a drag: the shorter side follows the longer (Shift). */
export function squared(s: Shape): Shape {
  const dx = s.x2 - s.x1, dy = s.y2 - s.y1;
  const d = Math.max(Math.abs(dx), Math.abs(dy));
  return { ...s, x2: s.x1 + Math.sign(dx || 1) * d, y2: s.y1 + Math.sign(dy || 1) * d };
}

export type Shape = {
  id: number;
  kind: Exclude<Tool, "select">;
  // Image-pixel coordinates. For arrow: start (x1,y1) -> tip (x2,y2). For boxes: two corners.
  x1: number; y1: number; x2: number; y2: number;
  color: string;
  width: number;     // stroke width in POINTS; multiplied by the capture's scale when drawn
  text?: string;
  n?: number;        // step number
};

export const COLORS = ["#ff3b30", "#ff9500", "#ffcc00", "#34c759", "#0a84ff", "#1c1c1e", "#ffffff"];
export const WIDTHS = [2, 4, 7];

export function box(s: Shape) {
  return { x: Math.min(s.x1, s.x2), y: Math.min(s.y1, s.y2), w: Math.abs(s.x2 - s.x1), h: Math.abs(s.y2 - s.y1) };
}

/** Text and steps are points, not boxes; give them a real extent so they can be hit and selected. */
export function bounds(s: Shape, k: number, ctx?: CanvasRenderingContext2D) {
  if (s.kind === "step") {
    const r = 15 * k;
    return { x: s.x1 - r, y: s.y1 - r, w: 2 * r, h: 2 * r };
  }
  if (s.kind === "text") {
    const size = textSize(s, k);
    let w = (s.text ?? "").length * size * 0.6;
    if (ctx) { ctx.save(); ctx.font = font(size); w = ctx.measureText(s.text ?? "").width; ctx.restore(); }
    const pad = 6 * k;
    return { x: s.x1 - pad, y: s.y1 - pad, w: w + 2 * pad, h: size * 1.25 + 2 * pad };
  }
  return box(s);
}

const textSize = (s: Shape, k: number) => (14 + s.width * 3) * k;
const font = (px: number) => `600 ${px}px -apple-system, BlinkMacSystemFont, "Helvetica Neue", sans-serif`;

function distToSegment(px: number, py: number, s: Shape) {
  const dx = s.x2 - s.x1, dy = s.y2 - s.y1;
  const len2 = dx * dx + dy * dy || 1;
  const t = Math.max(0, Math.min(1, ((px - s.x1) * dx + (py - s.y1) * dy) / len2));
  return Math.hypot(px - (s.x1 + t * dx), py - (s.y1 + t * dy));
}

/** Topmost shape under a point, or null. */
export function hitTest(shapes: Shape[], x: number, y: number, k: number, ctx?: CanvasRenderingContext2D): Shape | null {
  for (let i = shapes.length - 1; i >= 0; i--) {
    const s = shapes[i];
    if (isLineKind(s.kind)) {
      if (distToSegment(x, y, s) <= (s.width * k) / 2 + 8 * k) return s;
      continue;
    }
    if (s.kind === "ellipse") {
      // Hit on the OUTLINE, like a box: inside it, the click goes to what is underneath.
      const b = box(s);
      const rx = b.w / 2 || 1, ry = b.h / 2 || 1;
      const nx = (x - (b.x + rx)) / rx, ny = (y - (b.y + ry)) / ry;
      const r = Math.hypot(nx, ny);
      const tol = ((s.width * k) / 2 + 8 * k) / Math.min(rx, ry);
      if (Math.abs(r - 1) <= tol) return s;
      continue;
    }
    const b = bounds(s, k, ctx);
    const pad = 6 * k;
    if (s.kind === "rect") {
      // A rectangle is hit on its OUTLINE, so clicking inside it selects what is underneath.
      const inOuter = x >= b.x - pad && x <= b.x + b.w + pad && y >= b.y - pad && y <= b.y + b.h + pad;
      const inInner = x > b.x + pad && x < b.x + b.w - pad && y > b.y + pad && y < b.y + b.h - pad;
      if (inOuter && !inInner) return s;
      continue;
    }
    if (x >= b.x && x <= b.x + b.w && y >= b.y && y <= b.y + b.h) return s;
  }
  return null;
}

export function moved(s: Shape, dx: number, dy: number): Shape {
  return { ...s, x1: s.x1 + dx, y1: s.y1 + dy, x2: s.x2 + dx, y2: s.y2 + dy };
}

/**
 * Draw the capture and its annotations.
 *
 * PIXELATE IS BAKED IN: it is drawn from the ORIGINAL pixels, scaled down and back up, and it
 * REPLACES those pixels in the canvas. The exported file is rendered by this same function, so the
 * hidden content is genuinely gone from the saved image -- not covered by a layer someone could
 * remove. Pixelate regions are drawn first so later annotations stay crisp on top of them.
 */
export function render(ctx: CanvasRenderingContext2D, base: CanvasImageSource, W: number, H: number,
                       shapes: Shape[], k: number, selectedId: number | null, forExport: boolean) {
  ctx.save();
  ctx.clearRect(0, 0, W, H);
  ctx.globalCompositeOperation = "source-over";
  ctx.drawImage(base, 0, 0, W, H);

  for (const s of shapes.filter((s) => s.kind === "pixelate")) {
    const b = box(s);
    if (b.w < 1 || b.h < 1) continue;
    // Coarse on purpose: small blocks over text can be partly reversed (depixelation tools match
    // the block averages against a known font). 12 points minimum, or a sixth of the box.
    const block = Math.max(12 * k, Math.min(b.w, b.h) / 6);
    const tw = Math.max(1, Math.round(b.w / block)), th = Math.max(1, Math.round(b.h / block));
    const small = new OffscreenCanvas(tw, th);
    const sc = small.getContext("2d")!;
    sc.imageSmoothingEnabled = true;
    sc.drawImage(base, b.x, b.y, b.w, b.h, 0, 0, tw, th);
    ctx.imageSmoothingEnabled = false;
    ctx.drawImage(small, 0, 0, tw, th, b.x, b.y, b.w, b.h);
    ctx.imageSmoothingEnabled = true;
  }

  for (const s of shapes) {
    if (s.kind === "pixelate") continue;
    const lw = s.width * k;
    ctx.strokeStyle = s.color;
    ctx.fillStyle = s.color;
    ctx.lineWidth = lw;
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    if (s.kind === "rect") {
      const b = box(s);
      ctx.strokeRect(b.x, b.y, b.w, b.h);
    } else if (s.kind === "highlight") {
      const b = box(s);
      ctx.save();
      ctx.globalAlpha = 0.38;
      ctx.globalCompositeOperation = "multiply";
      ctx.fillRect(b.x, b.y, b.w, b.h);
      ctx.restore();
    } else if (s.kind === "ellipse") {
      const b = box(s);
      ctx.beginPath();
      ctx.ellipse(b.x + b.w / 2, b.y + b.h / 2, Math.max(b.w / 2, 0.5), Math.max(b.h / 2, 0.5), 0, 0, Math.PI * 2);
      ctx.stroke();
    } else if (isLineKind(s.kind)) {
      const ang = Math.atan2(s.y2 - s.y1, s.x2 - s.x1);
      const head = Math.max(14 * k, lw * 3.6);
      const atEnd = s.kind !== "line", atStart = s.kind === "darrow";
      // Stop the shaft short of each head, so a thick line does not poke through the point.
      const ex = atEnd ? s.x2 - Math.cos(ang) * head * 0.85 : s.x2, ey = atEnd ? s.y2 - Math.sin(ang) * head * 0.85 : s.y2;
      const sx = atStart ? s.x1 + Math.cos(ang) * head * 0.85 : s.x1, sy = atStart ? s.y1 + Math.sin(ang) * head * 0.85 : s.y1;
      ctx.beginPath(); ctx.moveTo(sx, sy); ctx.lineTo(ex, ey); ctx.stroke();
      const tip = (x: number, y: number, a: number) => {
        ctx.beginPath();
        ctx.moveTo(x, y);
        ctx.lineTo(x - head * Math.cos(a - 0.45), y - head * Math.sin(a - 0.45));
        ctx.lineTo(x - head * Math.cos(a + 0.45), y - head * Math.sin(a + 0.45));
        ctx.closePath(); ctx.fill();
      };
      if (atEnd) tip(s.x2, s.y2, ang);
      if (atStart) tip(s.x1, s.y1, ang + Math.PI);
    } else if (s.kind === "step") {
      const r = 15 * k;
      ctx.beginPath(); ctx.arc(s.x1, s.y1, r, 0, Math.PI * 2); ctx.fill();
      ctx.lineWidth = 2.5 * k; ctx.strokeStyle = "#ffffff"; ctx.stroke();
      ctx.fillStyle = isLight(s.color) ? "#1c1c1e" : "#ffffff";
      ctx.font = font(16 * k); ctx.textAlign = "center"; ctx.textBaseline = "middle";
      ctx.fillText(String(s.n ?? 1), s.x1, s.y1 + 1 * k);
    } else if (s.kind === "text") {
      const size = textSize(s, k);
      ctx.font = font(size); ctx.textAlign = "left"; ctx.textBaseline = "top";
      // A contrasting outline keeps text readable on any background.
      ctx.lineWidth = Math.max(3 * k, size / 7);
      ctx.strokeStyle = isLight(s.color) ? "rgba(0,0,0,0.85)" : "rgba(255,255,255,0.95)";
      ctx.strokeText(s.text ?? "", s.x1, s.y1);
      ctx.fillText(s.text ?? "", s.x1, s.y1);
    }
  }

  // Selection chrome is for the screen only, never in the saved file.
  if (!forExport && selectedId !== null) {
    const s = shapes.find((x) => x.id === selectedId);
    if (s) {
      const b = isLineKind(s.kind) || s.kind === "ellipse" ? box(s) : bounds(s, k, ctx);
      const pad = 5 * k;
      ctx.setLineDash([6 * k, 4 * k]);
      ctx.lineWidth = 1.5 * k;
      ctx.strokeStyle = "#0a84ff";
      ctx.strokeRect(b.x - pad, b.y - pad, b.w + 2 * pad, b.h + 2 * pad);
    }
  }
  ctx.restore();
}

function isLight(hex: string) {
  const n = parseInt(hex.slice(1), 16);
  const r = (n >> 16) & 255, g = (n >> 8) & 255, b = n & 255;
  return 0.299 * r + 0.587 * g + 0.114 * b > 170;
}
