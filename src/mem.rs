//! The bus the CPU sees: 1 MiB (+HMA) of real-mode memory with the VGA window
//! carved out, and the I/O ports of the PC chips the programs touch directly
//! (8259 PIC, 8253 PIT, 8042 keyboard controller, CMOS, VGA, OPL2 / AdLib).
//!
//! Time on the bus is the instruction count (`now`), so every port that depends on
//! time (retrace, PIT counter, OPL timers) is deterministic.

use crate::opl::Opl;
use crate::vga::Vga;
use crate::IPS_PER_MS;
use std::collections::VecDeque;

pub const MEM_SIZE: usize = 0x11_0000;
const PIT_HZ: u64 = 1_193_182;

#[derive(Default, Clone)]
pub struct Pic {
    pub imr: u8,
    pub irr: u8,
    pub isr: u8,
    pub base: u8,
    icw_step: u8,
    read_isr: bool,
}

impl Pic {
    fn new(base: u8) -> Pic {
        Pic { base, ..Default::default() }
    }

    fn outp(&mut self, cmd: bool, v: u8) {
        if cmd {
            if v & 0x10 != 0 {
                // ICW1: start the init sequence.
                self.icw_step = 2;
                self.imr = 0;
                self.isr = 0;
                self.irr = 0;
            } else if v & 0x18 == 0x08 {
                // OCW3
                if v & 2 != 0 {
                    self.read_isr = v & 1 != 0;
                }
            } else if v & 0x20 != 0 {
                // OCW2 EOI: specific (bit 6) or the highest in service.
                if v & 0x40 != 0 {
                    self.isr &= !(1 << (v & 7));
                } else if self.isr != 0 {
                    self.isr &= self.isr - 1;
                }
            }
        } else {
            match self.icw_step {
                2 => { self.base = v & 0xf8; self.icw_step = 3 }
                3 => self.icw_step = 4,
                4 => self.icw_step = 0,
                _ => self.imr = v,
            }
        }
    }

    fn inp(&self, cmd: bool) -> u8 {
        if cmd {
            if self.read_isr { self.isr } else { self.irr }
        } else {
            self.imr
        }
    }

    pub fn raise(&mut self, irq: u8) {
        self.irr |= 1 << irq;
    }

    /// Highest-priority request that is unmasked and above everything in service.
    pub fn pending(&self) -> Option<u8> {
        let req = self.irr & !self.imr;
        for i in 0..8 {
            if self.isr & (1 << i) != 0 {
                return None;
            }
            if req & (1 << i) != 0 {
                return Some(i);
            }
        }
        None
    }

    pub fn ack(&mut self, irq: u8) {
        self.irr &= !(1 << irq);
        self.isr |= 1 << irq;
    }
}

#[derive(Clone, Copy, Default)]
struct PitChan {
    reload: u16,
    access: u8,
    hi_next: bool,
    latch: Option<u16>,
    latch_hi_next: bool,
    /// Instruction count when the current count started.
    start: u64,
}

#[derive(Clone)]
pub struct Pit {
    ch: [PitChan; 3],
    /// Next IRQ0 time (instruction count).
    pub next_irq0: u64,
}

impl Pit {
    fn new() -> Pit {
        let mut p = Pit { ch: [PitChan::default(); 3], next_irq0: 0 };
        for c in &mut p.ch {
            c.access = 3;
        }
        p.next_irq0 = p.period(0);
        p
    }

    fn divisor(&self, i: usize) -> u64 {
        if self.ch[i].reload == 0 { 65536 } else { self.ch[i].reload as u64 }
    }

    /// Channel period in instructions.
    pub fn period(&self, i: usize) -> u64 {
        (self.divisor(i) * IPS_PER_MS * 1000 / PIT_HZ).max(1)
    }

    fn count(&self, i: usize, now: u64) -> u16 {
        let el = now.saturating_sub(self.ch[i].start) * PIT_HZ / (IPS_PER_MS * 1000);
        let d = self.divisor(i);
        (d - el % d) as u16
    }

    fn outp(&mut self, port: u16, v: u8, now: u64) {
        if port == 0x43 {
            let i = (v >> 6) as usize;
            if i == 3 {
                return;
            }
            if v & 0x30 == 0 {
                self.ch[i].latch = Some(self.count(i, now));
                self.ch[i].latch_hi_next = false;
            } else {
                self.ch[i].access = (v >> 4) & 3;
                self.ch[i].hi_next = false;
            }
            return;
        }
        let i = (port - 0x40) as usize;
        let c = &mut self.ch[i];
        match c.access {
            1 => c.reload = (c.reload & 0xff00) | v as u16,
            2 => c.reload = (c.reload & 0x00ff) | (v as u16) << 8,
            _ => {
                if c.hi_next {
                    c.reload = (c.reload & 0x00ff) | (v as u16) << 8;
                } else {
                    c.reload = (c.reload & 0xff00) | v as u16;
                }
                c.hi_next = !c.hi_next;
                if c.hi_next {
                    return;
                }
            }
        }
        c.start = now;
        if i == 0 {
            self.next_irq0 = now + self.period(0);
        }
    }

    fn inp(&mut self, port: u16, now: u64) -> u8 {
        let i = (port - 0x40) as usize;
        let v = match self.ch[i].latch {
            Some(l) => l,
            None => self.count(i, now),
        };
        let c = &mut self.ch[i];
        let hi = c.latch_hi_next;
        c.latch_hi_next = !hi;
        if hi || c.access == 2 {
            if c.latch.is_some() && (hi || c.access != 3) {
                c.latch = None;
            }
            (v >> 8) as u8
        } else {
            if c.access == 1 {
                c.latch = None;
                c.latch_hi_next = false;
            }
            v as u8
        }
    }
}

/// The most RAM bytes one round of a polling loop may change (Mem::wlog).
pub const WLOG_MAX: usize = 256;

#[derive(Clone)]
pub struct Mem {
    pub ram: Vec<u8>,
    pub vga: Vga,
    pub pic: Pic,
    pub pic2: Pic,
    pub pit: Pit,
    pub opl: Opl,
    /// Scancodes waiting for the keyboard controller's output buffer.
    pub kbd_queue: VecDeque<u8>,
    kbd_data: u8,
    kbd_full: bool,
    port61: u8,
    cmos_i: u8,
    /// Current time (instruction count), kept up to date by the machine.
    pub now: u64,
    /// Ports nobody emulates, reported once each.
    unknown_ports: std::collections::BTreeSet<u16>,
    pub trace_ports: bool,
    /// Debug: linear addresses whose reads are logged with `pc` (EIKETSU_RWATCH).
    pub rwatch: Vec<u32>,
    pub pc: (u16, u32),
    pub rwatch_hits: Vec<(u32, u16, u32)>,
    /// Counts everything the CPU does that could change what it sees next, other
    /// than RAM writes while `wlog_on`: any video memory access, any port I/O.
    /// The machine adds its own (interrupts, host services). See Machine::spin_check.
    /// Not saved: only differences count.
    pub epoch: u64,
    /// While on: the RAM bytes changed since the polling point being watched,
    /// each once, with the value it had then. Full (WLOG_MAX): off, and counted
    /// in `epoch`. `wmark` has a bit set for each byte in the log.
    pub wlog: Vec<(u32, u8)>,
    pub wlog_on: bool,
    wmark: Vec<u64>,
    /// Reads of the word at `cwatch` are counted in `creads` (u32::MAX: none).
    pub cwatch: u32,
    pub creads: u32,
}

impl Mem {
    pub fn new() -> Mem {
        Mem {
            ram: vec![0; MEM_SIZE + 0x10000],
            vga: Vga::new(),
            pic: Pic::new(0x08),
            pic2: Pic::new(0x70),
            pit: Pit::new(),
            opl: Opl::new(),
            kbd_queue: VecDeque::new(),
            kbd_data: 0,
            kbd_full: false,
            port61: 0,
            cmos_i: 0,
            now: 0,
            unknown_ports: Default::default(),
            trace_ports: false,
            rwatch: Vec::new(),
            pc: (0, 0),
            rwatch_hits: Vec::new(),
            epoch: 0,
            wlog: Vec::new(),
            wlog_on: false,
            wmark: vec![0; (MEM_SIZE + 0x10000) / 64 + 1],
            cwatch: u32::MAX,
            creads: 0,
        }
    }

    #[inline]
    pub fn r8(&mut self, a: u32) -> u8 {
        if a.wrapping_sub(self.cwatch) < 2 {
            self.creads += 1;
        }
        if !self.rwatch.is_empty() && self.rwatch.contains(&a) && self.rwatch_hits.len() < 4000 {
            self.rwatch_hits.push((a, self.pc.0, self.pc.1));
        }
        let a = a as usize;
        if a >= 0xa0000 && a < 0xb0000 && self.vga.claims(a as u32) {
            self.epoch += 1; // loads the latches
            return self.vga.read(a as u32);
        }
        self.ram[a]
    }
    #[inline]
    pub fn w8(&mut self, a: u32, v: u8) {
        let a = a as usize;
        if a >= 0xa0000 {
            if a < 0xb0000 {
                if self.vga.claims(a as u32) {
                    self.epoch += 1;
                    self.vga.write(a as u32, v);
                }
                return;
            }
            if a >= 0xc0000 && a < 0x100000 {
                return; // ROM
            }
            if a >= 0xb8000 && a < 0xc0000 {
                self.vga.dirty += 1;
            }
        }
        let old = self.ram[a];
        if old != v {
            self.ram[a] = v;
            if !self.wlog_on {
                self.epoch += 1;
            } else if self.wmark[a >> 6] & 1 << (a & 63) == 0 {
                if self.wlog.len() < WLOG_MAX {
                    self.wmark[a >> 6] |= 1 << (a & 63);
                    self.wlog.push((a as u32, old));
                } else {
                    self.wlog_on = false;
                    self.epoch += 1;
                }
            }
        }
    }
    pub fn r16(&mut self, a: u32) -> u16 {
        self.r8(a) as u16 | (self.r8(a + 1) as u16) << 8
    }
    pub fn r32(&mut self, a: u32) -> u32 {
        self.r16(a) as u32 | (self.r16(a + 2) as u32) << 16
    }
    pub fn w16(&mut self, a: u32, v: u16) {
        self.w8(a, v as u8);
        self.w8(a + 1, (v >> 8) as u8);
    }
    pub fn w32(&mut self, a: u32, v: u32) {
        self.w16(a, v as u16);
        self.w16(a + 2, (v >> 16) as u16);
    }

    /// Start a new write log (Mem::wlog).
    pub fn wlog_restart(&mut self) {
        for &(a, _) in &self.wlog {
            self.wmark[a as usize >> 6] &= !(1 << (a & 63));
        }
        self.wlog.clear();
        self.wlog_on = true;
    }

    /// Plain RAM access for the host (no device side effects).
    pub fn write_bytes(&mut self, lin: u32, b: &[u8]) {
        self.ram[lin as usize..lin as usize + b.len()].copy_from_slice(b);
    }
    pub fn bytes(&self, lin: u32, n: usize) -> &[u8] {
        &self.ram[lin as usize..lin as usize + n]
    }
    pub fn rd16(&self, a: u32) -> u16 {
        u16::from_le_bytes([self.ram[a as usize], self.ram[a as usize + 1]])
    }
    pub fn wr16(&mut self, a: u32, v: u16) {
        self.ram[a as usize..a as usize + 2].copy_from_slice(&v.to_le_bytes());
    }
    /// NUL-terminated string at a linear address.
    pub fn cstr(&self, lin: u32) -> Vec<u8> {
        let s = &self.ram[lin as usize..];
        s[..s.iter().position(|&b| b == 0).unwrap_or(0).min(256)].to_vec()
    }

    /// Feed the next queued scancode to the controller if its buffer is free.
    pub fn kbd_pump(&mut self) {
        if !self.kbd_full {
            if let Some(s) = self.kbd_queue.pop_front() {
                self.epoch += 1;
                self.kbd_data = s;
                self.kbd_full = true;
                self.pic.raise(1);
            }
        }
    }

    fn retrace(&self) -> (bool, bool) {
        // 70 Hz frames (the 400-line modes), vertical retrace for the last 5%.
        let frame = IPS_PER_MS * 1000 / 70;
        let t = self.now % frame;
        (t >= frame - frame / 20, (self.now / 8) % 4 == 0)
    }

    pub fn inp(&mut self, port: u16, sz: u8) -> u32 {
        if sz == 2 {
            return self.inp(port, 1) | self.inp(port + 1, 1) << 8;
        }
        // Ports read time-dependent state (timers, retrace) or change it (keyboard).
        self.epoch += 1;
        let v = match port {
            0x20 | 0x21 => self.pic.inp(port == 0x20),
            0xa0 | 0xa1 => self.pic2.inp(port == 0xa0),
            0x40..=0x42 => self.pit.inp(port, self.now),
            0x60 => {
                self.kbd_full = false;
                self.kbd_data
            }
            0x61 => {
                // Bit 4 toggles with DRAM refresh (every ~15 us); bit 5 is PIT ch2 out.
                let refresh = ((self.now * 1000 / (IPS_PER_MS * 15)) & 1) as u8;
                (self.port61 & 0x0f) | refresh << 4
            }
            0x64 => 0x14 | self.kbd_full as u8,
            0x71 => match self.cmos_i & 0x7f {
                0x0a => 0x26,
                0x0b => 0x02,
                0x0d => 0x80,
                0x10 => 0x40,
                0x14 => 0x2f,
                0x15 => 0x80,
                0x16 => 0x02,
                _ => 0,
            },
            0x388 | 0x389 => self.opl.status(self.now),
            0x3b0..=0x3df => {
                let (v, h) = self.retrace();
                self.vga.inp(port, v, h)
            }
            _ => {
                self.unknown(port, "in");
                0xff
            }
        };
        if self.trace_ports {
            eprintln!("[in ] {port:04x} -> {v:02x}");
        }
        v as u32
    }

    pub fn outp(&mut self, port: u16, sz: u8, v: u32) {
        if sz == 2 {
            self.outp(port, 1, v & 0xff);
            self.outp(port + 1, 1, (v >> 8) & 0xff);
            return;
        }
        let v = v as u8;
        self.epoch += 1;
        if self.trace_ports {
            eprintln!("[out] {port:04x} <- {v:02x}");
        }
        match port {
            0x20 | 0x21 => self.pic.outp(port == 0x20, v),
            0xa0 | 0xa1 => self.pic2.outp(port == 0xa0, v),
            0x40..=0x43 => self.pit.outp(port, v, self.now),
            0x61 => self.port61 = v,
            0x70 => self.cmos_i = v,
            0x71 | 0x80 => {}
            0x388 | 0x389 => self.opl.write(port == 0x389, v, self.now),
            0x3b0..=0x3df => self.vga.outp(port, v),
            _ => self.unknown(port, "out"),
        }
    }

    fn unknown(&mut self, port: u16, dir: &str) {
        if self.unknown_ports.insert(port) {
            eprintln!("[io] unemulated port {port:04x} ({dir})");
        }
    }
}

impl Pic {
    fn save(&self, w: &mut crate::ser::W) {
        w.u8(self.imr); w.u8(self.irr); w.u8(self.isr); w.u8(self.base); w.u8(self.icw_step); w.bool(self.read_isr);
    }
    fn load(&mut self, r: &mut crate::ser::R) -> Result<(), String> {
        self.imr = r.u8()?; self.irr = r.u8()?; self.isr = r.u8()?; self.base = r.u8()?; self.icw_step = r.u8()?; self.read_isr = r.bool()?;
        Ok(())
    }
}

impl Pit {
    fn save(&self, w: &mut crate::ser::W) {
        for c in &self.ch {
            w.u16(c.reload); w.u8(c.access); w.bool(c.hi_next); w.opt_u16(c.latch); w.bool(c.latch_hi_next); w.u64(c.start);
        }
        w.u64(self.next_irq0);
    }
    fn load(&mut self, r: &mut crate::ser::R) -> Result<(), String> {
        for c in &mut self.ch {
            c.reload = r.u16()?; c.access = r.u8()?; c.hi_next = r.bool()?; c.latch = r.opt_u16()?; c.latch_hi_next = r.bool()?; c.start = r.u64()?;
        }
        self.next_irq0 = r.u64()?;
        Ok(())
    }
}

impl Mem {
    pub fn save(&self, w: &mut crate::ser::W) {
        w.bytes(&self.ram);
        self.vga.save(w);
        self.pic.save(w);
        self.pic2.save(w);
        self.pit.save(w);
        self.opl.save(w);
        let q: Vec<u8> = self.kbd_queue.iter().copied().collect();
        w.bytes(&q);
        w.u8(self.kbd_data); w.bool(self.kbd_full); w.u8(self.port61); w.u8(self.cmos_i); w.u64(self.now);
    }
    pub fn load(&mut self, r: &mut crate::ser::R) -> Result<(), String> {
        self.ram = r.bytes()?.to_vec();
        self.vga.load(r)?;
        self.pic.load(r)?;
        self.pic2.load(r)?;
        self.pit.load(r)?;
        self.opl.load(r)?;
        self.kbd_queue = r.bytes()?.iter().copied().collect();
        self.kbd_data = r.u8()?; self.kbd_full = r.bool()?; self.port61 = r.u8()?; self.cmos_i = r.u8()?; self.now = r.u64()?;
        Ok(())
    }
}
