// How much game time each animation frame runs. Pure logic, checked by
// test-pace.mjs.
//
// Normally a frame runs exactly the time that passed. When the device can't
// emulate in real time, running all of it makes each frame longer than the last
// (8 fps bursts that feel frozen); instead each frame gets a fixed amount of
// emulation work and the game runs a little slower, still smooth.

export const MAX_DT = 100; // a longer gap (tab hidden, debugger) counts as this
export const FRAME_BUDGET = 25; // ms of emulation a frame: about 30-40 fps when behind
export const MIN_CHUNK = 4; // game-ms a frame however slow, so the game moves on

/**
 * carry: game time owed from earlier frames (ms, < 1 when keeping up);
 * dt: real time since the last frame; costPerMs: measured ms of emulation per
 * game-ms (0: not measured yet). Returns the whole ms to run now and the new carry.
 */
export function pace(carry, dt, costPerMs) {
  const owed = carry + Math.min(dt, MAX_DT);
  const cap = costPerMs > 0 ? Math.max(MIN_CHUNK, Math.floor(FRAME_BUDGET / costPerMs)) : Infinity;
  const ms = Math.floor(owed);
  if (ms > cap) return { ms: cap, carry: 0 };
  return { ms, carry: owed - ms };
}
