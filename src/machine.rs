//! The emulated PC: CPU + bus + the BIOS/DOS services implemented in Rust, on a
//! deterministic clock (time = instruction count / IPS_PER_MS).
//!
//! Every interrupt vector starts out pointing at a stub in the F000 segment:
//! `FE 38 nn` (host callback nn) followed by `RETF 2`, so a handler's flags reach the
//! caller like a real DOS/BIOS handler's do. Programs that hook a vector and chain
//! to the old one end up in the same stub. The timer and keyboard IRQ stubs are
//! real code around the callback (`int 1Ch`, EOI) so hooks behave as on a PC.

use crate::cpu::{Cpu, Exit, CF, CS, DS, EAX, EBX, ECX, EDI, EDX, ES, ESI, ESP, IF, SS, ZF};
use crate::dos::Dos;
use crate::host::{Event, Host};
use crate::mem::Mem;
use crate::script::{ScriptAction, ScriptEvent};
use crate::IPS_PER_MS;
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;

pub const BIOS_SEG: u16 = 0xf000;
const STUB_TIMER: u16 = 0x0800;
const STUB_KBD: u16 = 0x0818;
const STUB_IRQ_MASTER: u16 = 0x0828;
const STUB_IRQ_SLAVE: u16 = 0x0830;
const STUB_IRET: u16 = 0x0840;
const STUB_CRIT: u16 = 0x0844;
/// Return point of the mouse event handler: callback (restore registers) + IRET.
const STUB_MOUSE: u16 = 0x0848;
const FONT8_OFF: u16 = 0xfa6e;
const FONT16_OFF: u16 = 0xa000;

pub type R<T> = Result<T, String>;

pub struct Config {
    /// Folder with REKO3.COM and the game data; it is drive C:'s root.
    pub game_dir: PathBuf,
    pub headless: bool,
    pub trace: bool,
    pub journal: Option<PathBuf>,
    pub replay: Option<PathBuf>,
    pub max_steps: Option<u64>,
    pub script: Vec<ScriptEvent>,
    /// Program to start (DOS path on C:).
    pub program: String,
    pub audio: bool,
    /// Window placement (x, y, width, height); borderless when given.
    pub window: Option<(i32, i32, usize, usize)>,
    /// Localhost TCP port for remote control (see control.rs).
    pub control: Option<u16>,
    /// Emulated time runs at this multiple of real time in a window.
    pub speed: u64,
    /// Folder for the battle log: unit changes, clicks as tiles, a save state and
    /// a grid screenshot at the start of every turn.
    pub battle_log: Option<PathBuf>,
    /// Draw the tile grid over the window picture.
    pub debug_grid: bool,
}

#[derive(Debug)]
pub enum Stop {
    Exit(i32),
    Closed,
    Budget,
    Error(String),
}

#[derive(Default, Clone)]
pub struct Mouse {
    pub x: i32,
    pub y: i32,
    pub buttons: u16,
    pub shown: i32,
    pub xr: (i32, i32),
    pub yr: (i32, i32),
    press: [(u16, i32, i32); 3],
    release: [(u16, i32, i32); 3],
    pub hot: (i16, i16),
    /// AND / XOR masks of the graphics cursor (16 rows each).
    pub masks: Option<([u16; 16], [u16; 16])>,
    mick: (i32, i32),
    /// User event handler (INT 33h AX=0Ch): call mask, segment, offset.
    pub handler: Option<(u16, u16, u16)>,
    /// Event bits not yet delivered to the handler.
    pending: u16,
    /// Registers of the interrupted code while the handler runs.
    saved: Option<Box<Cpu>>,
}

pub struct Machine {
    pub cpu: Cpu,
    pub mem: Mem,
    pub cfg: Config,
    pub dos: Dos,
    pub mouse: Mouse,
    pub host: Option<Host>,
    pub control: Option<crate::control::Control>,
    pub audio: Option<crate::audio::Audio>,
    /// (checkpoint, expected, actual) screen hashes.
    pub checks: Vec<(String, u64, u64)>,
    pub stop: Option<Stop>,
    pub frames: u64,
    /// CS:IP of the last instructions executed (for error reports).
    history: [(u16, u16); 64],
    /// EIKETSU_DUMP=STEP,N: at STEP write dump.bin (RAM + VGA planes) and the
    /// registers of the next N instructions to trace.txt (CPU differential testing).
    dbg_dump: Option<(u64, u64)>,
    /// EIKETSU_WATCH=addr,addr (hex linear): report every instruction that changes these bytes.
    dbg_watch: Vec<u32>,
    /// EIKETSU_BREAK=seg:off,seg:off: print the registers whenever one is reached.
    dbg_break: Vec<(u16, u16)>,
    dbg_trace: Option<std::io::BufWriter<std::fs::File>>,
    pub tty: Vec<u8>,
    journal_file: Option<std::fs::File>,
    /// Every journal line, also kept in memory so snapshots can roll it back.
    journal_mem: Vec<String>,
    next_service: u64,
    replay: VecDeque<(u64, Event)>,
    replay_drift_warned: bool,
    warned: std::collections::BTreeSet<String>,
    last_present: std::time::Instant,
    real_start: std::time::Instant,
    /// Emulated ms at `real_start` (windowed play runs emulated time at real speed).
    emu_start_ms: u64,
    blog: Option<BattleLog>,
    pub trial: bool,
    pub bot_mode: bool,
    /// Save slot (0-based) the game last loaded or saved this session: the
    /// automatic save copies its files (see saveui.rs).
    pub saveui_last: Option<u8>,
    /// The visible picture (4 planes) while a save/load list is open, put back
    /// when it closes (see saveui.rs). Part of save states.
    pub saveui_screen: Option<Vec<u8>>,
    /// Save states loaded this session (each starts a new journal file).
    loads: u32,
}

/// State of the battle log (see Config::battle_log).
struct BattleLog {
    dir: PathBuf,
    file: std::fs::File,
    next_ms: u64,
    turn: u32,
    /// Per unit-table slot: the last logged line describing it.
    last: Vec<String>,
    last_acted: Vec<bool>,
    buttons: u16,
}

fn lin(seg: u16, off: u16) -> u32 {
    ((seg as u32) << 4) + off as u32
}

impl Machine {
    pub fn new(cfg: Config) -> R<Machine> {
        let mut m = Machine {
            cpu: Cpu::new(),
            mem: Mem::new(),
            dos: Dos::new(cfg.game_dir.clone()),
            cfg,
            mouse: Mouse { shown: -1, xr: (0, 639), yr: (0, 199), ..Default::default() },
            host: None,
            control: None,
            audio: None,
            checks: Vec::new(),
            stop: None,
            frames: 0,
            history: [(0, 0); 64],
            dbg_dump: std::env::var("EIKETSU_DUMP").ok().and_then(|v| { let (a, b) = v.split_once(',')?; Some((a.parse().ok()?, b.parse().ok()?)) }),
            dbg_trace: None,
            dbg_break: std::env::var("EIKETSU_BREAK").ok().map_or(Vec::new(), |v| v.split(',').filter_map(|s| {
                let (a, b) = s.trim().split_once(':')?;
                Some((u16::from_str_radix(a, 16).ok()?, u16::from_str_radix(b, 16).ok()?))
            }).collect()),
            dbg_watch: std::env::var("EIKETSU_WATCH").ok().map_or(Vec::new(), |v| v.split(',').filter_map(|s| u32::from_str_radix(s.trim(), 16).ok()).collect()),
            tty: Vec::new(),
            journal_file: None,
            journal_mem: Vec::new(),
            next_service: 0,
            replay: VecDeque::new(),
            replay_drift_warned: false,
            warned: Default::default(),
            last_present: std::time::Instant::now(),
            real_start: std::time::Instant::now(),
            emu_start_ms: 0,
            blog: None,
            trial: false,
            bot_mode: false,
            saveui_last: None,
            saveui_screen: None,
            loads: 0,
        };
        m.mem.trace_ports = std::env::var("EIKETSU_TRACE_PORTS").is_ok();
        m.install_bios();
        m.dos.init(&mut m.mem);
        if let Some(p) = m.cfg.replay.clone() {
            m.replay = crate::journal::load(&p)?;
        }
        if let Some(p) = m.cfg.journal.clone() {
            m.journal_file = Some(crate::journal::create(&p)?);
        }
        if let Some(dir) = m.cfg.battle_log.clone() {
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let file = std::fs::File::create(dir.join("battle.log")).map_err(|e| e.to_string())?;
            m.blog = Some(BattleLog { dir, file, next_ms: 0, turn: 0, last: Vec::new(), last_acted: Vec::new(), buttons: 0 });
        }
        if !m.cfg.headless {
            m.host = Some(Host::open(640, 480, m.cfg.window));
            if m.cfg.audio {
                m.audio = crate::audio::Audio::open();
                if let Some(a) = &m.audio {
                    m.mem.opl.sink = Some(a.sink.clone());
                }
            }
        }
        if let Some(port) = m.cfg.control {
            m.control = Some(crate::control::Control::bind(port)?);
        }
        let prog = m.cfg.program.clone();
        crate::dos::start_program(&mut m, &prog)?;
        Ok(m)
    }

    fn install_bios(&mut self) {
        let base = lin(BIOS_SEG, 0);
        for n in 0..=255u16 {
            let o = n * 8;
            self.mem.write_bytes(base + o as u32, &[0xfe, 0x38, n as u8, 0xca, 0x02, 0x00, 0x90, 0x90]);
            self.set_vector(n as u8, BIOS_SEG, o);
        }
        // Timer, as the IBM BIOS does it: save DS/AX/DX, DS=40h, tick, int 1Ch, EOI,
        // restore (1Ch hooks are allowed to change DS). Keyboard: translate, EOI.
        let eoi = [0x50, 0xb0, 0x20, 0xe6, 0x20, 0x58, 0xcf];
        #[rustfmt::skip]
        let t = [
            0x1e, 0x50, 0x52,       // push ds; push ax; push dx
            0xb8, 0x40, 0x00,       // mov ax,40h
            0x8e, 0xd8,             // mov ds,ax
            0xfe, 0x38, 0x08,       // callback: tick
            0xcd, 0x1c,             // int 1Ch
            0xfa,                   // cli
            0xb0, 0x20, 0xe6, 0x20, // EOI
            0x5a, 0x58, 0x1f,       // pop dx; pop ax; pop ds
            0xcf,                   // iret
        ];
        self.mem.write_bytes(base + STUB_TIMER as u32, &t);
        let mut k = vec![0xfe, 0x38, 0x09];
        k.extend_from_slice(&eoi);
        self.mem.write_bytes(base + STUB_KBD as u32, &k);
        self.mem.write_bytes(base + STUB_IRQ_MASTER as u32, &eoi);
        self.mem.write_bytes(base + STUB_IRQ_SLAVE as u32, &[0x50, 0xb0, 0x20, 0xe6, 0xa0, 0xe6, 0x20, 0x58, 0xcf]);
        self.mem.write_bytes(base + STUB_IRET as u32, &[0xcf]);
        self.mem.write_bytes(base + STUB_CRIT as u32, &[0xb0, 0x03, 0xcf]);
        self.mem.write_bytes(base + STUB_MOUSE as u32, &[0xfe, 0x38, 0x33, 0xcf]);
        self.set_vector(0x08, BIOS_SEG, STUB_TIMER);
        self.set_vector(0x09, BIOS_SEG, STUB_KBD);
        for v in [0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f] {
            self.set_vector(v, BIOS_SEG, STUB_IRQ_MASTER);
        }
        for v in 0x70..0x78 {
            self.set_vector(v, BIOS_SEG, STUB_IRQ_SLAVE);
        }
        for v in [0x1b, 0x1c, 0x23, 0x28] {
            self.set_vector(v, BIOS_SEG, STUB_IRET);
        }
        self.set_vector(0x24, BIOS_SEG, STUB_CRIT);
        // Vectors that are data pointers or unused on an AT stay null like on a real
        // machine (programs test them for "installed").
        for v in [0x1d, 0x1e, 0x1f, 0x43, 0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67] {
            self.set_vector(v, 0, 0);
        }
        // ROM fonts: the 8x8 table at the IBM-fixed F000:FA6E (programs read it
        // directly), 8x16 for INT 10h AX=1130h. Bitmaps are the standard VGA BIOS fonts.
        self.mem.write_bytes(lin(BIOS_SEG, FONT8_OFF), include_bytes!("../assets/font8x8.bin"));
        self.mem.write_bytes(lin(BIOS_SEG, FONT16_OFF), include_bytes!("../assets/font8x16.bin"));
        // Machine id (AT) and BIOS date.
        self.mem.write_bytes(lin(BIOS_SEG, 0xfff5), b"01/01/93\0\0\xfc");
        // BIOS data area.
        let bda = 0x400;
        self.mem.wr16(bda + 0x10, 0x0021);
        self.mem.wr16(bda + 0x13, 640);
        self.mem.wr16(bda + 0x1a, 0x1e);
        self.mem.wr16(bda + 0x1c, 0x1e);
        self.mem.wr16(bda + 0x80, 0x1e);
        self.mem.wr16(bda + 0x82, 0x3e);
        self.mem.write_bytes(bda + 0x49, &[0x03]);
        self.mem.wr16(bda + 0x4a, 80);
        self.mem.wr16(bda + 0x4c, 0x1000);
        self.mem.wr16(bda + 0x63, 0x3d4);
        self.mem.write_bytes(bda + 0x84, &[24, 16, 0]);
        self.mem.write_bytes(bda + 0x87, &[0x60, 0xf9, 0x51, 0x0b]);
    }

    pub fn vector(&self, n: u8) -> (u16, u16) {
        let a = n as u32 * 4;
        (self.mem.rd16(a + 2), self.mem.rd16(a))
    }

    pub fn set_vector(&mut self, n: u8, seg: u16, off: u16) {
        let a = n as u32 * 4;
        self.mem.wr16(a, off);
        self.mem.wr16(a + 2, seg);
    }

    pub fn now_ms(&self) -> u64 {
        self.cpu.steps / IPS_PER_MS
    }

    pub fn warn_once(&mut self, msg: String) {
        if self.warned.insert(msg.clone()) {
            eprintln!("[warn] {msg}");
        }
    }

    // ---------- register helpers for services ----------
    pub fn ax(&self) -> u16 { self.cpu.r[EAX] as u16 }
    pub fn bx(&self) -> u16 { self.cpu.r[EBX] as u16 }
    pub fn cx(&self) -> u16 { self.cpu.r[ECX] as u16 }
    pub fn dx(&self) -> u16 { self.cpu.r[EDX] as u16 }
    pub fn si(&self) -> u16 { self.cpu.r[ESI] as u16 }
    pub fn di(&self) -> u16 { self.cpu.r[EDI] as u16 }
    pub fn ah(&self) -> u8 { (self.ax() >> 8) as u8 }
    pub fn al(&self) -> u8 { self.ax() as u8 }
    pub fn bl(&self) -> u8 { self.bx() as u8 }
    pub fn bh(&self) -> u8 { (self.bx() >> 8) as u8 }
    pub fn set_ax(&mut self, v: u16) { self.cpu.set16(EAX, v) }
    pub fn set_bx(&mut self, v: u16) { self.cpu.set16(EBX, v) }
    pub fn set_cx(&mut self, v: u16) { self.cpu.set16(ECX, v) }
    pub fn set_dx(&mut self, v: u16) { self.cpu.set16(EDX, v) }
    pub fn set_si(&mut self, v: u16) { self.cpu.set16(ESI, v) }
    pub fn set_di(&mut self, v: u16) { self.cpu.set16(EDI, v) }
    pub fn set_al(&mut self, v: u8) { self.cpu.set8(0, v) }
    pub fn set_ah(&mut self, v: u8) { self.cpu.set8(4, v) }
    pub fn seg(&self, i: usize) -> u16 { self.cpu.s[i] }
    pub fn set_seg(&mut self, i: usize, v: u16) {
        let Machine { cpu, mem, .. } = self;
        cpu.set_seg(mem, i, v);
    }
    pub fn set_cf(&mut self, on: bool) {
        if on { self.cpu.fl |= CF } else { self.cpu.fl &= !CF }
    }
    pub fn set_zf(&mut self, on: bool) {
        if on { self.cpu.fl |= ZF } else { self.cpu.fl &= !ZF }
    }
    /// Linear address of seg:off.
    pub fn at(&self, seg: usize, off: u16) -> u32 {
        lin(self.cpu.s[seg], off)
    }

    // ---------- interrupts ----------
    /// Enter interrupt `n` the way the CPU does: push FLAGS, CS, IP; clear IF/TF.
    pub fn do_int(&mut self, n: u8) {
        let Machine { cpu, mem, .. } = self;
        let fl = (cpu.fl & 0xffff) as u16 | 2;
        cpu.push16(mem, fl);
        let cs = cpu.s[CS];
        cpu.push16(mem, cs);
        let ip = cpu.ip as u16;
        cpu.push16(mem, ip);
        cpu.fl &= !(IF | 0x100);
        let a = n as u32 * 4;
        let (off, seg) = (mem.rd16(a), mem.rd16(a + 2));
        cpu.set_seg(mem, CS, seg);
        cpu.ip = off as u32;
    }

    /// Advance the clock to the next timer interrupt (HLT, or waiting for a key).
    fn idle(&mut self) {
        let next = self.mem.pit.next_irq0;
        if next > self.cpu.steps {
            self.cpu.steps = next;
        }
    }

    pub fn run(&mut self) -> Stop {
        loop {
            if let Some(s) = self.stop.take() {
                return s;
            }
            let steps = self.cpu.steps;
            if self.cfg.max_steps.map_or(false, |b| steps >= b) {
                return Stop::Budget;
            }
            if steps >= self.next_service {
                // A fixed 2000-step grid (kept across run() calls and snapshots), so
                // input lands on the same steps when a journal is replayed.
                self.next_service = steps - steps % 2000 + 2000;
                if let Err(e) = self.service() {
                    return Stop::Error(e);
                }
                if self.stop.is_some() {
                    continue;
                }
            }
            self.mem.now = steps;
            if steps >= self.mem.pit.next_irq0 {
                let p = self.mem.pit.period(0);
                self.mem.pit.next_irq0 += p;
                if self.mem.pit.next_irq0 <= steps {
                    // Far behind (a long idle skip): don't queue a burst of ticks.
                    self.mem.pit.next_irq0 = steps + p;
                }
                self.mem.pic.raise(0);
            }
            if self.cpu.fl & IF != 0 && !self.cpu.int_shadow {
                if let Some(irq) = self.mem.pic.pending() {
                    self.mem.pic.ack(irq);
                    let v = self.mem.pic.base + irq;
                    self.do_int(v);
                }
            }
            if self.mouse.pending != 0 && self.cpu.fl & IF != 0 && !self.cpu.int_shadow && self.mouse.saved.is_none() {
                self.mouse_event();
            }
            self.cpu.int_shadow = false;
            self.history[(steps & 63) as usize] = (self.cpu.s[CS], self.cpu.ip as u16);
            if let Some((at, n)) = self.dbg_dump {
                if steps == at {
                    let mut f = std::fs::File::create("dump.bin").unwrap();
                    f.write_all(&self.mem.ram[..0x110000]).unwrap();
                    for i in 0..4 { f.write_all(&self.mem.vga.planes[i]).unwrap(); }
                    self.dbg_trace = Some(std::io::BufWriter::new(std::fs::File::create("trace.txt").unwrap()));
                }
                if steps >= at && steps < at + n {
                    let c = &self.cpu;
                    let line = format!("{:04x}:{:04x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:04x} {:04x} {:04x} {:04x} {:04x}
",
                        c.s[CS], c.ip, c.r[0], c.r[1], c.r[2], c.r[3], c.r[4], c.r[5], c.r[6], c.r[7], c.s[DS], c.s[ES], c.s[SS], c.fl & 0x8d5, 0);
                    if let Some(t) = &mut self.dbg_trace { t.write_all(line.as_bytes()).unwrap(); }
                }
            }
            if !self.dbg_break.is_empty() && self.dbg_break.contains(&(self.cpu.s[CS], self.cpu.ip as u16)) {
                let c = &self.cpu;
                let bp = c.r[5] as u16;
                let ss = c.s[SS];
                let loc = |o: i32| self.mem.rd16(lin(ss, bp.wrapping_add(o as u16)));
                eprintln!("[break] {:04x}:{:04x} ms={} ax={:04x} bx={:04x} cx={:04x} dx={:04x} si={:04x} di={:04x} bp={bp:04x} [bp-2]={:04x} [bp+6]={:04x}",
                    c.s[CS], c.ip, self.now_ms(), c.r[0] as u16, c.r[3] as u16, c.r[1] as u16, c.r[2] as u16, c.r[6] as u16, c.r[7] as u16, loc(-2), loc(6));
            }
            let watched: Vec<u8> = self.dbg_watch.iter().map(|&a| self.mem.ram[a as usize]).collect();
            if !self.mem.rwatch.is_empty() {
                self.mem.pc = (self.cpu.s[CS], self.cpu.ip);
            }
            let r = {
                let Machine { cpu, mem, .. } = self;
                cpu.step(mem)
            };
            for (k, &a) in self.dbg_watch.iter().enumerate() {
                let v = self.mem.ram[a as usize];
                if v != watched[k] {
                    let c = &self.cpu;
                    eprintln!("[watch] {a:05x}: {:02x} -> {v:02x} by {:04x}:{:04x} step {} ax={:04x} bx={:04x} cx={:04x} dx={:04x} si={:04x} di={:04x} bp={:04x} ds={:04x} es={:04x}",
                        watched[k], c.s[CS], c.op_ip, c.steps, c.r[0] as u16, c.r[3] as u16, c.r[1] as u16, c.r[2] as u16, c.r[6] as u16, c.r[7] as u16, c.r[5] as u16, c.s[DS], c.s[ES]);
                }
            }
            match r {
                Ok(()) => {}
                Err(Exit::Int(n)) => self.do_int(n),
                Err(Exit::Callback(n)) => {
                    if let Err(e) = self.callback(n) {
                        return Stop::Error(e);
                    }
                }
                Err(Exit::Halt) => self.idle(),
                Err(Exit::DivideError) => {
                    self.cpu.ip = self.cpu.op_ip;
                    self.do_int(0);
                }
                Err(Exit::Unimplemented(what)) => {
                    let s = self.cpu.steps;
                    let trail: Vec<String> = (1..=64u64).rev().map(|k| self.history[((s - k) & 63) as usize]).map(|(c, i)| format!("{c:04x}:{i:04x}")).collect();
                    eprintln!("[trail] {}", trail.join(" "));
                    return Stop::Error(format!("{what} at {:04x}:{:04x} ({})", self.cpu.s[CS], self.cpu.op_ip, self.dos.current_name()));
                }
            }
        }
    }

    /// Host-side work between instruction batches: input, replay, script, frames.
    fn service(&mut self) -> R<()> {
        if self.blog.as_ref().map_or(false, |b| self.now_ms() >= b.next_ms) {
            self.battle_log_tick();
        }
        self.run_replay();
        self.run_script()?;
        self.mem.kbd_pump();
        self.service_control();
        if self.host.is_some() {
            if !self.trial {
                // Keep emulated time in step with (speed x) real time (never run ahead).
                let emu_ms = self.now_ms().saturating_sub(self.emu_start_ms);
                let real_ms = self.real_start.elapsed().as_millis() as u64 * self.cfg.speed.max(1);
                if emu_ms > real_ms + 2 {
                    std::thread::sleep(std::time::Duration::from_millis(((emu_ms - real_ms) / self.cfg.speed.max(1)).clamp(1, 20)));
                } else if real_ms > emu_ms + 250 {
                    // Fell behind (slow host): drop the debt instead of racing to catch up.
                    self.emu_start_ms = self.now_ms();
                    self.real_start = std::time::Instant::now();
                }
            }
            let interval = if self.trial { 40 } else { 16 };
            if self.last_present.elapsed().as_millis() >= interval {
                self.last_present = std::time::Instant::now();
                self.present()?;
            }
        }
        Ok(())
    }

    fn present(&mut self) -> R<()> {
        let mut frame = self.frame();
        if self.cfg.debug_grid {
            crate::battle::draw_grid(&self.mem, frame.0, frame.1, &mut frame.2);
        }
        let title = if self.frames % 5 == 0 {
            crate::battle::units(&self.mem).and_then(|units| {
                let ours: Vec<_> = units.into_iter().filter(|u| u.side == "我軍" && u.x != 0xff && u.troops > 0).collect();
                if ours.is_empty() {
                    None
                } else {
                    let tot: i32 = ours.iter().map(|u| u.level as i32 * 100 + u.exp as i32).sum();
                    let s: Vec<_> = ours.iter().map(|u| format!("{} LV{}({:02}) 兵{}", u.name, u.level, u.exp, u.troops)).collect();
                    let mode = if self.trial { "AI 推演計算中" } else { "AI 正式行動" };
                    Some(format!("三國志英傑傳 [{mode} | 總經驗: {tot}] — {}", s.join(" | ")))
                }
            })
        } else {
            None
        };
        let mut host = self.host.take().unwrap();
        if let Some(t) = &title {
            host.set_title(t);
        }
        let open = host.present(&frame.2, frame.0, frame.1);
        let events = host.events();
        self.host = Some(host);
        self.frames += 1;
        if !open {
            self.journal("close");
            self.stop = Some(Stop::Closed);
            return Ok(());
        }
        // While replaying a journal the recorded input is the only input; a window
        // under remote control takes input only from the control port.
        if self.cfg.replay.is_none() && self.cfg.control.is_none() && !self.bot_mode {
            for e in events {
                self.journal(&crate::journal::encode(&e));
                self.apply_event(e);
            }
        }
        Ok(())
    }

    // ---------- save states used by the game's save screens ----------
    /// Save the whole machine. Written to a temporary file first, so a failed
    /// write never destroys the previous contents.
    pub fn save_state_atomic(&mut self, p: &std::path::Path) -> R<()> {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        let tmp = p.with_extension("tmp");
        self.save_state(&tmp)?;
        std::fs::rename(&tmp, p).map_err(|e| format!("{}: {e}", p.display()))
    }

    /// Replace the running machine with a saved one while the session goes on.
    /// Pending script events are kept (they continue on the loaded clock). On
    /// failure the machine is unchanged.
    pub fn load_state_live(&mut self, p: &std::path::Path) -> R<()> {
        let before = self.snapshot();
        let script = std::mem::take(&mut self.cfg.script);
        if let Err(e) = self.load_state(p) {
            self.restore(&before);
            self.cfg.script = script;
            return Err(e);
        }
        self.cfg.script = script;
        self.mem.opl.resync();
        self.rotate_journal(p);
        Ok(())
    }

    /// A journal only replays from the state it was recorded from: after a load,
    /// continue in a new journal file next to a copy of that state.
    fn rotate_journal(&mut self, state: &std::path::Path) {
        let Some(base) = self.cfg.journal.clone() else { return };
        if self.journal_file.is_none() {
            return;
        }
        self.loads += 1;
        let stem = base.file_stem().map_or("session".into(), |s| s.to_string_lossy().to_string());
        let j = base.with_file_name(format!("{stem}-load{}.txt", self.loads));
        let st = j.with_extension("state");
        let made = std::fs::copy(state, &st).map_err(|e| e.to_string()).and_then(|_| crate::journal::create(&j));
        match made {
            Ok(mut f) => {
                writeln!(f, "# continues {} after loading a save state; replay with: --state {} --replay {}", base.display(), st.display(), j.display()).ok();
                eprintln!("[journal] {}", j.display());
                self.journal_file = Some(f);
            }
            Err(e) => eprintln!("[journal] cannot start {}: {e}", j.display()),
        }
    }

    pub fn set_speed(&mut self, n: u64) {
        self.cfg.speed = n;
        self.emu_start_ms = self.now_ms();
        self.real_start = std::time::Instant::now();
    }

    /// The visible picture with the mouse cursor drawn on top (as the driver would).
    pub fn frame(&self) -> (usize, usize, Vec<u32>) {
        let (w, h, mut px) = self.mem.vga.render();
        if self.mouse.shown >= 0 {
            if let Some((and, xor)) = &self.mouse.masks {
                let sx = if w <= 320 { 2 } else { 1 };
                let cx = self.mouse.x / sx - self.mouse.hot.0 as i32;
                let cy = self.mouse.y - self.mouse.hot.1 as i32;
                for r in 0..16 {
                    for c in 0..16 {
                        let (x, y) = (cx + c, cy + r);
                        if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
                            continue;
                        }
                        let bit = 0x8000 >> c;
                        let p = &mut px[y as usize * w + x as usize];
                        if and[r as usize] & bit == 0 {
                            *p = 0;
                        }
                        if xor[r as usize] & bit != 0 {
                            *p ^= 0xffffff;
                        }
                    }
                }
            }
        }
        (w, h, px)
    }

    pub fn screen_hash(&self) -> u64 {
        // FNV-1a over the rendered pixels.
        let (w, h, px) = self.frame();
        let mut x = 0xcbf2_9ce4_8422_2325u64 ^ (w as u64) << 32 ^ h as u64;
        for p in px {
            x ^= p as u64;
            x = x.wrapping_mul(0x100_0000_01b3);
        }
        x
    }

    pub fn screenshot_png(&self) -> Vec<u8> {
        let (w, h, px) = self.frame();
        let rgb: Vec<u8> = px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
        crate::png::encode_rgb(w, h, &rgb)
    }

    // ---------- input ----------
    pub fn apply_event(&mut self, e: Event) {
        match e {
            Event::Close => self.stop = Some(Stop::Closed),
            Event::Key { sc, down } => {
                if sc & 0x100 != 0 {
                    self.mem.kbd_queue.push_back(0xe0);
                }
                self.mem.kbd_queue.push_back((sc as u8 & 0x7f) | if down { 0 } else { 0x80 });
            }
            Event::Mouse { x, y, buttons } => self.mouse_input(x, y, buttons),
        }
    }

    /// Position in driver coordinates (x 0-639 even in 320-wide modes, y = screen row).
    fn mouse_input(&mut self, x: i32, y: i32, buttons: u16) {
        let m = &mut self.mouse;
        let (nx, ny) = (x.clamp(m.xr.0, m.xr.1), y.clamp(m.yr.0, m.yr.1));
        // Event bits as the driver reports them: 1 move, then press/release pairs
        // for left (2/4), right (8/16), middle (32/64).
        let mut ev = if (nx, ny) != (m.x, m.y) { 1 } else { 0 };
        // Default mickey ratio: 8 per 8 pixels across, 16 per 8 pixels down.
        m.mick.0 += nx - m.x;
        m.mick.1 += (ny - m.y) * 2;
        m.x = nx;
        m.y = ny;
        for b in 0..3 {
            let bit = 1 << b;
            if buttons & bit != 0 && m.buttons & bit == 0 {
                m.press[b] = (m.press[b].0.wrapping_add(1), nx, ny);
                ev |= 2 << (b * 2);
            }
            if buttons & bit == 0 && m.buttons & bit != 0 {
                m.release[b] = (m.release[b].0.wrapping_add(1), nx, ny);
                ev |= 4 << (b * 2);
            }
        }
        m.buttons = buttons;
        if let Some((mask, _, _)) = m.handler {
            m.pending |= ev & mask;
        }
    }

    /// Call the program's INT 33h event handler as the driver does from IRQ12:
    /// AX = events, BX = buttons, CX/DX = position, SI/DI = mickeys. It returns with
    /// RETF into STUB_MOUSE, which restores the interrupted registers and IRETs.
    fn mouse_event(&mut self) {
        let Some((_, seg, off)) = self.mouse.handler else { return };
        let ev = std::mem::take(&mut self.mouse.pending);
        self.mouse.saved = Some(Box::new(self.cpu.clone()));
        let Machine { cpu, mem, mouse, .. } = self;
        let fl = (cpu.fl & 0xffff) as u16 | 2;
        cpu.push16(mem, fl);
        let (cs, ip) = (cpu.s[CS], cpu.ip as u16);
        cpu.push16(mem, cs);
        cpu.push16(mem, ip);
        cpu.push16(mem, BIOS_SEG);
        cpu.push16(mem, STUB_MOUSE);
        cpu.fl &= !(IF | 0x100);
        cpu.set16(EAX, ev);
        cpu.set16(EBX, mouse.buttons);
        cpu.set16(ECX, mouse.x as u16);
        cpu.set16(EDX, mouse.y as u16);
        cpu.set16(ESI, mouse.mick.0 as u16);
        cpu.set16(EDI, mouse.mick.1 as u16);
        cpu.set_seg(mem, CS, seg);
        cpu.ip = off as u32;
    }

    /// Back from the event handler: everything but CS:IP/SP/FLAGS (popped by the
    /// stub's IRET) comes from before the call.
    fn mouse_event_done(&mut self) {
        let Some(saved) = self.mouse.saved.take() else { return };
        let (sp, ip, cs, steps) = (self.cpu.r[ESP], self.cpu.ip, self.cpu.s[CS], self.cpu.steps);
        self.cpu = *saved;
        self.cpu.r[ESP] = sp;
        self.cpu.steps = steps;
        let Machine { cpu, mem, .. } = self;
        cpu.set_seg(mem, CS, cs);
        cpu.ip = ip;
    }

    fn blog_write(&mut self, line: &str) {
        let t = self.now_ms() as f64 / 1000.0;
        if let Some(b) = &mut self.blog {
            writeln!(b.file, "[{t:8.1}s] {line}").ok();
            b.file.flush().ok();
        }
    }

    /// Every 100 emulated ms: log what changed on the battlefield; at the start
    /// of a turn (all of our units ready again after some had acted) dump every
    /// unit, a save state and a grid screenshot.
    fn battle_log_tick(&mut self) {
        let now = self.now_ms();
        let Some(b) = &mut self.blog else { return };
        b.next_ms = now + 100;
        let Some(units) = crate::battle::units(&self.mem) else { return };
        let ours: Vec<&crate::battle::Unit> = units.iter().filter(|u| u.side == "我軍" && u.x != 0xff && u.troops > 0).collect();
        let b = self.blog.as_mut().unwrap();
        let had_acted = b.last_acted.iter().any(|&a| a);
        let new_turn = !ours.is_empty() && (b.turn == 0 || (had_acted && ours.iter().all(|u| !u.acted)));
        if !ours.is_empty() {
            b.last_acted = ours.iter().map(|u| u.acted).collect();
        }
        let mut lines = Vec::new();
        if new_turn {
            b.turn += 1;
            lines.push(format!("===== 第 {} 回合開始 =====", b.turn));
            for u in units.iter().filter(|u| u.x != 0xff && (u.troops > 0 || u.side == "我軍")) {
                lines.push(format!("  {}{} ({},{}) 兵{} LV{} EXP{} 士氣{}", u.side, u.name, u.x, u.y, u.troops, u.level, u.exp, u.morale));
            }
        }
        let turn = b.turn;
        if b.last.len() < units.len() {
            b.last.resize(units.len(), String::new());
        }
        for (i, u) in units.iter().enumerate() {
            let pos = if u.x == 0xff { "離場".to_string() } else { format!("({},{})", u.x, u.y) };
            let now_s = format!("{pos} 兵{} LV{} EXP{}{}", u.troops, u.level, u.exp, if u.acted { " 已行動" } else { "" });
            if b.last[i] != now_s {
                if !b.last[i].is_empty() && turn > 0 {
                    lines.push(format!("T{turn} {}{}: {} → {}", u.side, u.name, b.last[i], now_s));
                }
                b.last[i] = now_s;
            }
        }
        let dir = b.dir.clone();
        for l in lines {
            self.blog_write(&l);
        }
        if new_turn {
            let _ = self.save_state(&dir.join(format!("turn{turn:02}.bin")));
            let (w, h, mut px) = self.frame();
            crate::battle::draw_grid(&self.mem, w, h, &mut px);
            let rgb: Vec<u8> = px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
            let _ = std::fs::write(dir.join(format!("turn{turn:02}.png")), crate::png::encode_rgb(w, h, &rgb));
        }
    }

    /// Log a mouse press as the map tile under it (or the screen point elsewhere).
    fn battle_log_click(&mut self, entry: &str) {
        let Some(b) = &mut self.blog else { return };
        let t: Vec<&str> = entry.split_whitespace().collect();
        if t.first() != Some(&"mouse") || t.len() < 4 {
            return;
        }
        let (Ok(x), Ok(y), Ok(btn)) = (t[1].parse::<i32>(), t[2].parse::<i32>(), t[3].parse::<u16>()) else { return };
        let pressed = btn & !b.buttons;
        b.buttons = btn;
        if pressed == 0 || b.turn == 0 {
            return;
        }
        let which = if pressed & 1 != 0 { "左鍵" } else { "右鍵" };
        let (l, tp, r, bt) = crate::battle::VIEW_PX;
        let place = match crate::battle::view(&self.mem) {
            Some((sx, sy)) if x > l && x < r && y > tp && y < bt => {
                // Tile centers sit at 32 + 16·(2x − sx): invert to the nearest tile.
                let tx = ((x - 32) as f32 / 16.0 + sx as f32) / 2.0;
                let ty = ((y - 88) as f32 / 16.0 + sy as f32) / 2.0;
                format!("格 ({},{})", tx.round() as i32, ty.round() as i32)
            }
            _ => format!("畫面 ({x},{y})"),
        };
        let turn = b.turn;
        self.blog_write(&format!("T{turn} {which} {place}"));
    }

    pub fn journal(&mut self, entry: &str) {
        let step = self.cpu.steps;
        self.battle_log_click(entry);
        self.journal_mem.push(format!("{step} {entry}"));
        if let Some(f) = &mut self.journal_file {
            writeln!(f, "{step} {entry}").ok();
            f.flush().ok();
        }
    }

    fn run_replay(&mut self) {
        while self.replay.front().map_or(false, |(s, _)| *s <= self.cpu.steps) {
            let (step, e) = self.replay.pop_front().unwrap();
            if step + 1_000_000 < self.cpu.steps && !self.replay_drift_warned {
                eprintln!("[replay] warning: drifted from the recording (event for step {step} applied at {}); the journal is probably from another build", self.cpu.steps);
                self.replay_drift_warned = true;
            }
            self.apply_event(e);
        }
    }

    fn run_script(&mut self) -> R<()> {
        let now = self.now_ms();
        while self.cfg.script.first().map_or(false, |e| e.ms <= now) {
            let ev = self.cfg.script.remove(0);
            match ev.action {
                ScriptAction::Mouse(..) | ScriptAction::Key(..) => {
                    let e = match ev.action {
                        ScriptAction::Mouse(x, y, b) => Event::Mouse { x, y, buttons: b },
                        ScriptAction::Key(sc, down) => Event::Key { sc, down },
                        _ => unreachable!(),
                    };
                    // Scripted input is journaled like real input, so the session replays.
                    self.journal(&crate::journal::encode(&e));
                    self.apply_event(e);
                }
                ScriptAction::Shot(p) => {
                    if let Some(d) = p.parent() {
                        std::fs::create_dir_all(d).ok();
                    }
                    std::fs::write(&p, self.screenshot_png()).map_err(|e| format!("{}: {e}", p.display()))?;
                }
                ScriptAction::Expect(name, want) => {
                    let got = self.screen_hash();
                    self.checks.push((name, want, got));
                }
                ScriptAction::ExpectFile(name, got, want) => {
                    let a = std::fs::read(self.cfg.game_dir.join(&got)).unwrap_or_default();
                    let b = std::fs::read(&want).map_err(|e| format!("{}: {e}", want.display()))?;
                    self.checks.push((name, fnv(&b), fnv(&a)));
                }
                ScriptAction::Quit => self.stop = Some(Stop::Exit(0)),
            }
        }
        Ok(())
    }

    // ---------- host services ----------
    fn callback(&mut self, n: u8) -> R<()> {
        if self.cpu.s[CS] != BIOS_SEG || crate::saveui::owns_bios_callback(self.cpu.op_ip as u16) {
            // Hooks patched into the game's own code (see saveui.rs).
            return crate::saveui::hook(self, n);
        }
        if self.cpu.s[CS] == BIOS_SEG && self.cpu.op_ip == STUB_MOUSE as u32 {
            self.mouse_event_done();
            return Ok(());
        }
        let flags_at = self.at(SS, (self.cpu.r[ESP] as u16).wrapping_add(4));
        let caller_if = self.mem.rd16(flags_at) & IF as u16;
        let ip_before = self.cpu.ip;
        if self.cfg.trace && !matches!(n, 0x08 | 0x09 | 0x16 | 0x1a) {
            eprintln!("[int {n:02x}] ax={:04x} bx={:04x} cx={:04x} dx={:04x} ds={:04x} es={:04x}  ({})", self.ax(), self.bx(), self.cx(), self.dx(), self.seg(DS), self.seg(ES), self.dos.current_name());
        }
        match n {
            0x08 => self.timer_tick(),
            0x09 => self.kbd_irq(),
            0x10 => self.int10(),
            0x11 => self.set_ax(0x0021),
            0x12 => self.set_ax(640),
            0x15 => self.int15(),
            0x16 => self.int16(),
            0x1a => self.int1a(),
            0x20 => {
                crate::dos::terminate(self, 0, None)?;
                return Ok(());
            }
            0x21 => {
                if crate::dos::int21(self)? {
                    return Ok(()); // control was transferred (EXEC / terminate)
                }
            }
            0x27 => {
                let paras = (self.dx() as u32 + 15) >> 4;
                crate::dos::terminate(self, 0, Some(paras as u16))?;
                return Ok(());
            }
            0x2f => self.int2f(),
            0x33 => self.int33(),
            0x13 | 0x14 | 0x17 => {
                self.set_ah(0x80);
                self.set_cf(true);
            }
            0x00 => return Err(format!("divide error in {}", self.dos.current_name())),
            _ => self.warn_once(format!("unhandled interrupt {n:02x} (ax={:04x}) from {}", self.ax(), self.dos.current_name())),
        }
        // A handler returns with the caller's IF (RETF 2 keeps our flags otherwise),
        // unless it is waiting for input and will run again with interrupts on.
        if self.cpu.ip == ip_before {
            self.cpu.fl = (self.cpu.fl & !IF) | caller_if as u32;
        }
        Ok(())
    }

    fn timer_tick(&mut self) {
        let a = 0x46c;
        let t = (self.mem.rd16(a) as u32 | (self.mem.rd16(a + 2) as u32) << 16) + 1;
        let t = if t >= 0x1800b0 {
            self.mem.ram[0x470] = 1;
            0
        } else {
            t
        };
        self.mem.wr16(a, t as u16);
        self.mem.wr16(a + 2, (t >> 16) as u16);
    }

    fn kbd_irq(&mut self) {
        let sc = self.mem.inp(0x60, 1) as u8;
        let flags = self.mem.ram[0x417];
        let (bit, down) = match sc & 0x7f {
            0x2a | 0x36 => (if sc & 0x7f == 0x2a { 0x02 } else { 0x01 }, sc & 0x80 == 0),
            0x1d => (0x04, sc & 0x80 == 0),
            0x38 => (0x08, sc & 0x80 == 0),
            _ => (0, false),
        };
        if bit != 0 {
            self.mem.ram[0x417] = if down { flags | bit } else { flags & !bit };
            return;
        }
        if sc == 0xe0 || sc & 0x80 != 0 {
            return;
        }
        let shift = flags & 3 != 0;
        let ascii = crate::keys::ascii(sc, shift, flags & 4 != 0);
        let code = (sc as u16) << 8 | ascii as u16;
        let (head, tail) = (self.mem.rd16(0x41a), self.mem.rd16(0x41c));
        let next = if tail + 2 >= 0x3e { 0x1e } else { tail + 2 };
        if next != head {
            self.mem.wr16(0x400 + tail as u32, code);
            self.mem.wr16(0x41c, next);
        }
    }

    fn int16(&mut self) {
        let (head, tail) = (self.mem.rd16(0x41a), self.mem.rd16(0x41c));
        match self.ah() & 0xef {
            0x00 => {
                if head == tail {
                    // Block: re-run this callback with interrupts on until a key arrives.
                    self.cpu.ip = self.cpu.ip.wrapping_sub(3) & 0xffff;
                    self.cpu.fl |= IF;
                    self.idle_for_key();
                    return;
                }
                let k = self.mem.rd16(0x400 + head as u32);
                self.mem.wr16(0x41a, if head + 2 >= 0x3e { 0x1e } else { head + 2 });
                self.set_ax(k);
            }
            0x01 => {
                if head == tail {
                    self.set_zf(true);
                } else {
                    let k = self.mem.rd16(0x400 + head as u32);
                    self.set_ax(k);
                    self.set_zf(false);
                }
            }
            0x02 => self.set_al(self.mem.ram[0x417]),
            0x05 => {
                let next = if tail + 2 >= 0x3e { 0x1e } else { tail + 2 };
                if next == head {
                    self.set_al(1);
                } else {
                    self.mem.wr16(0x400 + tail as u32, self.cx());
                    self.mem.wr16(0x41c, next);
                    self.set_al(0);
                }
            }
            _ => {}
        }
    }

    /// Waiting for a key inside INT 16h: let the clock run to the next tick.
    fn idle_for_key(&mut self) {
        if self.mem.kbd_queue.is_empty() {
            self.idle();
        }
    }

    fn int15(&mut self) {
        match self.ah() {
            0x86 => {
                let us = (self.cx() as u64) << 16 | self.dx() as u64;
                self.cpu.steps += us * IPS_PER_MS / 1000;
                self.set_cf(false);
            }
            0x88 => {
                self.set_ax(0);
                self.set_cf(false);
            }
            _ => {
                self.set_ah(0x86);
                self.set_cf(true);
            }
        }
    }

    fn int1a(&mut self) {
        // Fixed epoch (1993-08-01 09:00:00) + emulated time: runs replay identically.
        let secs = 9 * 3600 + self.now_ms() / 1000;
        let bcd = |v: u64| (((v / 10) << 4) | (v % 10)) as u16;
        match self.ah() {
            0x00 => {
                self.set_cx(self.mem.rd16(0x46e));
                self.set_dx(self.mem.rd16(0x46c));
                let f = std::mem::take(&mut self.mem.ram[0x470]);
                self.set_al(f);
            }
            0x02 => {
                self.set_cx(bcd(secs / 3600 % 24) << 8 | bcd(secs / 60 % 60));
                self.set_dx(bcd(secs % 60) << 8);
                self.set_cf(false);
            }
            0x04 => {
                self.set_cx(0x1993);
                self.set_dx(0x0801);
                self.set_cf(false);
            }
            _ => {}
        }
    }

    fn int2f(&mut self) {
        // Nothing is installed in the multiplex chain (no Windows, XMS, MSCDEX…).
        if self.ax() == 0x1680 {
            self.set_al(0x80);
        }
    }

    fn int10(&mut self) {
        match self.ah() {
            0x00 => {
                let mode = self.al();
                if self.mem.vga.set_mode(mode, true) {
                    self.mem.ram[0x449] = mode & 0x7f;
                    let cols = if matches!(mode & 0x7f, 0x0d | 0x13) { 40 } else { 80 };
                    self.mem.wr16(0x44a, cols);
                    self.mouse.yr = (0, if mode & 0x7f == 0x12 { 479 } else if mode & 0x7f == 0x10 { 349 } else { 199 });
                } else {
                    self.warn_once(format!("INT 10h: unsupported video mode {mode:02x}"));
                }
            }
            0x0f => {
                let mode = self.mem.ram[0x449];
                let cols = self.mem.rd16(0x44a);
                self.set_ax((cols << 8) | mode as u16);
                self.set_bx(self.bx() & 0x00ff);
            }
            0x02 | 0x01 | 0x05 => {}
            0x03 => {
                self.set_dx(0);
                self.set_cx(0x0607);
            }
            0x0e | 0x09 | 0x0a => {
                let c = self.al();
                self.tty.push(c);
                if c == b'\n' || self.tty.len() > 200 {
                    let line = std::mem::take(&mut self.tty);
                    eprintln!("[tty] {}", String::from_utf8_lossy(&line).trim_end());
                }
            }
            0x13 => {
                let a = self.at(ES, self.cpu.r[5] as u16);
                let n = self.cx() as usize;
                let s = self.mem.bytes(a, n).to_vec();
                let s = if self.al() & 2 != 0 { s.iter().step_by(2).copied().collect() } else { s };
                eprintln!("[tty] {}", String::from_utf8_lossy(&s).trim_end());
            }
            0x10 => self.int10_palette(),
            0x11 => {
                if self.al() == 0x30 {
                    let (off, h) = match self.bh() {
                        3 => (FONT8_OFF, 8),
                        4 => (FONT8_OFF + 0x400, 8),
                        _ => (FONT16_OFF, 16),
                    };
                    self.set_seg(ES, BIOS_SEG);
                    self.cpu.set16(5, off);
                    self.set_cx(h);
                    self.set_dx(if self.mem.vga.text { 24 } else { 29 });
                }
            }
            0x12 => {
                if self.bl() == 0x10 {
                    self.set_bx(0x0003);
                    self.set_cx(0x0009);
                }
            }
            0x1a => {
                if self.al() == 0 {
                    self.set_al(0x1a);
                    self.set_bx(0x0008);
                }
            }
            0x1b => self.set_al(0),
            0x06 | 0x07 | 0x08 | 0x0b | 0x0c | 0x0d => {}
            ah => self.warn_once(format!("INT 10h AH={ah:02x} not implemented")),
        }
    }

    fn int10_palette(&mut self) {
        let v = &mut self.mem.vga;
        let bx = self.cpu.r[EBX] as u16;
        let (bl, bh) = (bx as u8, (bx >> 8) as u8);
        match self.cpu.r[EAX] as u8 {
            0x00 => {
                if bl < 16 {
                    v.attr[bl as usize] = bh;
                    v.dirty += 1;
                }
            }
            0x01 => v.attr[0x11] = bh,
            0x02 => {
                let a = lin(self.cpu.s[ES], self.cpu.r[EDX] as u16);
                for i in 0..16 {
                    v.attr[i] = self.mem.ram[a as usize + i];
                }
                v.attr[0x11] = self.mem.ram[a as usize + 16];
                v.dirty += 1;
            }
            0x03 => {}
            0x07 => {
                let r = v.attr[(bl & 0x1f) as usize];
                self.cpu.set8(7, r);
            }
            0x10 => {
                let (r, g, b) = ((self.cpu.r[EDX] >> 8) as u8, (self.cpu.r[ECX] >> 8) as u8, self.cpu.r[ECX] as u8);
                v.dac[bx as u8 as usize] = [r & 0x3f, g & 0x3f, b & 0x3f];
                v.dirty += 1;
            }
            0x12 => {
                let a = lin(self.cpu.s[ES], self.cpu.r[EDX] as u16) as usize;
                let n = self.cpu.r[ECX] as u16 as usize;
                for i in 0..n {
                    let e = (bx as usize + i) & 0xff;
                    for c in 0..3 {
                        v.dac[e][c] = self.mem.ram[a + i * 3 + c] & 0x3f;
                    }
                }
                v.dirty += 1;
            }
            0x15 => {
                let [r, g, b] = v.dac[bl as usize];
                self.cpu.set8(6, r);
                self.cpu.set8(5, g);
                self.cpu.set8(1, b);
            }
            0x17 => {
                let a = lin(self.cpu.s[ES], self.cpu.r[EDX] as u16) as usize;
                let n = self.cpu.r[ECX] as u16 as usize;
                for i in 0..n {
                    let e = (bx as usize + i) & 0xff;
                    for c in 0..3 {
                        self.mem.ram[a + i * 3 + c] = self.mem.vga.dac[e][c];
                    }
                }
            }
            al => self.warn_once(format!("INT 10h AX=10{al:02x} not implemented")),
        }
    }

    fn int33(&mut self) {
        let m = &mut self.mouse;
        match self.cpu.r[EAX] as u16 {
            0x00 | 0x21 => {
                m.shown = -1;
                m.handler = None;
                m.pending = 0;
                m.xr = (0, 639);
                self.set_ax(0xffff);
                self.set_bx(2);
            }
            0x01 => {
                if m.shown < 0 {
                    m.shown += 1;
                }
                self.mem.vga.dirty += 1;
            }
            0x02 => {
                m.shown -= 1;
                self.mem.vga.dirty += 1;
            }
            0x03 => {
                let (b, x, y) = (m.buttons, m.x, m.y);
                self.set_bx(b);
                self.set_cx(x as u16);
                self.set_dx(y as u16);
            }
            0x04 => {
                m.x = (self.cpu.r[ECX] as u16 as i16 as i32).clamp(m.xr.0, m.xr.1);
                m.y = (self.cpu.r[EDX] as u16 as i16 as i32).clamp(m.yr.0, m.yr.1);
            }
            0x05 | 0x06 => {
                let b = (self.cpu.r[EBX] as u16 as usize).min(2);
                let list = if self.cpu.r[EAX] as u16 == 5 { &mut m.press } else { &mut m.release };
                let (n, x, y) = std::mem::take(&mut list[b]);
                let buttons = m.buttons;
                self.set_ax(buttons);
                self.set_bx(n);
                self.set_cx(x as u16);
                self.set_dx(y as u16);
            }
            0x07 => {
                let (a, b) = (self.cpu.r[ECX] as u16 as i16 as i32, self.cpu.r[EDX] as u16 as i16 as i32);
                m.xr = (a.min(b), a.max(b));
                m.x = m.x.clamp(m.xr.0, m.xr.1);
            }
            0x08 => {
                let (a, b) = (self.cpu.r[ECX] as u16 as i16 as i32, self.cpu.r[EDX] as u16 as i16 as i32);
                m.yr = (a.min(b), a.max(b));
                m.y = m.y.clamp(m.yr.0, m.yr.1);
            }
            0x09 => {
                m.hot = (self.cpu.r[EBX] as u16 as i16, self.cpu.r[ECX] as u16 as i16);
                let a = lin(self.cpu.s[ES], self.cpu.r[EDX] as u16);
                let mut and = [0u16; 16];
                let mut xor = [0u16; 16];
                for i in 0..16 {
                    and[i] = self.mem.rd16(a + i as u32 * 2);
                    xor[i] = self.mem.rd16(a + 32 + i as u32 * 2);
                }
                self.mouse.masks = Some((and, xor));
                self.mem.vga.dirty += 1;
            }
            0x0b => {
                let (x, y) = std::mem::take(&mut m.mick);
                self.set_cx(x as u16);
                self.set_dx(y as u16);
            }
            0x0a | 0x0f | 0x10 | 0x13 | 0x1a | 0x1d => {}
            0x0c | 0x14 => {
                let new = (self.cpu.r[ECX] as u16, self.cpu.s[ES], self.cpu.r[EDX] as u16);
                let old = m.handler.unwrap_or((0, 0, 0));
                m.handler = (new.0 != 0).then_some(new);
                m.pending = 0;
                if self.cpu.r[EAX] as u16 == 0x14 {
                    self.set_cx(old.0);
                    self.set_seg(ES, old.1);
                    self.set_dx(old.2);
                }
            }
            0x24 => {
                self.set_bx(0x0626);
                self.set_cx(0x0400);
            }
            ax => self.warn_once(format!("INT 33h AX={ax:04x} not implemented")),
        }
    }
}

pub fn fnv(b: &[u8]) -> u64 {
    let mut x = 0xcbf2_9ce4_8422_2325u64;
    for &c in b {
        x ^= c as u64;
        x = x.wrapping_mul(0x100_0000_01b3);
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pic_masks_and_eoi() {
        let mut m = Mem::new();
        m.outp(0x21, 1, 0xfe); // only IRQ0 unmasked
        m.pic.raise(1);
        assert_eq!(m.pic.pending(), None);
        m.pic.raise(0);
        assert_eq!(m.pic.pending(), Some(0));
        m.pic.ack(0);
        m.pic.raise(0);
        assert_eq!(m.pic.pending(), None, "IRQ0 in service blocks itself");
        m.outp(0x20, 1, 0x20);
        assert_eq!(m.pic.pending(), Some(0));
    }

    #[test]
    fn pit_reprogramming_changes_irq_rate() {
        let mut m = Mem::new();
        assert_eq!(m.pit.period(0), 65536 * IPS_PER_MS * 1000 / 1_193_182);
        // 1193182 / 11932 ≈ 100 Hz → 10 ms.
        m.outp(0x43, 1, 0x36);
        m.outp(0x40, 1, (11932 & 0xff) as u32);
        m.outp(0x40, 1, (11932 >> 8) as u32);
        // Hand-computed: 11932 * 25_000_000 / 1_193_182 = 250_003 instructions (10 ms).
        assert_eq!(m.pit.period(0), 250_003);
    }
}

/// A full copy of the emulated PC, for trying moves and going back (the emulator
/// is deterministic, so restoring and replaying the same input gives the same result).
pub struct Snapshot {
    saveui_screen: Option<Vec<u8>>,
    cpu: Cpu,
    mem: Mem,
    dos: Dos,
    files: Vec<(usize, u64)>,
    mouse: Mouse,
    script: Vec<ScriptEvent>,
    journal_len: usize,
    journal_mem: Vec<String>,
    next_service: u64,
}

impl Machine {
    pub fn snapshot(&mut self) -> Snapshot {
        Snapshot {
            saveui_screen: self.saveui_screen.clone(),
            cpu: self.cpu.clone(),
            mem: self.mem.clone(),
            files: self.dos.file_positions(),
            dos: self.dos.clone(),
            mouse: self.mouse.clone(),
            script: self.cfg.script.clone(),
            journal_len: self.journal_mem.len(),
            journal_mem: self.journal_mem.clone(),
            next_service: self.next_service,
        }
    }

    pub fn restore(&mut self, s: &Snapshot) {
        self.saveui_screen = s.saveui_screen.clone();
        self.cpu = s.cpu.clone();
        self.mem = s.mem.clone();
        self.dos = s.dos.clone();
        self.dos.seek_files(&s.files);
        self.mouse = s.mouse.clone();
        self.cfg.script = s.script.clone();
        if s.journal_mem.len() >= self.journal_mem.len() {
            self.journal_mem = s.journal_mem.clone();
        } else {
            self.journal_mem.truncate(s.journal_len);
        }
        self.next_service = s.next_service;
        self.stop = None;
        if !self.trial {
            if let Some(a) = &self.audio {
                self.mem.opl.sink = Some(a.sink.clone());
            }
            self.emu_start_ms = self.now_ms();
            self.real_start = std::time::Instant::now();
        } else {
            self.mem.opl.sink = None;
        }
    }

    pub fn set_trial(&mut self, on: bool) {
        self.trial = on;
        if on {
            self.mem.opl.sink = None;
        } else {
            if let Some(a) = &self.audio {
                self.mem.opl.sink = Some(a.sink.clone());
            }
            self.emu_start_ms = self.now_ms();
            self.real_start = std::time::Instant::now();
        }
    }

    pub fn finish_turn(&mut self, start: &Snapshot, snap: &Snapshot, live: bool) {
        if live && self.host.is_some() {
            let lines = if snap.journal_mem.len() > start.journal_len {
                snap.journal_mem[start.journal_len..].to_vec()
            } else {
                Vec::new()
            };
            self.restore(start);
            self.set_trial(false);
            for l in &lines {
                if let Some((step, ev)) = crate::journal::parse_line(l) {
                    self.replay.push_back((step, ev));
                }
            }
            self.cfg.max_steps = Some(snap.cpu.steps);
            let _ = self.run();
            self.replay.clear();
        }
        self.restore(snap);
        self.set_trial(false);
    }

    pub fn clear_journal(&mut self) {
        self.journal_mem.clear();
    }

    /// Journal entries kept in memory (written out by whoever drives the machine).
    pub fn journal_entries(&self) -> &[String] {
        &self.journal_mem
    }
}

impl Mouse {
    fn save(&self, w: &mut crate::ser::W) {
        w.i32(self.x); w.i32(self.y); w.u16(self.buttons); w.i32(self.shown);
        w.i32(self.xr.0); w.i32(self.xr.1); w.i32(self.yr.0); w.i32(self.yr.1);
        for p in self.press.iter().chain(self.release.iter()) { w.u16(p.0); w.i32(p.1); w.i32(p.2) }
        w.u16(self.hot.0 as u16); w.u16(self.hot.1 as u16);
        match &self.masks {
            None => w.u8(0),
            Some((a, x)) => { w.u8(1); for v in a.iter().chain(x.iter()) { w.u16(*v) } }
        }
        w.i32(self.mick.0); w.i32(self.mick.1);
        match self.handler {
            None => w.u8(0),
            Some((m, s, o)) => { w.u8(1); w.u16(m); w.u16(s); w.u16(o) }
        }
        w.u16(self.pending);
    }
    fn load(r: &mut crate::ser::R) -> Result<Mouse, String> {
        let mut m = Mouse::default();
        m.x = r.i32()?; m.y = r.i32()?; m.buttons = r.u16()?; m.shown = r.i32()?;
        m.xr = (r.i32()?, r.i32()?); m.yr = (r.i32()?, r.i32()?);
        for i in 0..6 {
            let p = (r.u16()?, r.i32()?, r.i32()?);
            if i < 3 { m.press[i] = p } else { m.release[i - 3] = p }
        }
        m.hot = (r.u16()? as i16, r.u16()? as i16);
        if r.u8()? != 0 {
            let (mut a, mut x) = ([0u16; 16], [0u16; 16]);
            for i in 0..16 { a[i] = r.u16()? }
            for i in 0..16 { x[i] = r.u16()? }
            m.masks = Some((a, x));
        }
        m.mick = (r.i32()?, r.i32()?);
        if r.u8()? != 0 { m.handler = Some((r.u16()?, r.u16()?, r.u16()?)) }
        m.pending = r.u16()?;
        Ok(m)
    }
}

impl Machine {
    /// Everything needed to continue this session later: CPU, memory, devices,
    /// DOS (open files by name + position), mouse driver, journal so far.
    pub fn save_state(&mut self, path: &std::path::Path) -> R<()> {
        let mut w = crate::ser::W::new();
        w.str("eiketsu-state-3");
        self.cpu.save(&mut w);
        self.mem.save(&mut w);
        self.dos.save(&mut w);
        self.mouse.save(&mut w);
        w.u32(self.journal_mem.len() as u32);
        for l in &self.journal_mem { w.str(l) }
        w.u64(self.next_service);
        w.bytes(&self.mem.opl.regs);
        match &self.saveui_screen {
            Some(v) => { w.u8(1); w.bytes(v) }
            None => w.u8(0),
        }
        std::fs::write(path, &w.0).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn load_state(&mut self, path: &std::path::Path) -> R<()> {
        let b = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut r = crate::ser::R::new(&b);
        // Version 1 (the battle log's and bot's older states) lacks the OPL registers.
        let version = match r.str()?.as_str() {
            "eiketsu-state-1" => 1,
            "eiketsu-state-2" => 2,
            "eiketsu-state-3" => 3,
            _ => return Err("not a save state".into()),
        };
        self.cpu = Cpu::load(&mut r)?;
        self.mem.load(&mut r)?;
        self.dos.load(&mut r)?;
        self.mouse = Mouse::load(&mut r)?;
        let n = r.u32()? as usize;
        self.journal_mem = (0..n).map(|_| r.str()).collect::<Result<_, _>>()?;
        self.next_service = r.u64()?;
        self.mem.opl.regs = [0; 256];
        if version >= 2 {
            let regs = r.bytes()?;
            if regs.len() != 256 {
                return Err("save state: bad OPL register block".into());
            }
            self.mem.opl.regs.copy_from_slice(regs);
        }
        self.saveui_screen = None;
        if version >= 3 && r.u8()? != 0 {
            self.saveui_screen = Some(r.bytes()?.to_vec());
        }
        self.cfg.script.clear();
        self.stop = None;
        // Windowed play paces emulated time against real time from here on.
        self.emu_start_ms = self.now_ms();
        self.real_start = std::time::Instant::now();
        Ok(())
    }
}
