// Where the game picture sits on the page, and what a touch means. Pure logic
// (no DOM) so test-view.mjs can check it with hand-worked numbers.

/** Tuning, in CSS px / ms. */
export const ZOOM_STEPS = [1, 1.5, 2, 3];
export const TOUCH_START_ZOOM = 2; // phones: the picture is too small to read or hit whole
export const MAX_ZOOM = 3;
export const HOLD_DELAY = 300; // one finger held this long without moving holds the mouse button
export const SWIPE_MIN = 30; // one finger moving this far is a swipe to the next page
export const TAP_SLOP = 12; // two-finger tap: fingers may drift this much
export const TAP_TIME = 350;

/** Size of the picture fitted whole into the stage, on the PC's shape
 *  (320-wide modes doubled, 200-line modes doubled). */
export function fitSize(stageW, stageH, picW, picH) {
  const aw = picW * (picW <= 320 ? 2 : 1), ah = picH * (picH <= 240 ? 2 : 1);
  const s = Math.min(stageW / aw, stageH / ah);
  return { w: Math.floor(aw * s), h: Math.floor(ah * s) };
}

/** Clamp a view {z, x, y} (x/y: picture's top-left in stage px) so a picture
 *  smaller than the stage is centred and a bigger one leaves no gap. */
export function clampView(v, stageW, stageH, base) {
  const z = Math.min(MAX_ZOOM, Math.max(1, v.z));
  const w = base.w * z, h = base.h * z;
  const axis = (pos, size, room) => (size <= room ? (room - size) / 2 : Math.min(0, Math.max(room - size, pos)));
  return { z, x: axis(v.x, w, stageW), y: axis(v.y, h, stageH) };
}

/** Zoom to z keeping stage point (px, py) on the same picture point. */
export function zoomAt(v, z, px, py, stageW, stageH, base) {
  const k = Math.min(MAX_ZOOM, Math.max(1, z)) / v.z;
  return clampView({ z: v.z * k, x: px - (px - v.x) * k, y: py - (py - v.y) * k }, stageW, stageH, base);
}

/** The view after the stage or the fitted picture changes size (the game
 *  switching screen mode, the phone turning): the picture point in the middle
 *  of the stage stays in the middle. Not clamped. */
export function refit(v, oldStage, oldBase, stage, base) {
  const axis = (pos, oldRoom, oldSize, room, size) => room / 2 - ((oldRoom / 2 - pos) / oldSize) * size;
  return {
    z: v.z,
    x: axis(v.x, oldStage.w, oldBase.w * v.z, stage.w, base.w * v.z),
    y: axis(v.y, oldStage.h, oldBase.h * v.z, stage.h, base.h * v.z),
  };
}

/** Stage point → picture pixel (picW×picH), clamped to the picture. */
export function picturePoint(v, base, picW, picH, sx, sy) {
  const px = Math.floor(((sx - v.x) * picW) / (base.w * v.z)), py = Math.floor(((sy - v.y) * picH) / (base.h * v.z));
  return { x: Math.min(picW - 1, Math.max(0, px)), y: Math.min(picH - 1, Math.max(0, py)) };
}

/** The view a game starts in: on touch screens zoomed, on the page nearest the middle. */
export function startView(stageW, stageH, base, touch) {
  const whole = clampView({ z: 1, x: 0, y: 0 }, stageW, stageH, base);
  if (!touch) return whole;
  return snapView(zoomAt(whole, TOUCH_START_ZOOM, stageW / 2, stageH / 2, stageW, stageH, base), stageW, stageH, base);
}

// Pages: the zoomed picture cut into screen-sized pieces a swipe moves between.
// Neighbours overlap by at least half a screen, so anything up to half a screen
// big (a menu in the middle, say) is whole on some page.

/** Positions of the picture's edge for each page along one axis. */
function pages(size, room) {
  if (size <= room + 0.5) return [(room - size) / 2];
  const k = Math.max(2, Math.ceil((2 * size) / room - 1e-9) - 1);
  return Array.from({ length: k }, (_, i) => ((room - size) * i) / (k - 1) + 0); // + 0: no -0
}
/** The page nearest pos (a tie goes to the later page). */
function nearest(pos, ps) {
  let best = 0;
  ps.forEach((p, i) => {
    if (Math.abs(p - pos) <= Math.abs(ps[best] - pos) + 1e-9) best = i;
  });
  return best;
}
function axes(v, stageW, stageH, base) {
  const xs = pages(base.w * v.z, stageW), ys = pages(base.h * v.z, stageH);
  return { xs, ys, col: nearest(v.x, xs), row: nearest(v.y, ys) };
}

/** Page count and the page the view is on. */
export function pageInfo(v, stageW, stageH, base) {
  const a = axes(v, stageW, stageH, base);
  return { cols: a.xs.length, rows: a.ys.length, col: a.col, row: a.row };
}
/** The view moved onto its nearest page. */
export function snapView(v, stageW, stageH, base) {
  const a = axes(v, stageW, stageH, base);
  return { z: v.z, x: a.xs[a.col], y: a.ys[a.row] };
}
/** The view moved dc pages right and dr pages down (held at the edges). */
export function swipeView(v, dc, dr, stageW, stageH, base) {
  const a = axes(v, stageW, stageH, base);
  const c = Math.min(a.xs.length - 1, Math.max(0, a.col + dc)), r = Math.min(a.ys.length - 1, Math.max(0, a.row + dr));
  return { z: v.z, x: a.xs[c], y: a.ys[r] };
}

/** The next step of the zoom button after z. */
export function nextZoom(z) {
  const i = ZOOM_STEPS.findIndex((s) => s > z + 0.01);
  return i < 0 ? ZOOM_STEPS[0] : ZOOM_STEPS[i];
}

/**
 * Touches → game actions. Feed it pointer events with a clock; it returns
 * what to do:
 *   {kind:"press", x, y} / {kind:"drag", x, y} / {kind:"release", x, y}  (stage px)
 *   {kind:"swipe", col, row}            one finger swept: go a page right/down (-1: left/up)
 *   {kind:"right"}                     two-finger tap
 *   {kind:"pinch", scale, cx, cy, dx, dy}  since the last pinch action
 * One finger: a tap clicks where it landed; moving SWIPE_MIN is a swipe; held
 * still for HOLD_DELAY (call timeout(t) when it passes) it holds the button and
 * then drags. A second finger makes it a pinch / two-finger tap instead.
 */
export class Touches {
  constructor() {
    this.pts = new Map();
    this.mode = "idle"; // idle | pending | swipe | press | gesture
    this.start = null;
  }
  down(id, x, y, t) {
    this.pts.set(id, { x, y });
    const out = [];
    if (this.pts.size === 1) {
      this.mode = "pending";
      this.downAt = t;
      this.downPt = { x, y };
    } else if (this.pts.size === 2) {
      if (this.mode === "press") {
        const [p] = this.pts.values();
        out.push({ kind: "release", x: p.x, y: p.y });
      }
      this.mode = "gesture";
      const [a, b] = this.pts.values();
      this.start = { t, a: { ...a }, b: { ...b } };
      this.moved = false;
      this.last = pair(a, b);
    }
    return out;
  }
  timeout(t) {
    if (this.mode !== "pending" || t - this.downAt < HOLD_DELAY) return [];
    this.mode = "press";
    const [p] = this.pts.values();
    return [{ kind: "press", x: p.x, y: p.y }];
  }
  move(id, x, y, t) {
    if (!this.pts.has(id)) return [];
    this.pts.set(id, { x, y });
    if (this.mode === "pending" && dist({ x, y }, this.downPt) >= SWIPE_MIN) this.mode = "swipe";
    if (this.mode === "press") return [{ kind: "drag", x, y }];
    if (this.mode === "gesture" && this.pts.size >= 2) {
      const [a, b] = this.pts.values();
      const s = this.start;
      if (Math.max(dist(a, s.a), dist(b, s.b)) > TAP_SLOP) this.moved = true;
      const now = pair(a, b);
      const out = { kind: "pinch", scale: now.d / this.last.d, cx: now.cx, cy: now.cy, dx: now.cx - this.last.cx, dy: now.cy - this.last.cy };
      this.last = now;
      return [out];
    }
    return [];
  }
  up(id, x, y, t) {
    if (!this.pts.has(id)) return [];
    this.pts.set(id, { x, y });
    const out = [];
    const d = this.downPt;
    if (this.mode === "pending" && dist({ x, y }, d) >= SWIPE_MIN) this.mode = "swipe";
    if (this.mode === "pending") out.push({ kind: "press", x: d.x, y: d.y }, { kind: "release", x: d.x, y: d.y });
    else if (this.mode === "swipe") {
      // The finger drags the picture: moving left shows the page on the right.
      const dx = x - d.x, dy = y - d.y;
      out.push(Math.abs(dx) >= Math.abs(dy) ? { kind: "swipe", col: -Math.sign(dx), row: 0 } : { kind: "swipe", col: 0, row: -Math.sign(dy) });
    } else if (this.mode === "press") out.push({ kind: "release", x, y });
    else if (this.mode === "gesture" && this.start && this.pts.size === 2) {
      if (!this.moved && t - this.start.t <= TAP_TIME) out.push({ kind: "right" });
      this.start = null; // decided once, when the first of the two lifts
    }
    this.pts.delete(id);
    if (this.pts.size === 0) this.mode = "idle";
    return out;
  }
}

const dist = (p, q) => Math.hypot(p.x - q.x, p.y - q.y);
const pair = (a, b) => ({ d: Math.max(1, dist(a, b)), cx: (a.x + b.x) / 2, cy: (a.y + b.y) / 2 });
