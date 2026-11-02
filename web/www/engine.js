// Loads eiketsu_web.wasm on a MemFS holding the game folder and wraps its
// exports. Shared by the page (app.js) and the Node test (test.mjs).
import { WASI } from "./wasi.js";

export async function startEngine(wasmBytes, fs, log = console.log) {
  const wasi = new WASI(fs, { stdout: log, stderr: log });
  const { instance } = await WebAssembly.instantiate(wasmBytes, { wasi_snapshot_preview1: wasi.imports });
  const x = instance.exports;
  wasi.memory = x.memory;
  if (x._initialize) x._initialize();
  const error = () => new TextDecoder().decode(new Uint8Array(x.memory.buffer, x.eik_error_ptr(), x.eik_error_len()));
  if (x.eik_start() !== 0) throw new Error(error());
  return {
    /** Run `ms` of game time. "running" | "exited"; throws on an emulator error. */
    run(ms) {
      const r = x.eik_run(ms);
      if (r === 2) throw new Error(error());
      return r === 0 ? "running" : "exited";
    },
    /** { width, height, rgba: Uint8ClampedArray } (a view: copy before the next run). */
    frame() {
      const ptr = x.eik_frame();
      const width = x.eik_width(), height = x.eik_height();
      return { width, height, rgba: new Uint8ClampedArray(x.memory.buffer, ptr, width * height * 4) };
    },
    /** Mouse in picture pixels; buttons: 1 left, 2 right. */
    mouse(px, py, buttons) {
      x.eik_mouse(px | 0, py | 0, buttons);
    },
    key(scancode, down) {
      x.eik_key(scancode, down ? 1 : 0);
    },
    /** Interleaved stereo float samples (a view). */
    audio(rate, frames) {
      return new Float32Array(x.memory.buffer, x.eik_audio(rate, frames), frames * 2);
    },
    fsWrites() {
      return x.eik_fs_writes();
    },
  };
}

/** Same FNV-1a hash as Machine::screen_hash (golden values are shared). */
export function screenHash({ width, height, rgba }) {
  const M = (1n << 64n) - 1n;
  let h = 0xcbf29ce484222325n ^ ((BigInt(width) << 32n) ^ BigInt(height));
  for (let i = 0; i < rgba.length; i += 4) {
    h ^= BigInt((rgba[i] << 16) | (rgba[i + 1] << 8) | rgba[i + 2]);
    h = (h * 0x100000001b3n) & M;
  }
  return h.toString(16).padStart(16, "0");
}
