/** Global logical rect: [x, y, w, h]. */
export type Rect = [number, number, number, number];

/** Smallest selection a resize can shrink to, in points. */
const MIN_SIDE = 4;

/** Which sides a press on the selection moves: a corner moves two, an edge one, inside moves all. */
export type Grip = { l?: boolean; r?: boolean; t?: boolean; b?: boolean; move?: boolean };
export const GRIPS: Record<string, Grip> = {
  nw: { l: true, t: true }, n: { t: true }, ne: { r: true, t: true }, e: { r: true },
  se: { r: true, b: true }, s: { b: true }, sw: { l: true, b: true }, w: { l: true },
  move: { move: true },
};

/** The selection after dragging `grip` by (dx, dy) from `start`. Dragging an edge past the
 *  opposite one flips it, the way every editor does, instead of producing a negative size. */
export function reshape(start: Rect, grip: Grip, dx: number, dy: number): Rect {
  let [x0, y0] = [start[0], start[1]];
  let [x1, y1] = [start[0] + start[2], start[1] + start[3]];
  if (grip.move) return [x0 + dx, y0 + dy, start[2], start[3]];
  if (grip.l) x0 += dx;
  if (grip.r) x1 += dx;
  if (grip.t) y0 += dy;
  if (grip.b) y1 += dy;
  const [l, r] = x0 <= x1 ? [x0, x1] : [x1, x0];
  const [t, b] = y0 <= y1 ? [y0, y1] : [y1, y0];
  return [l, t, Math.max(MIN_SIDE, r - l), Math.max(MIN_SIDE, b - t)];
}
