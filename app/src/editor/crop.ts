// Crop in the editor: which handle a press is on, and keeping the frame inside the image.
// Rect is [x, y, w, h] in IMAGE pixels, the same convention as the selector (../overlay/reshape).
import { GRIPS, Grip, Rect, reshape } from "../overlay/reshape";

export type { Rect };

/** Smallest crop, in image pixels. */
export const MIN_CROP = 8;

/** What a press at (x, y) grabs: a corner or edge (resize), the inside (move), or nothing (draw a
 *  new frame). `tol` is how far from an edge still counts as on it, in image pixels. */
export function gripAt(r: Rect, x: number, y: number, tol: number): string | null {
  const [rx, ry, rw, rh] = r;
  const nearL = Math.abs(x - rx) <= tol, nearR = Math.abs(x - (rx + rw)) <= tol;
  const nearT = Math.abs(y - ry) <= tol, nearB = Math.abs(y - (ry + rh)) <= tol;
  const inX = x >= rx - tol && x <= rx + rw + tol, inY = y >= ry - tol && y <= ry + rh + tol;
  if (!inX || !inY) return null;
  const v = nearT ? "n" : nearB ? "s" : "";
  const h = nearL ? "w" : nearR ? "e" : "";
  if (v || h) return v + h;
  return "move";
}

/** CSS cursor for a grip (or the crosshair for drawing a new frame). */
export function gripCursor(g: string | null): string {
  switch (g) {
    case "nw": case "se": return "nwse-resize";
    case "ne": case "sw": return "nesw-resize";
    case "n": case "s": return "ns-resize";
    case "e": case "w": return "ew-resize";
    case "move": return "move";
    default: return "crosshair";
  }
}

/** Keep a frame inside the W x H image and at least MIN_CROP on each side. A moved frame keeps its
 *  size and is pushed back inside rather than squashed. */
export function clampRect(r: Rect, W: number, H: number, moving = false): Rect {
  let [x, y, w, h] = r;
  if (moving) {
    w = Math.min(Math.max(w, MIN_CROP), W);
    h = Math.min(Math.max(h, MIN_CROP), H);
    x = Math.min(Math.max(x, 0), W - w);
    y = Math.min(Math.max(y, 0), H - h);
  } else {
    // Cut each side at the image edge it crossed (from the frame's REAL extent, before any cap).
    const x2 = Math.min(x + w, W), y2 = Math.min(y + h, H);
    x = Math.max(x, 0); y = Math.max(y, 0);
    w = Math.max(MIN_CROP, x2 - x); h = Math.max(MIN_CROP, y2 - y);
    if (x + w > W) x = Math.max(0, W - w);
    if (y + h > H) y = Math.max(0, H - h);
    w = Math.min(w, W); h = Math.min(h, H);
  }
  return [Math.round(x), Math.round(y), Math.round(w), Math.round(h)];
}

/** Drag `grip` of `start` by (dx, dy) and keep the result inside the image. */
export function dragCrop(start: Rect, grip: string, dx: number, dy: number, W: number, H: number): Rect {
  const g: Grip | undefined = GRIPS[grip];
  if (!g) return start;
  return clampRect(reshape(start, g, dx, dy), W, H, !!g.move);
}

/** A new frame drawn from (x0, y0) to (x1, y1). */
export function drawnCrop(x0: number, y0: number, x1: number, y1: number, W: number, H: number): Rect {
  return clampRect([Math.min(x0, x1), Math.min(y0, y1), Math.abs(x1 - x0), Math.abs(y1 - y0)], W, H);
}

/** The whole image -- applying it means "no crop". */
export const isFull = (r: Rect, W: number, H: number) => r[0] <= 0 && r[1] <= 0 && r[2] >= W && r[3] >= H;
