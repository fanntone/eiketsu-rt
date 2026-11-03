// How much game time each animation frame runs (pace.js). Expected values
// worked out by hand.   node web/www/test-pace.mjs
import { pace } from "./pace.js";
import { deepStrictEqual as eq } from "node:assert";

const fails = [];
const check = (name, f) => {
  try {
    f();
  } catch (e) {
    fails.push(`${name}: ${e.message}`);
  }
};

// Budget 25 ms of emulation a frame; at 0.55 ms per game-ms that is 45 game-ms.
check("keeping up: run what elapsed, keep the fraction", () => eq(pace(0.5, 16.25, 0.55), { ms: 16, carry: 0.75 }));
// 1.25 ms per game-ms: 25 / 1.25 = 20 game-ms a frame; the other 80 are dropped
// (the game runs slower) instead of piling into ever longer frames.
check("falling behind: capped, backlog dropped", () => eq(pace(0, 100, 1.25), { ms: 20, carry: 0 }));
check("a long pause counts as 100 ms", () => eq(pace(0, 500, 0.5), { ms: 50, carry: 0 }));
check("very slow: still at least 4 ms a frame", () => eq(pace(0, 50, 30), { ms: 4, carry: 0 }));
check("cost not measured yet: no cap", () => eq(pace(0, 100, 0), { ms: 100, carry: 0 }));

if (fails.length) {
  console.error("FAIL\n" + fails.join("\n"));
  process.exit(1);
}
console.log("pace ok");
