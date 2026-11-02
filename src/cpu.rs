//! x86 interpreter for real-mode DOS code (8086/286 + the 386 extensions: 66/67
//! prefixes, MOVZX/MOVSX, SETcc, SHLD/SHRD, BTx, FS/GS).
//!
//! Adapted from koumei-rt's protected-mode core: a segment's base is `seg << 4`, and
//! IN/OUT go to the bus. Software interrupts, the host-callback opcode (`FE 38 nn`)
//! and anything outside the supported set leave `step` as an `Exit`, so the host
//! decides what to do instead of the CPU guessing.

use crate::mem::Mem;

pub const ES: usize = 0;
pub const CS: usize = 1;
pub const SS: usize = 2;
pub const DS: usize = 3;
pub const FS: usize = 4;
pub const GS: usize = 5;

pub const EAX: usize = 0;
pub const ECX: usize = 1;
pub const EDX: usize = 2;
pub const EBX: usize = 3;
pub const ESP: usize = 4;
pub const EBP: usize = 5;
pub const ESI: usize = 6;
pub const EDI: usize = 7;

pub const CF: u32 = 0x001;
pub const PF: u32 = 0x004;
pub const AF: u32 = 0x010;
pub const ZF: u32 = 0x040;
pub const SF: u32 = 0x080;
pub const IF: u32 = 0x200;
pub const DF: u32 = 0x400;
pub const OF: u32 = 0x800;

#[derive(Debug)]
pub enum Exit {
    Int(u8),
    /// `FE 38 nn`: the BIOS/DOS stub for host service `nn` (an invalid opcode on real
    /// hardware, the same trick DOSBox uses for its callbacks).
    Callback(u8),
    Halt,
    DivideError,
    Unimplemented(String),
}

#[derive(Clone, Copy)]
enum Rm {
    Reg(u8),
    Mem(usize, u32),
}

#[derive(Clone, Copy)]
struct Pfx {
    seg: Option<usize>,
    osz: u8,
    asz: u8,
    rep: u8,
}

#[derive(Clone)]
pub struct Cpu {
    pub r: [u32; 8],
    pub ip: u32,
    pub s: [u16; 6],
    pub sb: [u32; 6],
    pub fl: u32,
    /// IP of the instruction currently executing (for diagnostics / restarts).
    pub op_ip: u32,
    pub steps: u64,
    /// Set by MOV/POP SS: interrupts are held off for one instruction.
    pub int_shadow: bool,
}

fn mask(sz: u8) -> u32 {
    match sz {
        1 => 0xff,
        2 => 0xffff,
        _ => 0xffff_ffff,
    }
}

fn sbit(sz: u8) -> u32 {
    1u32 << (sz as u32 * 8 - 1)
}

fn sext(v: u32, sz: u8) -> u32 {
    match sz {
        1 => v as u8 as i8 as i32 as u32,
        2 => v as u16 as i16 as i32 as u32,
        _ => v,
    }
}

impl Cpu {
    pub fn new() -> Cpu {
        Cpu { r: [0; 8], ip: 0, s: [0; 6], sb: [0; 6], fl: 0x0202, op_ip: 0, steps: 0, int_shadow: false }
    }

    // ---------- registers ----------
    #[inline]
    pub fn ax(&self) -> u16 {
        self.r[EAX] as u16
    }
    #[inline]
    pub fn reg16(&self, i: usize) -> u16 {
        self.r[i] as u16
    }
    #[inline]
    pub fn set16(&mut self, i: usize, v: u16) {
        self.r[i] = (self.r[i] & 0xffff_0000) | v as u32;
    }
    pub fn reg8(&self, i: usize) -> u8 {
        if i < 4 { self.r[i] as u8 } else { (self.r[i - 4] >> 8) as u8 }
    }
    pub fn set8(&mut self, i: usize, v: u8) {
        if i < 4 {
            self.r[i] = (self.r[i] & !0xff) | v as u32;
        } else {
            self.r[i - 4] = (self.r[i - 4] & !0xff00) | ((v as u32) << 8);
        }
    }
    fn getr(&self, i: u8, sz: u8) -> u32 {
        match sz {
            1 => self.reg8(i as usize) as u32,
            2 => self.r[i as usize] & 0xffff,
            _ => self.r[i as usize],
        }
    }
    fn setr(&mut self, i: u8, sz: u8, v: u32) {
        match sz {
            1 => self.set8(i as usize, v as u8),
            2 => self.set16(i as usize, v as u16),
            _ => self.r[i as usize] = v,
        }
    }

    pub fn set_seg(&mut self, _m: &Mem, i: usize, seg: u16) {
        self.s[i] = seg;
        self.sb[i] = (seg as u32) << 4;
        if i == SS {
            self.int_shadow = true;
        }
    }

    #[inline]
    fn flag(&self, f: u32) -> bool {
        self.fl & f != 0
    }
    #[inline]
    fn setf(&mut self, f: u32, on: bool) {
        if on { self.fl |= f } else { self.fl &= !f }
    }

    // ---------- fetch ----------
    #[inline]
    fn fetch8(&mut self, m: &mut Mem) -> u8 {
        let v = m.r8(self.sb[CS].wrapping_add(self.ip));
        self.ip = (self.ip + 1) & 0xffff;
        v
    }
    #[inline]
    fn fetch16(&mut self, m: &mut Mem) -> u16 {
        let lo = self.fetch8(m) as u16;
        lo | (self.fetch8(m) as u16) << 8
    }
    fn fetch32(&mut self, m: &mut Mem) -> u32 {
        let lo = self.fetch16(m) as u32;
        lo | (self.fetch16(m) as u32) << 16
    }
    fn fetch_sz(&mut self, m: &mut Mem, sz: u8) -> u32 {
        match sz {
            1 => self.fetch8(m) as u32,
            2 => self.fetch16(m) as u32,
            _ => self.fetch32(m),
        }
    }

    // ---------- stack ----------
    pub fn push16(&mut self, m: &mut Mem, v: u16) {
        let sp = (self.r[ESP] as u16).wrapping_sub(2);
        self.set16(ESP, sp);
        m.w16(self.sb[SS].wrapping_add(sp as u32), v);
    }
    pub fn pop16(&mut self, m: &mut Mem) -> u16 {
        let sp = self.r[ESP] as u16;
        let v = m.r16(self.sb[SS].wrapping_add(sp as u32));
        self.set16(ESP, sp.wrapping_add(2));
        v
    }
    fn push32(&mut self, m: &mut Mem, v: u32) {
        let sp = (self.r[ESP] as u16).wrapping_sub(4);
        self.set16(ESP, sp);
        m.w32(self.sb[SS].wrapping_add(sp as u32), v);
    }
    fn pop32(&mut self, m: &mut Mem) -> u32 {
        let sp = self.r[ESP] as u16;
        let v = m.r32(self.sb[SS].wrapping_add(sp as u32));
        self.set16(ESP, sp.wrapping_add(4));
        v
    }
    fn push_sz(&mut self, m: &mut Mem, sz: u8, v: u32) {
        if sz == 4 { self.push32(m, v) } else { self.push16(m, v as u16) }
    }
    fn pop_sz(&mut self, m: &mut Mem, sz: u8) -> u32 {
        if sz == 4 { self.pop32(m) } else { self.pop16(m) as u32 }
    }
    /// Read a word at SS:SP+off without popping.
    pub fn stack16(&self, m: &mut Mem, off: u16) -> u16 {
        m.r16(self.sb[SS].wrapping_add((self.r[ESP] as u16).wrapping_add(off) as u32))
    }
    pub fn stack32(&self, m: &mut Mem, off: u16) -> u32 {
        self.stack16(m, off) as u32 | (self.stack16(m, off.wrapping_add(2)) as u32) << 16
    }

    // ---------- modrm ----------
    fn modrm(&mut self, m: &mut Mem, p: &Pfx) -> (u8, Rm) {
        let b = self.fetch8(m);
        let md = b >> 6;
        let reg = (b >> 3) & 7;
        let rm = b & 7;
        if md == 3 {
            return (reg, Rm::Reg(rm));
        }
        if p.asz == 2 {
            let r = |c: &Cpu, i: usize| c.r[i] & 0xffff;
            let (base, def_ss) = match rm {
                0 => (r(self, EBX) + r(self, ESI), false),
                1 => (r(self, EBX) + r(self, EDI), false),
                2 => (r(self, EBP) + r(self, ESI), true),
                3 => (r(self, EBP) + r(self, EDI), true),
                4 => (r(self, ESI), false),
                5 => (r(self, EDI), false),
                6 if md == 0 => (0, false),
                6 => (r(self, EBP), true),
                _ => (r(self, EBX), false),
            };
            let disp = match md {
                0 if rm == 6 => self.fetch16(m) as u32,
                0 => 0,
                1 => self.fetch8(m) as i8 as i32 as u32,
                _ => self.fetch16(m) as u32,
            };
            let seg = p.seg.unwrap_or(if def_ss { SS } else { DS });
            (reg, Rm::Mem(seg, base.wrapping_add(disp) & 0xffff))
        } else {
            let mut def_ss = false;
            let base = if rm == 4 {
                let sib = self.fetch8(m);
                let sc = sib >> 6;
                let idx = (sib >> 3) & 7;
                let bs = sib & 7;
                let index = if idx == 4 { 0 } else { self.r[idx as usize] << sc };
                let b = if bs == 5 && md == 0 {
                    self.fetch32(m)
                } else {
                    def_ss = bs == 4 || bs == 5;
                    self.r[bs as usize]
                };
                b.wrapping_add(index)
            } else if rm == 5 && md == 0 {
                self.fetch32(m)
            } else {
                def_ss = rm == 5;
                self.r[rm as usize]
            };
            let disp = match md {
                0 => 0,
                1 => self.fetch8(m) as i8 as i32 as u32,
                _ => self.fetch32(m),
            };
            let seg = p.seg.unwrap_or(if def_ss { SS } else { DS });
            (reg, Rm::Mem(seg, base.wrapping_add(disp)))
        }
    }

    #[inline]
    fn ea(&self, rm: Rm) -> u32 {
        match rm {
            Rm::Mem(s, off) => self.sb[s].wrapping_add(off),
            Rm::Reg(_) => unreachable!("register operand used as memory"),
        }
    }

    fn rd(&self, m: &mut Mem, rm: Rm, sz: u8) -> u32 {
        match rm {
            Rm::Reg(r) => self.getr(r, sz),
            Rm::Mem(..) => {
                let a = self.ea(rm);
                match sz {
                    1 => m.r8(a) as u32,
                    2 => m.r16(a) as u32,
                    _ => m.r32(a),
                }
            }
        }
    }

    fn wr(&mut self, m: &mut Mem, rm: Rm, sz: u8, v: u32) {
        match rm {
            Rm::Reg(r) => self.setr(r, sz, v),
            Rm::Mem(..) => {
                let a = self.ea(rm);
                match sz {
                    1 => m.w8(a, v as u8),
                    2 => m.w16(a, v as u16),
                    _ => m.w32(a, v),
                }
            }
        }
    }

    // ---------- flags / alu ----------
    fn szp(&mut self, r: u32, sz: u8) {
        let r = r & mask(sz);
        self.setf(ZF, r == 0);
        self.setf(SF, r & sbit(sz) != 0);
        self.setf(PF, (r as u8).count_ones() % 2 == 0);
    }

    fn alu(&mut self, op: u8, a: u32, b: u32, sz: u8) -> u32 {
        let mk = mask(sz);
        let (a, b) = (a & mk, b & mk);
        let r = match op {
            0 | 2 => {
                let c = if op == 2 && self.flag(CF) { 1 } else { 0 };
                let w = a as u64 + b as u64 + c;
                let r = w as u32 & mk;
                self.setf(CF, w > mk as u64);
                self.setf(OF, (a ^ r) & (b ^ r) & sbit(sz) != 0);
                self.setf(AF, (a ^ b ^ r) & 0x10 != 0);
                r
            }
            3 | 5 | 7 => {
                let c = if op == 3 && self.flag(CF) { 1 } else { 0 };
                let r = (a as u64).wrapping_sub(b as u64).wrapping_sub(c) as u32 & mk;
                self.setf(CF, (a as u64) < b as u64 + c);
                self.setf(OF, (a ^ b) & (a ^ r) & sbit(sz) != 0);
                self.setf(AF, (a ^ b ^ r) & 0x10 != 0);
                r
            }
            1 | 4 | 6 => {
                let r = match op { 1 => a | b, 4 => a & b, _ => a ^ b };
                self.setf(CF, false);
                self.setf(OF, false);
                self.setf(AF, false);
                r
            }
            _ => unreachable!(),
        };
        self.szp(r, sz);
        if op == 7 { a } else { r }
    }

    fn incdec(&mut self, v: u32, sz: u8, dec: bool) -> u32 {
        let cf = self.flag(CF);
        let r = self.alu(if dec { 5 } else { 0 }, v, 1, sz);
        self.setf(CF, cf);
        r
    }

    fn shift(&mut self, op: u8, v: u32, cnt: u32, sz: u8) -> u32 {
        let bits = sz as u32 * 8;
        let mk = mask(sz);
        let cnt = cnt & 31;
        if cnt == 0 {
            return v & mk;
        }
        let v = v & mk;
        let r;
        match op {
            0 => {
                let c = cnt % bits;
                r = if c == 0 { v } else { ((v << c) | (v >> (bits - c))) & mk };
                self.setf(CF, r & 1 != 0);
                self.setf(OF, ((r >> (bits - 1)) ^ r) & 1 != 0);
            }
            1 => {
                let c = cnt % bits;
                r = if c == 0 { v } else { ((v >> c) | (v << (bits - c))) & mk };
                self.setf(CF, r & sbit(sz) != 0);
                self.setf(OF, ((r >> (bits - 1)) ^ (r >> (bits - 2))) & 1 != 0);
            }
            2 | 3 => {
                let c = cnt % (bits + 1);
                let mut x = v as u64;
                let mut cf = self.flag(CF) as u64;
                for _ in 0..c {
                    if op == 2 {
                        let out = (x >> (bits - 1)) & 1;
                        x = ((x << 1) | cf) & mk as u64;
                        cf = out;
                    } else {
                        let out = x & 1;
                        x = (x >> 1) | (cf << (bits - 1));
                        cf = out;
                    }
                }
                r = x as u32;
                self.setf(CF, cf != 0);
                if op == 2 {
                    self.setf(OF, ((r >> (bits - 1)) as u64 ^ cf) & 1 != 0);
                } else {
                    self.setf(OF, ((r >> (bits - 1)) ^ (r >> (bits - 2))) & 1 != 0);
                }
            }
            4 | 6 => {
                r = if cnt >= bits { 0 } else { (v << cnt) & mk };
                self.setf(CF, cnt <= bits && (v >> (bits - cnt)) & 1 != 0);
                self.setf(OF, ((r >> (bits - 1)) & 1 != 0) != self.flag(CF));
                self.szp(r, sz);
            }
            5 => {
                r = if cnt >= bits { 0 } else { v >> cnt };
                self.setf(CF, cnt <= bits && (v >> (cnt - 1)) & 1 != 0);
                self.setf(OF, v & sbit(sz) != 0);
                self.szp(r, sz);
            }
            _ => {
                let sv = sext(v, sz) as i32;
                let c = cnt.min(bits - 1);
                r = ((sv >> c) as u32) & mk;
                self.setf(CF, (sv >> (cnt.min(bits) - 1)) & 1 != 0);
                self.setf(OF, false);
                self.szp(r, sz);
            }
        }
        r
    }

    fn cond(&self, cc: u8) -> bool {
        let r = match cc >> 1 {
            0 => self.flag(OF),
            1 => self.flag(CF),
            2 => self.flag(ZF),
            3 => self.flag(CF) || self.flag(ZF),
            4 => self.flag(SF),
            5 => self.flag(PF),
            6 => self.flag(SF) != self.flag(OF),
            _ => self.flag(ZF) || (self.flag(SF) != self.flag(OF)),
        };
        r != (cc & 1 != 0)
    }

    fn far_jump(&mut self, m: &mut Mem, sel: u16, off: u32) {
        self.set_seg(m, CS, sel);
        self.ip = off & 0xffff;
    }

    /// Far call from host code: push return CS:IP and jump.
    pub fn call_far(&mut self, m: &mut Mem, sel: u16, off: u16) {
        let (cs, ip) = (self.s[CS], self.ip as u16);
        self.push16(m, cs);
        self.push16(m, ip);
        self.far_jump(m, sel, off as u32);
    }

    /// `RETF n` performed on behalf of a host-implemented function.
    pub fn retf(&mut self, m: &mut Mem, pop: u16) {
        let ip = self.pop16(m);
        let cs = self.pop16(m);
        let sp = (self.r[ESP] as u16).wrapping_add(pop);
        self.set16(ESP, sp);
        self.far_jump(m, cs, ip as u32);
    }

    fn get_flags(&self) -> u32 {
        (self.fl & 0x0003_7fd5) | 0x2
    }

    fn set_flags(&mut self, v: u32, sz: u8) {
        let keep = if sz == 2 { self.fl & 0xffff_0000 } else { 0 };
        self.fl = keep | (v & mask(sz) & 0x0003_7fd5) | 0x2;
    }

    // ---------- string ops ----------
    fn str_op(&mut self, m: &mut Mem, op: u8, p: &Pfx) -> Result<(), Exit> {
        let sz = if op & 1 == 0 { 1 } else { p.osz };
        let step = if self.flag(DF) { (sz as u32).wrapping_neg() } else { sz as u32 };
        let src_seg = p.seg.unwrap_or(DS);
        let am = mask(p.asz);
        let idx = |c: &Cpu, i: usize| c.r[i] & am;
        let adv = |c: &mut Cpu, i: usize| {
            let v = (c.r[i].wrapping_add(step)) & am;
            c.r[i] = (c.r[i] & !am) | v;
        };
        let read = |m: &mut Mem, a: u32| match sz { 1 => m.r8(a) as u32, 2 => m.r16(a) as u32, _ => m.r32(a) };
        loop {
            if p.rep != 0 && self.r[ECX] & am == 0 {
                break;
            }
            match op {
                0xa4 | 0xa5 => {
                    let v = read(m, self.sb[src_seg].wrapping_add(idx(self, ESI)));
                    let d = self.sb[ES].wrapping_add(idx(self, EDI));
                    match sz { 1 => m.w8(d, v as u8), 2 => m.w16(d, v as u16), _ => m.w32(d, v) }
                    adv(self, ESI);
                    adv(self, EDI);
                }
                0xa6 | 0xa7 => {
                    let a = read(m, self.sb[src_seg].wrapping_add(idx(self, ESI)));
                    let b = read(m, self.sb[ES].wrapping_add(idx(self, EDI)));
                    self.alu(7, a, b, sz);
                    adv(self, ESI);
                    adv(self, EDI);
                }
                0xaa | 0xab => {
                    let d = self.sb[ES].wrapping_add(idx(self, EDI));
                    let v = self.getr(0, sz);
                    match sz { 1 => m.w8(d, v as u8), 2 => m.w16(d, v as u16), _ => m.w32(d, v) }
                    adv(self, EDI);
                }
                0xac | 0xad => {
                    let v = read(m, self.sb[src_seg].wrapping_add(idx(self, ESI)));
                    self.setr(0, sz, v);
                    adv(self, ESI);
                }
                0xae | 0xaf => {
                    let b = read(m, self.sb[ES].wrapping_add(idx(self, EDI)));
                    let a = self.getr(0, sz);
                    self.alu(7, a, b, sz);
                    adv(self, EDI);
                }
                0x6c | 0x6d => {
                    let d = self.sb[ES].wrapping_add(idx(self, EDI));
                    let v = m.inp(self.r[EDX] as u16, sz);
                    match sz { 1 => m.w8(d, v as u8), 2 => m.w16(d, v as u16), _ => m.w32(d, v) }
                    adv(self, EDI);
                }
                0x6e | 0x6f => {
                    let v = read(m, self.sb[src_seg].wrapping_add(idx(self, ESI)));
                    m.outp(self.r[EDX] as u16, sz, v);
                    adv(self, ESI);
                }
                _ => unreachable!(),
            }
            if p.rep == 0 {
                break;
            }
            let c = (self.r[ECX].wrapping_sub(1)) & am;
            self.r[ECX] = (self.r[ECX] & !am) | c;
            if matches!(op, 0xa6 | 0xa7 | 0xae | 0xaf) {
                let z = self.flag(ZF);
                if (p.rep == 0xf3 && !z) || (p.rep == 0xf2 && z) {
                    break;
                }
            }
        }
        Ok(())
    }

    // ---------- main step ----------
    pub fn step(&mut self, m: &mut Mem) -> Result<(), Exit> {
        self.op_ip = self.ip;
        self.steps += 1;
        let mut p = Pfx { seg: None, osz: 2, asz: 2, rep: 0 };
        let mut b;
        loop {
            b = self.fetch8(m);
            match b {
                0x26 => p.seg = Some(ES),
                0x2e => p.seg = Some(CS),
                0x36 => p.seg = Some(SS),
                0x3e => p.seg = Some(DS),
                0x64 => p.seg = Some(FS),
                0x65 => p.seg = Some(GS),
                0x66 => p.osz = 4,
                0x67 => p.asz = 4,
                0xf2 | 0xf3 => p.rep = b,
                0xf0 => {}
                _ => break,
            }
        }
        let osz = p.osz;
        match b {
            // ALU r/m forms
            0x00..=0x3f if b & 7 < 6 => {
                let op = b >> 3;
                match b & 7 {
                    0 | 1 => {
                        let sz = if b & 1 == 0 { 1 } else { osz };
                        let (reg, rm) = self.modrm(m, &p);
                        let a = self.rd(m, rm, sz);
                        let r = self.alu(op, a, self.getr(reg, sz), sz);
                        if op != 7 { self.wr(m, rm, sz, r) }
                    }
                    2 | 3 => {
                        let sz = if b & 1 == 0 { 1 } else { osz };
                        let (reg, rm) = self.modrm(m, &p);
                        let v = self.rd(m, rm, sz);
                        let r = self.alu(op, self.getr(reg, sz), v, sz);
                        if op != 7 { self.setr(reg, sz, r) }
                    }
                    4 => {
                        let v = self.fetch8(m) as u32;
                        let r = self.alu(op, self.getr(0, 1), v, 1);
                        if op != 7 { self.setr(0, 1, r) }
                    }
                    _ => {
                        let v = self.fetch_sz(m, osz);
                        let r = self.alu(op, self.getr(0, osz), v, osz);
                        if op != 7 { self.setr(0, osz, r) }
                    }
                }
            }
            0x06 => { let v = self.s[ES]; self.push_sz(m, osz, v as u32) }
            0x07 => { let v = self.pop_sz(m, osz) as u16; self.set_seg(m, ES, v) }
            0x0e => { let v = self.s[CS]; self.push_sz(m, osz, v as u32) }
            0x16 => { let v = self.s[SS]; self.push_sz(m, osz, v as u32) }
            0x17 => { let v = self.pop_sz(m, osz) as u16; self.set_seg(m, SS, v) }
            0x1e => { let v = self.s[DS]; self.push_sz(m, osz, v as u32) }
            0x1f => { let v = self.pop_sz(m, osz) as u16; self.set_seg(m, DS, v) }
            0x0f => return self.step_0f(m, &p),
            0x27 | 0x2f => {
                // DAA / DAS
                let mut al = self.reg8(0) as u32;
                let old_al = al;
                let old_cf = self.flag(CF);
                let sub = b == 0x2f;
                if al & 0xf > 9 || self.flag(AF) {
                    al = if sub { al.wrapping_sub(6) } else { al + 6 };
                    self.setf(AF, true);
                } else {
                    self.setf(AF, false);
                }
                if old_al > 0x99 || old_cf {
                    al = if sub { al.wrapping_sub(0x60) } else { al + 0x60 };
                    self.setf(CF, true);
                } else {
                    self.setf(CF, false);
                }
                self.set8(0, al as u8);
                self.szp(al, 1);
            }
            0x37 | 0x3f => {
                // AAA / AAS
                if self.reg8(0) & 0xf > 9 || self.flag(AF) {
                    let ax = self.ax();
                    let ax = if b == 0x37 { ax.wrapping_add(0x106) } else { ax.wrapping_sub(6).wrapping_sub(0x100) };
                    self.set16(EAX, ax);
                    self.setf(AF, true);
                    self.setf(CF, true);
                } else {
                    self.setf(AF, false);
                    self.setf(CF, false);
                }
                let al = self.reg8(0) & 0xf;
                self.set8(0, al);
            }
            0x40..=0x47 => {
                let r = (b & 7) as u8;
                let v = self.incdec(self.getr(r, osz), osz, false);
                self.setr(r, osz, v);
            }
            0x48..=0x4f => {
                let r = (b & 7) as u8;
                let v = self.incdec(self.getr(r, osz), osz, true);
                self.setr(r, osz, v);
            }
            0x50..=0x57 => {
                let v = self.getr(b & 7, osz);
                self.push_sz(m, osz, v);
            }
            0x58..=0x5f => {
                let v = self.pop_sz(m, osz);
                self.setr(b & 7, osz, v);
            }
            0x60 => {
                let sp = self.getr(ESP as u8, osz);
                for i in 0..8u8 {
                    let v = if i == 4 { sp } else { self.getr(i, osz) };
                    self.push_sz(m, osz, v);
                }
            }
            0x61 => {
                for i in (0..8u8).rev() {
                    let v = self.pop_sz(m, osz);
                    if i != 4 { self.setr(i, osz, v) }
                }
            }
            0x62 => {
                // BOUND: assume in range.
                let _ = self.modrm(m, &p);
            }
            0x68 => { let v = self.fetch_sz(m, osz); self.push_sz(m, osz, v) }
            0x6a => { let v = self.fetch8(m) as i8 as i32 as u32; self.push_sz(m, osz, v) }
            0x69 | 0x6b => {
                let (reg, rm) = self.modrm(m, &p);
                let a = self.rd(m, rm, osz);
                let imm = if b == 0x6b { self.fetch8(m) as i8 as i32 as u32 } else { self.fetch_sz(m, osz) };
                let r = self.imul2(a, imm, osz);
                self.setr(reg, osz, r);
            }
            0x6c..=0x6f => self.str_op(m, b, &p)?,
            0x70..=0x7f => {
                let d = self.fetch8(m) as i8 as i32 as u32;
                if self.cond(b & 0xf) {
                    self.ip = self.ip.wrapping_add(d) & 0xffff;
                }
            }
            0x80..=0x83 => {
                let sz = if b & 1 == 0 { 1 } else { osz };
                let (op, rm) = self.modrm(m, &p);
                let a = self.rd(m, rm, sz);
                let imm = if b == 0x83 { self.fetch8(m) as i8 as i32 as u32 } else { self.fetch_sz(m, sz) };
                let r = self.alu(op, a, imm, sz);
                if op != 7 { self.wr(m, rm, sz, r) }
            }
            0x84 | 0x85 => {
                let sz = if b & 1 == 0 { 1 } else { osz };
                let (reg, rm) = self.modrm(m, &p);
                let a = self.rd(m, rm, sz);
                self.alu(4, a, self.getr(reg, sz), sz);
            }
            0x86 | 0x87 => {
                let sz = if b & 1 == 0 { 1 } else { osz };
                let (reg, rm) = self.modrm(m, &p);
                let a = self.rd(m, rm, sz);
                let r = self.getr(reg, sz);
                self.wr(m, rm, sz, r);
                self.setr(reg, sz, a);
            }
            0x88..=0x8b => {
                let sz = if b & 1 == 0 { 1 } else { osz };
                let (reg, rm) = self.modrm(m, &p);
                if b & 2 == 0 {
                    let v = self.getr(reg, sz);
                    self.wr(m, rm, sz, v);
                } else {
                    let v = self.rd(m, rm, sz);
                    self.setr(reg, sz, v);
                }
            }
            0x8c => {
                let (reg, rm) = self.modrm(m, &p);
                let v = self.s[reg as usize] as u32;
                let sz = if matches!(rm, Rm::Reg(_)) { osz } else { 2 };
                self.wr(m, rm, sz, v);
            }
            0x8d => {
                let (reg, rm) = self.modrm(m, &p);
                let Rm::Mem(_, off) = rm else { return Err(Exit::Unimplemented("LEA reg".into())) };
                self.setr(reg, osz, off);
            }
            0x8e => {
                let (reg, rm) = self.modrm(m, &p);
                let v = self.rd(m, rm, 2) as u16;
                self.set_seg(m, reg as usize, v);
            }
            0x8f => {
                let v = self.pop_sz(m, osz);
                let (_, rm) = self.modrm(m, &p);
                self.wr(m, rm, osz, v);
            }
            0x90 => {}
            0x91..=0x97 => {
                let r = b & 7;
                let a = self.getr(0, osz);
                let v = self.getr(r, osz);
                self.setr(0, osz, v);
                self.setr(r, osz, a);
            }
            0x98 => {
                if osz == 2 {
                    let v = self.reg8(0) as i8 as i16 as u16;
                    self.set16(EAX, v);
                } else {
                    self.r[EAX] = self.r[EAX] as u16 as i16 as i32 as u32;
                }
            }
            0x99 => {
                if osz == 2 {
                    let v = if self.ax() & 0x8000 != 0 { 0xffff } else { 0 };
                    self.set16(EDX, v);
                } else {
                    self.r[EDX] = if self.r[EAX] & 0x8000_0000 != 0 { 0xffff_ffff } else { 0 };
                }
            }
            0x9a => {
                let off = self.fetch_sz(m, osz);
                let sel = self.fetch16(m);
                let (cs, ip) = (self.s[CS], self.ip);
                self.push_sz(m, osz, cs as u32);
                self.push_sz(m, osz, ip);
                self.far_jump(m, sel, off);
            }
            0x9b => {}
            0x9c => { let f = self.get_flags(); self.push_sz(m, osz, f) }
            0x9d => { let f = self.pop_sz(m, osz); self.set_flags(f, osz) }
            0x9e => {
                let ah = self.reg8(4) as u32;
                self.fl = (self.fl & !0xd5) | (ah & 0xd5);
            }
            0x9f => { let f = (self.get_flags() & 0xff) as u8; self.set8(4, f) }
            0xa0..=0xa3 => {
                let sz = if b & 1 == 0 { 1 } else { osz };
                let off = self.fetch_sz(m, p.asz);
                let rm = Rm::Mem(p.seg.unwrap_or(DS), off);
                if b & 2 == 0 {
                    let v = self.rd(m, rm, sz);
                    self.setr(0, sz, v);
                } else {
                    let v = self.getr(0, sz);
                    self.wr(m, rm, sz, v);
                }
            }
            0xa4..=0xa7 | 0xaa..=0xaf => self.str_op(m, b, &p)?,
            0xa8 => { let v = self.fetch8(m) as u32; self.alu(4, self.getr(0, 1), v, 1); }
            0xa9 => { let v = self.fetch_sz(m, osz); self.alu(4, self.getr(0, osz), v, osz); }
            0xb0..=0xb7 => { let v = self.fetch8(m); self.set8((b & 7) as usize, v) }
            0xb8..=0xbf => { let v = self.fetch_sz(m, osz); self.setr(b & 7, osz, v) }
            0xc0 | 0xc1 | 0xd0..=0xd3 => {
                let sz = if b & 1 == 0 { 1 } else { osz };
                let (op, rm) = self.modrm(m, &p);
                let v = self.rd(m, rm, sz);
                let cnt = match b {
                    0xc0 | 0xc1 => self.fetch8(m) as u32,
                    0xd0 | 0xd1 => 1,
                    _ => self.reg8(1) as u32,
                };
                let r = self.shift(op, v, cnt, sz);
                self.wr(m, rm, sz, r);
            }
            0xc2 => {
                let n = self.fetch16(m);
                self.ip = self.pop_sz(m, osz) & 0xffff;
                let sp = (self.r[ESP] as u16).wrapping_add(n);
                self.set16(ESP, sp);
            }
            0xc3 => self.ip = self.pop_sz(m, osz) & 0xffff,
            0xc4 | 0xc5 => {
                let (reg, rm) = self.modrm(m, &p);
                let a = self.ea(rm);
                let off = if osz == 4 { m.r32(a) } else { m.r16(a) as u32 };
                let sel = m.r16(a + osz as u32);
                self.setr(reg, osz, off);
                self.set_seg(m, if b == 0xc4 { ES } else { DS }, sel);
            }
            0xc6 | 0xc7 => {
                let sz = if b & 1 == 0 { 1 } else { osz };
                let (_, rm) = self.modrm(m, &p);
                let v = self.fetch_sz(m, sz);
                self.wr(m, rm, sz, v);
            }
            0xc8 => {
                let size = self.fetch16(m);
                let level = self.fetch8(m) & 31;
                let bp = self.getr(EBP as u8, osz);
                self.push_sz(m, osz, bp);
                let frame = self.r[ESP] as u16;
                for i in 1..level {
                    let a = self.sb[SS].wrapping_add((self.r[EBP] as u16).wrapping_sub(i as u16 * osz as u16) as u32);
                    let v = if osz == 4 { m.r32(a) } else { m.r16(a) as u32 };
                    self.push_sz(m, osz, v);
                }
                if level > 0 {
                    self.push_sz(m, osz, frame as u32);
                }
                self.setr(EBP as u8, osz, frame as u32);
                let sp = (self.r[ESP] as u16).wrapping_sub(size);
                self.set16(ESP, sp);
            }
            0xc9 => {
                let bp = self.r[EBP] as u16;
                self.set16(ESP, bp);
                let v = self.pop_sz(m, osz);
                self.setr(EBP as u8, osz, v);
            }
            0xca | 0xcb => {
                let n = if b == 0xca { self.fetch16(m) } else { 0 };
                let ip = self.pop_sz(m, osz);
                let cs = self.pop_sz(m, osz) as u16;
                let sp = (self.r[ESP] as u16).wrapping_add(n);
                self.set16(ESP, sp);
                self.far_jump(m, cs, ip);
            }
            0xcc => return Err(Exit::Int(3)),
            0xcd => {
                let n = self.fetch8(m);
                return Err(Exit::Int(n));
            }
            0xce => {
                if self.flag(OF) {
                    return Err(Exit::Int(4));
                }
            }
            0xcf => {
                let ip = self.pop_sz(m, osz);
                let cs = self.pop_sz(m, osz) as u16;
                let f = self.pop_sz(m, osz);
                self.set_flags(f, osz);
                self.far_jump(m, cs, ip);
            }
            0xd4 => {
                let base = self.fetch8(m);
                if base == 0 {
                    return Err(Exit::DivideError);
                }
                let al = self.reg8(0);
                self.set8(4, al / base);
                self.set8(0, al % base);
                self.szp(self.reg8(0) as u32, 1);
            }
            0xd5 => {
                let base = self.fetch8(m);
                let al = self.reg8(0).wrapping_add(self.reg8(4).wrapping_mul(base));
                self.set16(EAX, al as u16);
                self.szp(al as u32, 1);
            }
            0xd6 => { let v = if self.flag(CF) { 0xff } else { 0 }; self.set8(0, v) }
            0xd7 => {
                let a = self.sb[p.seg.unwrap_or(DS)].wrapping_add(((self.r[EBX] as u16).wrapping_add(self.reg8(0) as u16)) as u32);
                let v = m.r8(a);
                self.set8(0, v);
            }
            0xd8..=0xdf => {
                return Err(Exit::Unimplemented(format!("x87 opcode {b:02x}")));
            }
            0xe0..=0xe3 => {
                let d = self.fetch8(m) as i8 as i32 as u32;
                let am = mask(p.asz);
                let taken = if b == 0xe3 {
                    self.r[ECX] & am == 0
                } else {
                    let c = self.r[ECX].wrapping_sub(1) & am;
                    self.r[ECX] = (self.r[ECX] & !am) | c;
                    c != 0 && match b {
                        0xe0 => !self.flag(ZF),
                        0xe1 => self.flag(ZF),
                        _ => true,
                    }
                };
                if taken {
                    self.ip = self.ip.wrapping_add(d) & 0xffff;
                }
            }
            0xe4 | 0xe5 => {
                let port = self.fetch8(m) as u16;
                let sz = if b == 0xe4 { 1 } else { osz };
                let v = m.inp(port, sz);
                self.setr(0, sz, v);
            }
            0xe6 | 0xe7 => {
                let port = self.fetch8(m) as u16;
                let sz = if b == 0xe6 { 1 } else { osz };
                m.outp(port, sz, self.getr(0, sz));
            }
            0xec | 0xed => {
                let sz = if b == 0xec { 1 } else { osz };
                let v = m.inp(self.r[EDX] as u16, sz);
                self.setr(0, sz, v);
            }
            0xee | 0xef => {
                let sz = if b == 0xee { 1 } else { osz };
                m.outp(self.r[EDX] as u16, sz, self.getr(0, sz));
            }
            0xe8 => {
                let d = self.fetch_sz(m, osz);
                let ip = self.ip;
                self.push_sz(m, osz, ip);
                self.ip = self.ip.wrapping_add(d) & 0xffff;
            }
            0xe9 => {
                let d = self.fetch_sz(m, osz);
                self.ip = self.ip.wrapping_add(d) & 0xffff;
            }
            0xea => {
                let off = self.fetch_sz(m, osz);
                let sel = self.fetch16(m);
                self.far_jump(m, sel, off);
            }
            0xeb => {
                let d = self.fetch8(m) as i8 as i32 as u32;
                self.ip = self.ip.wrapping_add(d) & 0xffff;
            }
            0xf4 => return Err(Exit::Halt),
            0xf5 => { let c = self.flag(CF); self.setf(CF, !c) }
            0xf6 | 0xf7 => {
                let sz = if b & 1 == 0 { 1 } else { osz };
                let (op, rm) = self.modrm(m, &p);
                let v = self.rd(m, rm, sz);
                match op {
                    0 | 1 => {
                        let imm = self.fetch_sz(m, sz);
                        self.alu(4, v, imm, sz);
                    }
                    2 => self.wr(m, rm, sz, !v & mask(sz)),
                    3 => {
                        let r = self.alu(5, 0, v, sz);
                        self.setf(CF, v & mask(sz) != 0);
                        self.wr(m, rm, sz, r);
                    }
                    4 | 5 => self.mul1(v, sz, op == 5),
                    _ => self.div1(v, sz, op == 7)?,
                }
            }
            0xf8 => self.setf(CF, false),
            0xf9 => self.setf(CF, true),
            0xfa => self.setf(IF, false),
            0xfb => self.setf(IF, true),
            0xfc => self.setf(DF, false),
            0xfd => self.setf(DF, true),
            0xfe | 0xff => {
                let sz = if b == 0xfe { 1 } else { osz };
                if b == 0xfe && m.r8(self.sb[CS].wrapping_add(self.ip)) == 0x38 {
                    self.fetch8(m);
                    return Err(Exit::Callback(self.fetch8(m)));
                }
                let (op, rm) = self.modrm(m, &p);
                match op {
                    0 | 1 => {
                        let v = self.rd(m, rm, sz);
                        let r = self.incdec(v, sz, op == 1);
                        self.wr(m, rm, sz, r);
                    }
                    2 => {
                        let t = self.rd(m, rm, osz);
                        let ip = self.ip;
                        self.push_sz(m, osz, ip);
                        self.ip = t & 0xffff;
                    }
                    3 | 5 => {
                        let a = self.ea(rm);
                        let off = if osz == 4 { m.r32(a) } else { m.r16(a) as u32 };
                        let sel = m.r16(a + osz as u32);
                        if op == 3 {
                            let (cs, ip) = (self.s[CS], self.ip);
                            self.push_sz(m, osz, cs as u32);
                            self.push_sz(m, osz, ip);
                        }
                        self.far_jump(m, sel, off);
                    }
                    4 => self.ip = self.rd(m, rm, osz) & 0xffff,
                    6 => {
                        let v = self.rd(m, rm, osz);
                        self.push_sz(m, osz, v);
                    }
                    _ => return Err(Exit::Unimplemented(format!("grp5 /7 {b:02x}"))),
                }
            }
            _ => return Err(Exit::Unimplemented(format!("opcode {b:02x}"))),
        }
        Ok(())
    }

    fn imul2(&mut self, a: u32, b: u32, sz: u8) -> u32 {
        let full = sext(a, sz) as i32 as i64 * sext(b, sz) as i32 as i64;
        let r = full as u32 & mask(sz);
        let ovf = sext(r, sz) as i32 as i64 != full;
        self.setf(CF, ovf);
        self.setf(OF, ovf);
        r
    }

    fn mul1(&mut self, v: u32, sz: u8, signed: bool) {
        match sz {
            1 => {
                let al = self.reg8(0);
                let (r, ovf) = if signed {
                    let r = (al as i8 as i16) * (v as u8 as i8 as i16);
                    (r as u16, r != r as i8 as i16)
                } else {
                    let r = al as u16 * v as u8 as u16;
                    (r, r >> 8 != 0)
                };
                self.set16(EAX, r);
                self.setf(CF, ovf);
                self.setf(OF, ovf);
            }
            2 => {
                let ax = self.ax();
                let (r, ovf) = if signed {
                    let r = (ax as i16 as i32) * (v as u16 as i16 as i32);
                    (r as u32, r != r as i16 as i32)
                } else {
                    let r = ax as u32 * v as u16 as u32;
                    (r, r >> 16 != 0)
                };
                self.set16(EAX, r as u16);
                self.set16(EDX, (r >> 16) as u16);
                self.setf(CF, ovf);
                self.setf(OF, ovf);
            }
            _ => {
                let a = self.r[EAX];
                let (r, ovf) = if signed {
                    let r = (a as i32 as i64) * (v as i32 as i64);
                    (r as u64, r != r as i32 as i64)
                } else {
                    let r = a as u64 * v as u64;
                    (r, r >> 32 != 0)
                };
                self.r[EAX] = r as u32;
                self.r[EDX] = (r >> 32) as u32;
                self.setf(CF, ovf);
                self.setf(OF, ovf);
            }
        }
    }

    fn div1(&mut self, v: u32, sz: u8, signed: bool) -> Result<(), Exit> {
        let v = v & mask(sz);
        if v == 0 {
            return Err(Exit::DivideError);
        }
        match sz {
            1 => {
                let n = self.ax();
                if signed {
                    let (n, d) = (n as i16 as i32, v as u8 as i8 as i32);
                    let (q, r) = (n / d, n % d);
                    if q != q as i8 as i32 { return Err(Exit::DivideError) }
                    self.set8(0, q as u8);
                    self.set8(4, r as u8);
                } else {
                    let (q, r) = (n as u32 / v, n as u32 % v);
                    if q > 0xff { return Err(Exit::DivideError) }
                    self.set8(0, q as u8);
                    self.set8(4, r as u8);
                }
            }
            2 => {
                let n = (self.reg16(EDX) as u32) << 16 | self.ax() as u32;
                if signed {
                    let (n, d) = (n as i32 as i64, v as u16 as i16 as i64);
                    let (q, r) = (n / d, n % d);
                    if q != q as i16 as i64 { return Err(Exit::DivideError) }
                    self.set16(EAX, q as u16);
                    self.set16(EDX, r as u16);
                } else {
                    let (q, r) = (n / v, n % v);
                    if q > 0xffff { return Err(Exit::DivideError) }
                    self.set16(EAX, q as u16);
                    self.set16(EDX, r as u16);
                }
            }
            _ => {
                let n = (self.r[EDX] as u64) << 32 | self.r[EAX] as u64;
                if signed {
                    let (n, d) = (n as i64 as i128, v as i32 as i128);
                    let (q, r) = (n / d, n % d);
                    if q != q as i32 as i128 { return Err(Exit::DivideError) }
                    self.r[EAX] = q as u32;
                    self.r[EDX] = r as u32;
                } else {
                    let (q, r) = (n / v as u64, n % v as u64);
                    if q > 0xffff_ffff { return Err(Exit::DivideError) }
                    self.r[EAX] = q as u32;
                    self.r[EDX] = r as u32;
                }
            }
        }
        Ok(())
    }

    fn step_0f(&mut self, m: &mut Mem, p: &Pfx) -> Result<(), Exit> {
        let b = self.fetch8(m);
        let osz = p.osz;
        match b {
            0x01 => {
                // SMSW (MSW of a 386 in real mode: only ET set); the rest is protected-mode only.
                let (op, rm) = self.modrm(m, p);
                if op != 4 {
                    return Err(Exit::Unimplemented(format!("0f 01 /{op}")));
                }
                self.wr(m, rm, 2, 0x0010);
            }
            0x80..=0x8f => {
                let d = if osz == 4 { self.fetch32(m) } else { self.fetch16(m) as i16 as i32 as u32 };
                if self.cond(b & 0xf) {
                    self.ip = self.ip.wrapping_add(d) & 0xffff;
                }
            }
            0x90..=0x9f => {
                let (_, rm) = self.modrm(m, p);
                let v = self.cond(b & 0xf) as u32;
                self.wr(m, rm, 1, v);
            }
            0xa0 => { let v = self.s[FS]; self.push_sz(m, osz, v as u32) }
            0xa1 => { let v = self.pop_sz(m, osz) as u16; self.set_seg(m, FS, v) }
            0xa8 => { let v = self.s[GS]; self.push_sz(m, osz, v as u32) }
            0xa9 => { let v = self.pop_sz(m, osz) as u16; self.set_seg(m, GS, v) }
            0xa3 | 0xab | 0xb3 | 0xbb | 0xba => {
                let (reg, rm) = self.modrm(m, p);
                let (op, bit) = if b == 0xba {
                    let imm = self.fetch8(m) as u32;
                    (reg, imm)
                } else {
                    (match b { 0xa3 => 4, 0xab => 5, 0xb3 => 6, _ => 7 }, self.getr(reg, osz))
                };
                let bits = osz as u32 * 8;
                // Register-sourced bit offsets on memory operands address beyond the word.
                let rm = match (rm, b != 0xba) {
                    (Rm::Mem(s, off), true) => {
                        let idx = sext(bit, osz) as i32 >> if osz == 4 { 5 } else { 4 };
                        Rm::Mem(s, off.wrapping_add((idx * osz as i32) as u32))
                    }
                    (r, _) => r,
                };
                let bit = bit % bits;
                let v = self.rd(m, rm, osz);
                self.setf(CF, (v >> bit) & 1 != 0);
                let nv = match op {
                    5 => v | (1 << bit),
                    6 => v & !(1 << bit),
                    7 => v ^ (1 << bit),
                    _ => v,
                };
                if op != 4 {
                    self.wr(m, rm, osz, nv);
                }
            }
            0xa4 | 0xa5 | 0xac | 0xad => {
                let (reg, rm) = self.modrm(m, p);
                let cnt = if b & 1 == 0 { self.fetch8(m) as u32 } else { self.reg8(1) as u32 } & 31;
                if cnt != 0 {
                    let bits = osz as u32 * 8;
                    let d = self.rd(m, rm, osz) as u64;
                    let s = self.getr(reg, osz) as u64;
                    let r = if b < 0xa8 {
                        let x = (d << bits) | s;
                        ((x << cnt) >> bits) as u32 & mask(osz)
                    } else {
                        let x = (s << bits) | d;
                        (x >> cnt) as u32 & mask(osz)
                    };
                    let cf = if b < 0xa8 { (d >> (bits - cnt)) & 1 } else { (d >> (cnt - 1)) & 1 };
                    self.setf(CF, cf != 0);
                    self.szp(r, osz);
                    self.wr(m, rm, osz, r);
                }
            }
            0xaf => {
                let (reg, rm) = self.modrm(m, p);
                let v = self.rd(m, rm, osz);
                let r = self.imul2(self.getr(reg, osz), v, osz);
                self.setr(reg, osz, r);
            }
            0xb2 | 0xb4 | 0xb5 => {
                let (reg, rm) = self.modrm(m, p);
                let a = self.ea(rm);
                let off = if osz == 4 { m.r32(a) } else { m.r16(a) as u32 };
                let sel = m.r16(a + osz as u32);
                self.setr(reg, osz, off);
                self.set_seg(m, match b { 0xb2 => SS, 0xb4 => FS, _ => GS }, sel);
            }
            0xb6 | 0xb7 | 0xbe | 0xbf => {
                let (reg, rm) = self.modrm(m, p);
                let ssz = if b & 1 == 0 { 1 } else { 2 };
                let v = self.rd(m, rm, ssz);
                let v = if b >= 0xbe { sext(v, ssz) } else { v };
                self.setr(reg, osz, v);
            }
            0xbc | 0xbd => {
                let (reg, rm) = self.modrm(m, p);
                let v = self.rd(m, rm, osz);
                if v == 0 {
                    self.setf(ZF, true);
                } else {
                    self.setf(ZF, false);
                    let r = if b == 0xbc { v.trailing_zeros() } else { 31 - v.leading_zeros() };
                    self.setr(reg, osz, r);
                }
            }
            _ => return Err(Exit::Unimplemented(format!("opcode 0f {b:02x}"))),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(code: &[u8], setup: impl FnOnce(&mut Cpu)) -> Cpu {
        let mut m = Mem::new();
        let (cs, ss) = (0x1000u16, 0x2000u16);
        m.write_bytes((cs as u32) << 4, code);
        m.write_bytes(((cs as u32) << 4) + code.len() as u32, &[0xf4]);
        let mut c = Cpu::new();
        c.set_seg(&m, CS, cs);
        c.set_seg(&m, SS, ss);
        c.set_seg(&m, DS, ss);
        c.set_seg(&m, ES, ss);
        c.r[ESP] = 0x800;
        setup(&mut c);
        loop {
            match c.step(&mut m) {
                Ok(()) => {}
                Err(Exit::Halt) => return c,
                Err(e) => panic!("{e:?}"),
            }
        }
    }

    #[test]
    fn add_sets_carry_and_overflow() {
        // mov ax,0x7fff ; add ax,1
        let c = run(&[0xb8, 0xff, 0x7f, 0x05, 0x01, 0x00], |_| {});
        assert_eq!(c.ax(), 0x8000);
        assert!(c.fl & OF != 0 && c.fl & CF == 0 && c.fl & SF != 0);
        // mov ax,0xffff ; add ax,1
        let c = run(&[0xb8, 0xff, 0xff, 0x05, 0x01, 0x00], |_| {});
        assert_eq!(c.ax(), 0);
        assert!(c.fl & CF != 0 && c.fl & ZF != 0 && c.fl & OF == 0);
    }

    #[test]
    fn sbb_borrow_chain_32bit_compare() {
        // dx:ax = 0x0001_0000 - 1 via sub/sbb
        let c = run(&[0xb8, 0x00, 0x00, 0xba, 0x01, 0x00, 0x2d, 0x01, 0x00, 0x83, 0xda, 0x00], |_| {});
        assert_eq!((c.reg16(EDX), c.ax()), (0x0000, 0xffff));
    }

    #[test]
    fn operand_size_prefix_movsx_and_cdq() {
        // mov ax,-2 ; 66 0f bf c0 (movsx eax,ax) ; 66 99 (cdq)
        let c = run(&[0xb8, 0xfe, 0xff, 0x66, 0x0f, 0xbf, 0xc0, 0x66, 0x99], |_| {});
        assert_eq!(c.r[EAX], 0xffff_fffe);
        assert_eq!(c.r[EDX], 0xffff_ffff);
    }

    #[test]
    fn idiv_and_mul_widths() {
        // mov ax,-7 ; cwd ; mov cx,2 ; idiv cx  => ax=-3 dx=-1
        let c = run(&[0xb8, 0xf9, 0xff, 0x99, 0xb9, 0x02, 0x00, 0xf7, 0xf9], |_| {});
        assert_eq!(c.ax() as i16, -3);
        assert_eq!(c.reg16(EDX) as i16, -1);
        // mov ax,300 ; mov cx,300 ; mul cx => dx:ax = 90000
        let c = run(&[0xb8, 0x2c, 0x01, 0xb9, 0x2c, 0x01, 0xf7, 0xe1], |_| {});
        assert_eq!((c.reg16(EDX) as u32) << 16 | c.ax() as u32, 90000);
    }

    #[test]
    fn enter_leave_and_near_call_ret() {
        // call +4 ; hlt-pad ; [sub] enter 4,0 ; mov word [bp-2],0x1234 ; mov ax,[bp-2] ; leave ; ret
        let code = [
            0xe8, 0x01, 0x00, // call sub (skip 1 byte)
            0xf4,             // hlt (return lands here)
            0xc8, 0x04, 0x00, 0x00, // enter 4,0
            0xc7, 0x46, 0xfe, 0x34, 0x12, // mov word [bp-2],0x1234
            0x8b, 0x46, 0xfe, // mov ax,[bp-2]
            0xc9, 0xc3,
        ];
        let c = run(&code, |_| {});
        assert_eq!(c.ax(), 0x1234);
        assert_eq!(c.r[ESP] as u16, 0x800);
    }

    #[test]
    fn rep_movsb_and_repne_scasb() {
        let mut m = Mem::new();
        let (cs, ds) = (0x1000u16, 0x2000u16);
        m.write_bytes(((ds as u32) << 4), b"hello\0");
        // mov cx,6; xor si,si; mov di,0x10; rep movsb; mov di,0x10; mov cx,0xff; xor al,al; repne scasb; hlt
        let code = [0xb9, 6, 0, 0x31, 0xf6, 0xbf, 0x10, 0, 0xf3, 0xa4, 0xbf, 0x10, 0, 0xb9, 0xff, 0, 0x30, 0xc0, 0xf2, 0xae, 0xf4];
        m.write_bytes(((cs as u32) << 4), &code);
        let mut c = Cpu::new();
        for (i, s) in [(CS, cs), (DS, ds), (ES, ds), (SS, ds)] {
            c.set_seg(&m, i, s);
        }
        c.r[ESP] = 0x80;
        while c.step(&mut m).is_ok() {}
        assert_eq!(m.bytes(((ds as u32) << 4) + 0x10, 6), b"hello\0");
        assert_eq!(c.reg16(EDI), 0x16);
        assert_eq!(c.reg16(ECX), 0xff - 6);
    }

    #[test]
    fn shifts_and_rotates() {
        // mov ax,0x8001 ; rol ax,1 => 0x0003 CF=1
        let c = run(&[0xb8, 0x01, 0x80, 0xd1, 0xc0], |_| {});
        assert_eq!(c.ax(), 0x0003);
        assert!(c.fl & CF != 0);
        // mov ax,0x8000 ; sar ax,3 => 0xf000
        let c = run(&[0xb8, 0x00, 0x80, 0xc1, 0xf8, 0x03], |_| {});
        assert_eq!(c.ax(), 0xf000);
        // mov ax,1; mov dx,0x8000; shld dx,ax,1 => dx=0x0000 CF=1
        let c = run(&[0xb8, 1, 0, 0xba, 0, 0x80, 0x0f, 0xa4, 0xc2, 1], |_| {});
        assert_eq!(c.reg16(EDX), 0x0000);
        assert!(c.fl & CF != 0);
    }
}

impl Cpu {
    pub fn save(&self, w: &mut crate::ser::W) {
        for v in self.r { w.u32(v) }
        w.u32(self.ip);
        for v in self.s { w.u16(v) }
        for v in self.sb { w.u32(v) }
        w.u32(self.fl);
        w.u32(self.op_ip);
        w.u64(self.steps);
        w.bool(self.int_shadow);
    }
    pub fn load(r: &mut crate::ser::R) -> Result<Cpu, String> {
        let mut c = Cpu::new();
        for i in 0..8 { c.r[i] = r.u32()? }
        c.ip = r.u32()?;
        for i in 0..6 { c.s[i] = r.u16()? }
        for i in 0..6 { c.sb[i] = r.u32()? }
        c.fl = r.u32()?;
        c.op_ip = r.u32()?;
        c.steps = r.u64()?;
        c.int_shadow = r.bool()?;
        Ok(c)
    }
}
