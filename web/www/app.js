// The page: keeps the player's game folder in IndexedDB, runs the engine from
// requestAnimationFrame, draws to a canvas, turns touch / mouse / keys into PC
// input and plays the OPL music through Web Audio.
import { MemFS } from "./wasi.js";
import { startEngine } from "./engine.js";
import { readZip, writeZip } from "./zip.js";

const $ = (id) => document.getElementById(id);

// ---------- storage (IndexedDB: path -> bytes) ----------
const db = await new Promise((ok, fail) => {
  const r = indexedDB.open("eiketsu-rt", 1);
  r.onupgradeneeded = () => r.result.createObjectStore("files");
  r.onsuccess = () => ok(r.result);
  r.onerror = () => fail(r.error);
});
function tx(mode, f) {
  return new Promise((ok, fail) => {
    const t = db.transaction("files", mode);
    const out = f(t.objectStore("files"));
    t.oncomplete = () => ok(out && out.result);
    t.onerror = () => fail(t.error);
  });
}
async function loadAll() {
  const store = db.transaction("files").objectStore("files");
  const [keys, vals] = await Promise.all(
    [store.getAllKeys(), store.getAll()].map((r) => new Promise((ok, fail) => ((r.onsuccess = () => ok(r.result)), (r.onerror = () => fail(r.error))))),
  );
  return keys.map((k, i) => [k, vals[i]]);
}
const putFiles = (files) => tx("readwrite", (s) => files.forEach(([p, d]) => s.put(d, p)));
const delFiles = (paths) => tx("readwrite", (s) => paths.forEach((p) => s.delete(p)));
const clearAll = () => tx("readwrite", (s) => s.clear());

// ---------- picking the game files ----------
const isGameFile = (p) => /(^|\/)REKO3\.COM$/i.test(p);

/** [[relativePath, bytes]] from picked files / a zip: the folder that has REKO3.COM. */
async function gatherPicked(fileList) {
  let entries = [];
  for (const f of fileList) {
    if (/\.zip$/i.test(f.name)) {
      for (const e of await readZip(await f.arrayBuffer())) entries.push([e.name, e.data]);
    } else {
      entries.push([f.webkitRelativePath || f.name, new Uint8Array(await f.arrayBuffer())]);
    }
  }
  entries = entries.map(([p, d]) => [p.replace(/\\/g, "/"), d]);
  const reko = entries.find(([p]) => isGameFile(p));
  if (!reko) {
    // Only saves (MSAVE*/ESAVE*/savestates): merge into the stored game.
    const saves = entries.filter(([p]) => /(^|\/)(MSAVE\d|ESAVE\d)[^/]*$/i.test(p) || /(^|\/)savestates\//i.test(p));
    return saves.length ? { saves: saves.map(([p, d]) => [p.replace(/^.*?(savestates\/[^/]+|[^/]+)$/i, "$1"), d]) } : { saves: [] };
  }
  const base = reko[0].slice(0, reko[0].length - "REKO3.COM".length);
  const files = [];
  for (const [p, d] of entries) {
    if (!p.startsWith(base)) continue;
    const rel = p.slice(base.length);
    // The folder itself and its savestates; not journals or other subfolders.
    if (!rel.includes("/") || /^savestates\/[^/]+$/i.test(rel)) files.push([rel, d]);
  }
  return { files };
}

async function onPicked(e) {
  const list = [...e.target.files];
  e.target.value = "";
  if (!list.length) return;
  setStatus("讀取檔案中…");
  try {
    const got = await gatherPicked(list);
    if (got.files) {
      if (!got.files.some(([p]) => /^MAIN\.EXE$/i.test(p))) throw new Error("找到 REKO3.COM，但缺少 MAIN.EXE");
      await clearAll();
      await putFiles(got.files);
      setStatus(`已匯入 ${got.files.length} 個檔案。`);
    } else if (got.saves.length) {
      await putFiles(got.saves);
      setStatus(`已匯入 ${got.saves.length} 個存檔檔案。`);
    } else throw new Error("沒有找到遊戲檔案（REKO3.COM）或存檔。");
    await showSetup();
  } catch (err) {
    setStatus("❌ " + err.message);
  }
}
$("pick").addEventListener("change", onPicked);
$("pickdir").addEventListener("change", onPicked);

function setStatus(s) {
  $("status").textContent = s;
}

async function showSetup() {
  const files = await loadAll();
  const has = files.some(([p]) => isGameFile(p));
  $("setup").hidden = false;
  $("game").hidden = true;
  $("start").disabled = !has;
  $("have").textContent = has ? `✅ 已有遊戲檔（${files.length} 個檔案，存在這個瀏覽器裡）` : "尚未匯入遊戲檔";
  $("reset").hidden = !has;
}

$("reset").addEventListener("click", async () => {
  if (!confirm("清除這個瀏覽器裡的遊戲檔與所有存檔？（無法復原，建議先匯出存檔）")) return;
  await clearAll();
  await showSetup();
});

// ---------- the game ----------
const KEYS = {
  Escape: 0x01, Digit1: 0x02, Digit2: 0x03, Digit3: 0x04, Digit4: 0x05, Digit5: 0x06, Digit6: 0x07, Digit7: 0x08,
  Digit8: 0x09, Digit9: 0x0a, Digit0: 0x0b, Minus: 0x0c, Equal: 0x0d, Backspace: 0x0e, Tab: 0x0f,
  KeyQ: 0x10, KeyW: 0x11, KeyE: 0x12, KeyR: 0x13, KeyT: 0x14, KeyY: 0x15, KeyU: 0x16, KeyI: 0x17, KeyO: 0x18, KeyP: 0x19,
  Enter: 0x1c, ControlLeft: 0x1d, KeyA: 0x1e, KeyS: 0x1f, KeyD: 0x20, KeyF: 0x21, KeyG: 0x22, KeyH: 0x23, KeyJ: 0x24,
  KeyK: 0x25, KeyL: 0x26, ShiftLeft: 0x2a, KeyZ: 0x2c, KeyX: 0x2d, KeyC: 0x2e, KeyV: 0x2f, KeyB: 0x30, KeyN: 0x31,
  KeyM: 0x32, ShiftRight: 0x36, AltLeft: 0x38, Space: 0x39,
  F1: 0x3b, F2: 0x3c, F3: 0x3d, F4: 0x3e, F5: 0x3f, F6: 0x40, F7: 0x41, F8: 0x42, F9: 0x43, F10: 0x44,
  NumpadEnter: 0x11c, Home: 0x147, ArrowUp: 0x148, PageUp: 0x149, ArrowLeft: 0x14b, ArrowRight: 0x14d,
  End: 0x14f, ArrowDown: 0x150, PageDown: 0x151, Insert: 0x152, Delete: 0x153,
};

let eng = null;
const wasmLog = []; // the emulator's last messages, shown with an error
const fs = new MemFS();
let writesSeen = 0;
const queue = []; // input, one item per frame so the game sees every press and release
let cursor = { x: 320, y: 240 };
let muted = false, gain = null, audioCtx = null;

$("start").addEventListener("click", async () => {
  // iOS only lets sound start inside the tap itself: before any await.
  try {
    audioCtx = new (window.AudioContext || window.webkitAudioContext)();
    audioCtx.resume();
  } catch (err) {
    console.warn("no audio", err);
  }
  $("start").disabled = true;
  setStatus("載入中…");
  try {
    for (const [p, d] of await loadAll()) fs.writeFile(p, d);
    // Revalidate every time: an updated build must not be shadowed by the cache.
    const wasm = await (await fetch("eiketsu_web.wasm", { cache: "no-cache" })).arrayBuffer();
    eng = await startEngine(wasm, fs, (l) => {
      console.log(l);
      wasmLog.push(l);
      if (wasmLog.length > 5) wasmLog.shift();
    });
    writesSeen = eng.fsWrites();
    startAudio();
    $("setup").hidden = true;
    $("game").hidden = false;
    fit();
    requestAnimationFrame(tick);
  } catch (err) {
    console.error(err);
    setStatus("❌ 無法啟動：" + err.message);
    $("start").disabled = false;
  }
});

function startAudio() {
  if (!audioCtx) return;
  try {
    const node = audioCtx.createScriptProcessor(2048, 0, 2);
    gain = audioCtx.createGain();
    node.onaudioprocess = (e) => {
      const L = e.outputBuffer.getChannelData(0), R = e.outputBuffer.getChannelData(1);
      if (!eng) return;
      const s = eng.audio(audioCtx.sampleRate, L.length);
      for (let i = 0; i < L.length; i++) {
        L[i] = s[2 * i];
        R[i] = s[2 * i + 1];
      }
    };
    node.connect(gain).connect(audioCtx.destination);
    audioCtx.resume();
  } catch (err) {
    console.warn("no audio", err);
  }
}

const canvas = $("screen");
const ctx2d = canvas.getContext("2d");
let last = 0, carry = 0, speedAvg = 1, persisting = false;
// For diagnosis (read from the console / test driver).
window.eikStats = { frames: 0, gameMs: 0, runMs: 0, drawMs: 0 };

function tick(now) {
  if (!last) last = now;
  const dt = Math.min(now - last, 100);
  last = now;
  carry += dt;
  const ms = Math.floor(carry);
  carry -= ms;
  applyInput();
  if (ms > 0) {
    const t0 = performance.now();
    let state;
    try {
      state = eng.run(ms);
    } catch (err) {
      showError("模擬器錯誤：" + err.message);
      return;
    }
    const cost = performance.now() - t0;
    eikStats.gameMs += ms;
    eikStats.runMs += cost;
    speedAvg = speedAvg * 0.95 + Math.min(2, dt / Math.max(cost, dt)) * 0.05;
    if (state === "exited") {
      showError("遊戲已結束。重新整理頁面就能再玩。");
      return;
    }
  }
  const d0 = performance.now();
  draw();
  eikStats.drawMs += performance.now() - d0;
  eikStats.frames++;
  $("speed").textContent = speedAvg < 0.9 ? `速度 ${Math.round(speedAvg * 100)}%` : "";
  const w = eng.fsWrites();
  if (w !== writesSeen && !persisting) {
    writesSeen = w;
    persist();
  }
  requestAnimationFrame(tick);
}

async function persist() {
  persisting = true;
  try {
    const dirty = fs.takeDirty();
    const keep = [], gone = [];
    for (const p of dirty) {
      const n = fs.lookup(p);
      if (n && !n.dir) keep.push([p, n.data.slice(0, n.size)]);
      else gone.push(p);
    }
    if (keep.length) await putFiles(keep);
    if (gone.length) await delFiles(gone);
    if (keep.some(([p]) => /SAVE\d|slot\d+\.state/i.test(p))) flash("💾 已儲存");
  } catch (err) {
    console.error(err);
    flash("⚠️ 存檔寫入瀏覽器失敗");
  }
  persisting = false;
}

let shown = { w: 0, h: 0 };
function draw() {
  const f = eng.frame();
  if (f.width !== shown.w || f.height !== shown.h) {
    canvas.width = f.width;
    canvas.height = f.height;
    shown = { w: f.width, h: f.height };
    fit();
  }
  ctx2d.putImageData(new ImageData(new Uint8ClampedArray(f.rgba), f.width, f.height), 0, 0);
}

// Display shape as on the PC: 320-wide modes doubled, 200-line modes doubled.
function fit() {
  const stage = $("stage");
  const w = shown.w || 640, h = shown.h || 480;
  const aw = w * (w <= 320 ? 2 : 1), ah = h * (h <= 240 ? 2 : 1);
  const s = Math.min(stage.clientWidth / aw, stage.clientHeight / ah);
  canvas.style.width = Math.floor(aw * s) + "px";
  canvas.style.height = Math.floor(ah * s) + "px";
}
addEventListener("resize", fit);

function showError(msg) {
  $("err").textContent = msg + (wasmLog.length ? "\n" + wasmLog.join("\n") : "");
  $("err").hidden = false;
}

let flashTimer = 0;
function flash(msg) {
  $("toast").textContent = msg;
  $("toast").hidden = false;
  clearTimeout(flashTimer);
  flashTimer = setTimeout(() => ($("toast").hidden = true), 1500);
}

// ---------- input ----------
function toPicture(e) {
  const r = canvas.getBoundingClientRect();
  return {
    x: Math.floor(((e.clientX - r.left) * (shown.w || 640)) / r.width),
    y: Math.floor(((e.clientY - r.top) * (shown.h || 480)) / r.height),
  };
}
function queueMouse(x, y, b) {
  const lastItem = queue[queue.length - 1];
  // Coalesce moves with the same buttons.
  if (lastItem && lastItem.kind === "mouse" && lastItem.b === b) queue.pop();
  queue.push({ kind: "mouse", x, y, b });
}
function applyInput() {
  const it = queue.shift();
  if (!it) return;
  if (it.kind === "mouse") {
    cursor = { x: it.x, y: it.y };
    eng.mouse(it.x, it.y, it.b);
  } else eng.key(it.sc, it.down);
}
function tapKey(sc) {
  queue.push({ kind: "key", sc, down: true }, { kind: "key", sc, down: false });
}

let touches = 0;
canvas.addEventListener("pointerdown", (e) => {
  e.preventDefault();
  canvas.setPointerCapture(e.pointerId);
  const p = toPicture(e);
  if (e.pointerType === "mouse") {
    queueMouse(p.x, p.y, e.buttons & 3);
    return;
  }
  touches++;
  if (touches === 2) {
    // Two-finger tap: right click (cancel) where the cursor is.
    queue.push({ kind: "mouse", x: cursor.x, y: cursor.y, b: 2 }, { kind: "mouse", x: cursor.x, y: cursor.y, b: 0 });
    return;
  }
  queue.push({ kind: "mouse", x: p.x, y: p.y, b: 0 }, { kind: "mouse", x: p.x, y: p.y, b: 1 });
});
canvas.addEventListener("pointermove", (e) => {
  const p = toPicture(e);
  if (e.pointerType === "mouse") queueMouse(p.x, p.y, e.buttons & 3);
  else if (touches === 1) queueMouse(p.x, p.y, 1);
});
const up = (e) => {
  const p = toPicture(e);
  if (e.pointerType === "mouse") {
    queueMouse(p.x, p.y, e.buttons & 3);
    return;
  }
  if (touches === 1) queue.push({ kind: "mouse", x: p.x, y: p.y, b: 0 });
  touches = Math.max(0, touches - 1);
};
canvas.addEventListener("pointerup", up);
canvas.addEventListener("pointercancel", up);
canvas.addEventListener("contextmenu", (e) => e.preventDefault());

addEventListener("keydown", (e) => {
  if (!eng || $("game").hidden) return;
  const sc = KEYS[e.code];
  if (sc === undefined) return;
  e.preventDefault();
  queue.push({ kind: "key", sc, down: true });
});
addEventListener("keyup", (e) => {
  if (!eng || $("game").hidden) return;
  const sc = KEYS[e.code];
  if (sc === undefined) return;
  e.preventDefault();
  queue.push({ kind: "key", sc, down: false });
});

// Toolbar.
$("b-right").addEventListener("click", () =>
  queue.push({ kind: "mouse", x: cursor.x, y: cursor.y, b: 2 }, { kind: "mouse", x: cursor.x, y: cursor.y, b: 0 }),
);
$("b-enter").addEventListener("click", () => tapKey(0x1c));
$("b-password").addEventListener("click", () => {
  tapKey(0x1e);
  tapKey(0x1c);
});
$("b-mute").addEventListener("click", () => {
  muted = !muted;
  if (gain) gain.gain.value = muted ? 0 : 1;
  $("b-mute").textContent = muted ? "🔇" : "🔊";
  if (audioCtx) audioCtx.resume();
});
$("b-full").addEventListener("click", () => {
  const el = document.documentElement;
  if (document.fullscreenElement) document.exitFullscreen();
  else if (el.requestFullscreen) el.requestFullscreen().catch(() => flash("這個瀏覽器不支援全螢幕，請用「加入主畫面」"));
  else flash("請用 Safari 的「分享 → 加入主畫面」來全螢幕遊玩");
  setTimeout(fit, 300);
});
$("b-export").addEventListener("click", () => {
  const saves = fs.files().filter(([p]) => /^(MSAVE\d|ESAVE\d)/i.test(p) || /^savestates\//i.test(p));
  if (!saves.length) return flash("還沒有存檔");
  const a = document.createElement("a");
  a.href = URL.createObjectURL(writeZip(saves));
  a.download = "eiketsuden-saves.zip";
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 5000);
});

await showSetup();
