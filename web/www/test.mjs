// End-to-end checks of the browser build in Node: the real wasm, the page's own
// WASI layer and engine wrapper, the game folder copied into the in-memory file
// system. Golden hashes are the ones checked by hand for the native harness
// (harness/01, harness/02, tests/e2e.rs battle_menu_saves_and_loads_any_time).
//
//   node web/www/test.mjs <game folder> <eiketsu_web.wasm> [fixture folder]
//
// The fixture folder (harness/fixtures/sishui_hall) replaces slot 2 for the
// save/load case, as the native e2e tests do.
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { MemFS } from "./wasi.js";
import { startEngine, screenHash } from "./engine.js";

const [game, wasmPath, fixture] = process.argv.slice(2);
const wasm = readFileSync(wasmPath);
const fails = [];

async function boot(withFixture) {
  const fs = new MemFS();
  for (const name of readdirSync(game)) {
    if (!statSync(join(game, name)).isFile()) continue;
    if (withFixture && /^[ME]SAVE[1-9]\.R3S$/i.test(name)) continue; // only the fixture's saves
    fs.writeFile(name, new Uint8Array(readFileSync(join(game, name))));
  }
  if (withFixture) for (const f of readdirSync(fixture)) fs.writeFile(f, new Uint8Array(readFileSync(join(fixture, f))));
  const lines = [];
  const eng = await startEngine(wasm, fs, (l) => lines.push(l));
  // Game time is tracked here; events land on exact ms like the native scripts.
  const s = { eng, fs, t: 0, peak: 0, lines };
  s.until = (ms) => {
    while (s.t < ms) {
      const step = Math.min(16, ms - s.t);
      if (eng.run(step) !== "running") throw new Error("game ended at " + s.t);
      s.t += step;
      s.peak = Math.max(s.peak, ...eng.audio(48000, 256).map(Math.abs));
    }
  };
  s.click = (ms, x, y, b = 1) => {
    s.until(ms); eng.mouse(x, y, 0);
    s.until(ms + 30); eng.mouse(x, y, b);
    s.until(ms + 150); eng.mouse(x, y, 0);
  };
  s.key = (ms, sc) => {
    s.until(ms); eng.key(sc, true);
    s.until(ms + 80); eng.key(sc, false);
  };
  s.expect = (name, want) => {
    const got = screenHash(eng.frame());
    if (got !== want) fails.push(`${name}: expected ${want}, got ${got}`);
  };
  return s;
}

async function check(name, f) {
  const t0 = performance.now();
  try {
    await f();
    console.log(`${name}: ran in ${((performance.now() - t0) / 1000).toFixed(1)} s`);
  } catch (e) {
    fails.push(`${name}: ${e.message}`);
  }
}

await check("title and prologue", async () => {
  const s = await boot(false);
  s.key(9000, 0x1e);
  s.key(9600, 0x1c);
  s.until(14000);
  s.expect("title_menu", "093a158fdd5a2c4b");
  s.click(14000, 320, 187);
  s.until(17000);
  s.expect("chapter_title", "f4d72a9cca685e35");
  if (!(s.peak > 0.01)) fails.push(`no sound (peak ${s.peak})`);
});

if (fixture) {
  // Hall: save slot 7 (keeps a machine state) → battle → 功能/讀檔 slot 7:
  // restores the machine (open files included) and lands back in the hall.
  await check("battle menu loads a hall save", async () => {
    const s = await boot(true);
    s.key(9000, 0x1e);
    s.key(9400, 0x1c);
    s.click(14000, 320, 211); // 讀取存檔
    s.click(16500, 220, 200); // slot 2
    s.click(18000, 350, 231); // 是
    s.click(25000, 222, 238);
    s.click(26500, 222, 238); // 關羽
    s.click(29000, 382, 152); // 準備好了嗎？是
    s.click(32000, 222, 231); // 現在儲存嗎？是
    s.click(35000, 280, 316); // slot 7
    s.click(37000, 290, 231); // 是
    for (let i = 0; i < 18; i++) s.click(40000 + i * 2500, 320, 300);
    s.click(86000, 45, 55); // 功能
    s.click(87500, 85, 326); // 讀檔
    s.click(89500, 280, 316); // slot 7
    s.click(91000, 247, 231); // 是 → machine state
    s.until(110000); // the load rewinds the game clock; give the hall time to settle
    if (!s.fs.lookup("savestates/slot7.state")) fails.push("slot 7 kept no machine state; wasm said: " + s.lines.join(" / "));
    // The hall's 出征！ screen, checked by hand (the cursor differs from the
    // native run's golden 31bcf098b6299709, so the wasm run has its own).
    s.expect("hall_again", "362bf95262794369");
  });
}

if (fails.length) {
  console.error("FAIL\n" + fails.join("\n"));
  process.exit(1);
}
console.log("ok");
