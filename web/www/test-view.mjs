// Layout and touch logic of the page (view.js). Expected values worked out by
// hand, not by the same formulas.   node web/www/test-view.mjs
import { fitSize, clampView, zoomAt, nextZoom, picturePoint, startView, pageInfo, swipeView, snapView, refit, Touches } from "./view.js";
import { deepStrictEqual as eq } from "node:assert";

const fails = [];
const check = (name, f) => {
  try {
    f();
  } catch (e) {
    fails.push(`${name}: ${e.message}`);
  }
};

// Phone held sideways: stage 600×390 left between the two button rails.
const base = { w: 520, h: 390 };
check("fit 640x480 is height-bound", () => eq(fitSize(600, 390, 640, 480), { w: 520, h: 390 }));
check("fit 320x200 is doubled", () => eq(fitSize(600, 390, 320, 200), { w: 600, h: 375 }));
check("unzoomed picture is centred", () => eq(clampView({ z: 1, x: 7, y: 9 }, 600, 390, base), { z: 1, x: 40, y: 0 }));
check("zoom below 1 is held at 1", () => eq(clampView({ z: 0.5, x: 0, y: 0 }, 600, 390, base), { z: 1, x: 40, y: 0 }));
check("2x at the middle keeps the middle", () => eq(zoomAt({ z: 1, x: 40, y: 0 }, 2, 300, 195, 600, 390, base), { z: 2, x: -220, y: -195 }));
check("2x at the corner leaves no gap", () => eq(zoomAt({ z: 1, x: 40, y: 0 }, 2, 0, 0, 600, 390, base), { z: 2, x: 0, y: 0 }));
// 520×390 shows 640×480: 1 stage px = 640/520 picture px at 1x, half that at 2x.
check("tap maps to picture at 1x", () => eq(picturePoint({ z: 1, x: 40, y: 0 }, base, 640, 480, 300, 195), { x: 320, y: 240 }));
check("tap maps to picture at 2x", () => eq(picturePoint({ z: 2, x: -220, y: -195 }, base, 640, 480, 300, 195), { x: 320, y: 240 }));
check("tap maps to picture at 2x off-centre", () => eq(picturePoint({ z: 2, x: -220, y: -195 }, base, 640, 480, 430, 260), { x: 400, y: 280 }));
check("tap outside the picture is clamped", () => eq(picturePoint({ z: 1, x: 40, y: 0 }, base, 640, 480, 5, 400), { x: 0, y: 479 }));
check("computers start whole", () => eq(startView(600, 390, base, false), { z: 1, x: 40, y: 0 }));
check("zoom button cycles", () => eq([1, 1.5, 2, 3, 1.2].map(nextZoom), [1.5, 2, 3, 1, 1.5]));

// Pages: at 2x the 1040x780 picture in a 600x390 stage. Neighbouring pages
// overlap by at least half a screen, so pages per axis = ceil(2*size/room) - 1:
// x: ceil(3.47)-1 = 3 at 0, -220, -440;  y: ceil(4)-1 = 3 at 0, -195, -390.
const mid = { z: 2, x: -220, y: -195 };
check("pages at 2x, middle", () => eq(pageInfo(mid, 600, 390, base), { cols: 3, rows: 3, col: 1, row: 1 }));
check("swipe to the next page right", () => eq(swipeView(mid, 1, 0, 600, 390, base), { z: 2, x: -440, y: -195 }));
check("no page past the last", () => eq(swipeView({ z: 2, x: -440, y: -195 }, 1, 0, 600, 390, base), { z: 2, x: -440, y: -195 }));
check("swipe to the page above", () => eq(swipeView(mid, 0, -1, 600, 390, base), { z: 2, x: -220, y: 0 }));
check("free view snaps to the nearest page", () => eq(snapView({ z: 2, x: -100, y: -300 }, 600, 390, base), { z: 2, x: 0, y: -390 }));
check("whole picture is one page", () => eq(pageInfo({ z: 1, x: 40, y: 0 }, 600, 390, base), { cols: 1, rows: 1, col: 0, row: 0 }));
check("whole picture does not swipe", () => eq(swipeView({ z: 1, x: 40, y: 0 }, 1, 0, 600, 390, base), { z: 1, x: 40, y: 0 }));
// Phone upright: stage 390x490, picture fitted 390x292 (0.609 of 640x480).
// 2x = 780x584: x pages 0, -195, -390 (ceil(4)-1 = 3); y pages 0, -94 (ceil(2.38)-1 = 2).
// Start centred (-195, -47): the y tie goes to the later page.
const up = fitSize(390, 490, 640, 480);
check("upright fit", () => eq(up, { w: 390, h: 292 }));
check("phones start at 2x on the middle page", () => eq(startView(390, 490, up, true), { z: 2, x: -195, y: -94 }));
check("upright pages", () => eq(pageInfo({ z: 2, x: -195, y: -94 }, 390, 490, up), { cols: 3, rows: 2, col: 1, row: 1 }));

// The game switches 320x200 → 640x480 (fitted 544x340 → 453x340 in a 574x340 stage):
// the picture point in the middle of the stage stays there (it was half way: 0.5).
check("new picture size keeps the middle", () =>
  eq(refit({ z: 2, x: -257, y: -170 }, { w: 574, h: 340 }, { w: 544, h: 340 }, { w: 574, h: 340 }, { w: 453, h: 340 }), { z: 2, x: -166, y: -170 }));
// Turning the phone: 600x390 → 390x490. Middle was at (300+440)/1040 = 0.7115 across, 0.5 down.
check("turning the phone keeps the middle", () =>
  eq(refit({ z: 2, x: -440, y: -195 }, { w: 600, h: 390 }, base, { w: 390, h: 490 }, { w: 390, h: 292 }), { z: 2, x: -360, y: -47 }));

check("one-finger tap clicks where the finger landed", () => {
  const t = new Touches();
  eq(t.down(1, 10, 20, 0), []);
  eq(t.move(1, 14, 22, 30), []);
  eq(t.up(1, 15, 23, 60), [{ kind: "press", x: 10, y: 20 }, { kind: "release", x: 10, y: 20 }]);
});
check("swipe left goes to the page on the right", () => {
  const t = new Touches();
  t.down(1, 200, 100, 0);
  eq(t.move(1, 150, 102, 40), []);
  eq(t.timeout(400), []); // a swipe never turns into a press
  eq(t.up(1, 140, 105, 120), [{ kind: "swipe", col: 1, row: 0 }]);
});
check("swipe up goes to the page below", () => {
  const t = new Touches();
  t.down(1, 100, 300, 0);
  t.move(1, 102, 250, 40);
  eq(t.up(1, 104, 220, 100), [{ kind: "swipe", col: 0, row: 1 }]);
});
check("long press holds the button, then drags", () => {
  const t = new Touches();
  t.down(1, 10, 20, 0);
  eq(t.timeout(100), []);
  eq(t.timeout(310), [{ kind: "press", x: 10, y: 20 }]);
  eq(t.move(1, 60, 25, 400), [{ kind: "drag", x: 60, y: 25 }]);
  eq(t.up(1, 60, 25, 450), [{ kind: "release", x: 60, y: 25 }]);
});
check("two-finger tap is a right click, no left click", () => {
  const t = new Touches();
  t.down(1, 100, 100, 0);
  eq(t.down(2, 200, 100, 30), []);
  eq(t.up(1, 102, 101, 200), [{ kind: "right" }]);
  eq(t.up(2, 200, 100, 220), []);
});
check("pinch zooms, no click", () => {
  const t = new Touches();
  t.down(1, 100, 100, 0);
  t.down(2, 200, 100, 30);
  // fingers spread to 50..250: distance 100 → 200, centre stays at 150
  eq(t.move(1, 50, 100, 60), [{ kind: "pinch", scale: 1.5, cx: 125, cy: 100, dx: -25, dy: 0 }]);
  eq(t.move(2, 250, 100, 70), [{ kind: "pinch", scale: 200 / 150, cx: 150, cy: 100, dx: 25, dy: 0 }]);
  eq(t.up(1, 50, 100, 200), []);
  eq(t.up(2, 250, 100, 210), []);
});
check("second finger during a long press releases it first", () => {
  const t = new Touches();
  t.down(1, 10, 20, 0);
  t.timeout(400);
  eq(t.down(2, 90, 20, 500), [{ kind: "release", x: 10, y: 20 }]);
});

if (fails.length) {
  console.error("FAIL\n" + fails.join("\n"));
  process.exit(1);
}
console.log("view ok");
