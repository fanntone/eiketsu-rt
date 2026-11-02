//! Browser build of eiketsu-rt.
//!
//! Built for wasm32-wasip1 as a reactor. The page (web/www) gives it a WASI
//! file system holding the player's game folder at `/game`, then drives the
//! machine in time slices from requestAnimationFrame: `eik_run(ms)`, read the
//! picture with `eik_frame`, feed input with `eik_mouse` / `eik_key`, pull sound
//! with `eik_audio`, and persist the folder whenever `eik_fs_writes` changes.
//!
//! The same functions run natively, which is how tests/web_api.rs checks them.

use eiketsu_rt::host::Event;
use eiketsu_rt::machine::{Config, Machine, Stop};
use eiketsu_rt::opl::{Sink, Synth};
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub struct Web {
    pub m: Machine,
    rgba: Vec<u8>,
    size: (u32, u32),
    sink: Sink,
    synth: Option<(u32, Synth)>,
    audio: Vec<f32>,
    pub error: String,
}

thread_local! {
    static WEB: RefCell<Option<Web>> = const { RefCell::new(None) };
}

fn with<T>(f: impl FnOnce(&mut Web) -> T) -> Option<T> {
    WEB.with(|w| w.borrow_mut().as_mut().map(f))
}

/// What a slice of emulation ended with.
#[derive(Debug, PartialEq, Eq)]
pub enum Run {
    Running,
    /// The game ended (quit to DOS).
    Exited,
    Error,
}

impl Web {
    pub fn new(game: PathBuf) -> Result<Web, String> {
        let cfg = Config {
            game_dir: game,
            headless: true,
            trace: false,
            journal: None,
            replay: None,
            max_steps: None,
            script: Vec::new(),
            program: r"C:\REKO3.COM".into(),
            audio: false,
            window: None,
            control: None,
            speed: 1,
            battle_log: None,
            debug_grid: false,
        };
        let mut m = Machine::new(cfg)?;
        m.stop_on_load = true;
        // The machine hands OPL register writes to the sink (it stays attached
        // through state loads: they load memory in place); this side turns them
        // into samples.
        let sink: Sink = Arc::new(Mutex::new(Vec::new()));
        m.mem.opl.sink = Some(sink.clone());
        Ok(Web { m, rgba: Vec::new(), size: (0, 0), sink, synth: None, audio: Vec::new(), error: String::new() })
    }

    /// Run `ms` of emulated time (or less, if a save was loaded on the way).
    pub fn run(&mut self, ms: u32) -> Run {
        self.m.cfg.max_steps = Some(self.m.cpu.steps + ms as u64 * eiketsu_rt::IPS_PER_MS);
        match self.m.run() {
            Stop::Budget => Run::Running,
            Stop::Exit(_) | Stop::Closed => Run::Exited,
            Stop::Error(e) => {
                self.error = e;
                Run::Error
            }
        }
    }

    /// The current picture as RGBA bytes (size in `self.size()`).
    pub fn frame(&mut self) -> &[u8] {
        let (w, h, px) = self.m.frame();
        self.size = (w as u32, h as u32);
        self.rgba.clear();
        self.rgba.extend(px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255]));
        &self.rgba
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Mouse in picture pixels of the current mode (`frame` size) and button bits
    /// (1 left, 2 right).
    pub fn mouse(&mut self, x: i32, y: i32, buttons: u16) {
        // The current mode's picture size (as `frame` renders it), not the last
        // frame read: input may come before the page has drawn anything.
        let vga = &self.m.mem.vga;
        let (w, h) = if vga.text { (640, 400) } else { vga.size() };
        let (w, h) = (w as i32, h as i32);
        let (x, y) = (x.clamp(0, w - 1), y.clamp(0, h - 1));
        // Driver x is 0-639 whatever the mode; y is the screen row.
        self.m.apply_event(Event::Mouse { x: x * 640 / w, y, buttons });
    }

    /// Set-1 scancode (0x1xx = E0-prefixed).
    pub fn key(&mut self, sc: u16, down: bool) {
        self.m.apply_event(Event::Key { sc, down });
    }

    /// `frames` stereo samples (interleaved f32) at `rate` Hz.
    pub fn audio(&mut self, rate: u32, frames: usize) -> &[f32] {
        if self.synth.as_ref().map_or(true, |(r, _)| *r != rate) {
            self.synth = Some((rate, Synth::new(rate)));
        }
        let synth = &mut self.synth.as_mut().unwrap().1;
        for (r, v) in self.sink.lock().unwrap().drain(..) {
            synth.write(r, v);
        }
        self.audio.resize(frames * 2, 0.0);
        synth.generate(&mut self.audio);
        &self.audio
    }
}

// ---- exports for the page (wasm) ----

/// Start the game from /game. 0 = ok, else see eik_error_*.
#[no_mangle]
pub extern "C" fn eik_start() -> i32 {
    match Web::new(PathBuf::from("/game")) {
        Ok(w) => {
            WEB.with(|cell| *cell.borrow_mut() = Some(w));
            0
        }
        Err(e) => {
            ERROR.with(|s| *s.borrow_mut() = e);
            1
        }
    }
}

thread_local! {
    static ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

/// 0 running, 1 the game ended, 2 error (see eik_error_*).
#[no_mangle]
pub extern "C" fn eik_run(ms: u32) -> i32 {
    match with(|w| {
        let r = w.run(ms);
        if r == Run::Error {
            let e = w.error.clone();
            ERROR.with(|s| *s.borrow_mut() = e);
        }
        r
    }) {
        Some(Run::Running) => 0,
        Some(Run::Exited) => 1,
        _ => 2,
    }
}

#[no_mangle]
pub extern "C" fn eik_frame() -> *const u8 {
    with(|w| w.frame().as_ptr()).unwrap_or(std::ptr::null())
}

#[no_mangle]
pub extern "C" fn eik_width() -> u32 {
    with(|w| w.size().0).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn eik_height() -> u32 {
    with(|w| w.size().1).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn eik_mouse(x: i32, y: i32, buttons: u32) {
    with(|w| w.mouse(x, y, buttons as u16));
}

#[no_mangle]
pub extern "C" fn eik_key(sc: u32, down: u32) {
    with(|w| w.key(sc as u16, down != 0));
}

#[no_mangle]
pub extern "C" fn eik_audio(rate: u32, frames: u32) -> *const f32 {
    with(|w| w.audio(rate, frames as usize).as_ptr()).unwrap_or(std::ptr::null())
}

/// Changes whenever the game folder was written (time to persist it).
#[no_mangle]
pub extern "C" fn eik_fs_writes() -> u32 {
    eiketsu_rt::FS_WRITES.load(std::sync::atomic::Ordering::Relaxed) as u32
}

#[no_mangle]
pub extern "C" fn eik_error_ptr() -> *const u8 {
    ERROR.with(|s| s.borrow().as_ptr())
}

#[no_mangle]
pub extern "C" fn eik_error_len() -> u32 {
    ERROR.with(|s| s.borrow().len() as u32)
}
