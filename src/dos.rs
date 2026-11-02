//! DOS 5.0 kernel services in Rust: the MCB memory chain and PSPs live in emulated
//! memory exactly where programs expect them (they walk MCBs and read PSP fields),
//! while files map onto the game folder (drive C:'s root; every drive letter points
//! there, which is what REKO3.COM's A:/B: program paths need).

use crate::cpu::{Cpu, CF, CS, DS, ES, IF, SS};
use crate::machine::{Machine, Stop, R};
use crate::mem::Mem;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

const DOS_SEG: u16 = 0x0080;
const LOL_OFF: u16 = 0x0026;
const INDOS_OFF: u16 = 0x0010;
const FIRST_MCB: u16 = 0x0200;
const MEM_TOP: u16 = 0xa000;

enum Kind {
    Con,
    File(File),
}

/// `name` is kept for debugging dumps.
#[allow(dead_code)]
struct Sft {
    kind: Kind,
    refs: u16,
    name: String,
}

#[derive(Clone)]
struct Proc {
    psp: u16,
    name: String,
    /// Parent CPU state at its EXEC call (None for the first program).
    parent: Option<Cpu>,
}

#[derive(Clone)]
struct Search {
    entries: Vec<(String, u8, u32)>,
    next: usize,
}

pub struct Dos {
    root: PathBuf,
    /// Current directory below the root, DOS style without leading '\' (e.g. "SAVE").
    cwd: String,
    pub psp: u16,
    dta: u32,
    sft: Vec<Option<Sft>>,
    procs: Vec<Proc>,
    last_ret: u16,
    last_err: u16,
    searches: std::collections::HashMap<u32, Search>,
    out_line: Vec<u8>,
}

fn rd16(m: &Mem, a: u32) -> u16 {
    m.rd16(a)
}

fn lin(seg: u16, off: u16) -> u32 {
    ((seg as u32) << 4) + off as u32
}

/// 8.3 name match with '?' and '*' (DOS FCB semantics).
fn wild_match(pat: &str, name: &str) -> bool {
    fn fcb(s: &str) -> [u8; 11] {
        let mut out = [b' '; 11];
        let (n, e) = match s.rfind('.') {
            Some(i) if s != "." && s != ".." => (&s[..i], &s[i + 1..]),
            _ => (s, ""),
        };
        let fill = |dst: &mut [u8], src: &str| {
            let mut i = 0;
            for c in src.bytes() {
                if i >= dst.len() {
                    break;
                }
                if c == b'*' {
                    while i < dst.len() {
                        dst[i] = b'?';
                        i += 1;
                    }
                    break;
                }
                dst[i] = c.to_ascii_uppercase();
                i += 1;
            }
        };
        fill(&mut out[..8], n);
        fill(&mut out[8..], e);
        out
    }
    let (p, n) = (fcb(pat), fcb(name));
    p.iter().zip(n.iter()).all(|(a, b)| *a == b'?' || a == b)
}

fn is_83(name: &str) -> bool {
    let (n, e) = match name.rfind('.') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, ""),
    };
    !n.is_empty() && n.len() <= 8 && e.len() <= 3 && !e.contains('.') && name.is_ascii() && !name.contains(' ')
}

impl Clone for Dos {
    fn clone(&self) -> Dos {
        Dos {
            root: self.root.clone(),
            cwd: self.cwd.clone(),
            psp: self.psp,
            dta: self.dta,
            // Open files are opened again by name (at the same position) rather
            // than duplicated: WASI (the web build) cannot duplicate a handle.
            sft: self.sft.iter().map(|s| s.as_ref().map(|s| Sft {
                kind: match &s.kind {
                    Kind::Con => Kind::Con,
                    Kind::File(f) => {
                        let pos = (&*f).stream_position().unwrap_or(0);
                        Kind::File(self.reopen(&s.name, pos).unwrap_or_else(|e| panic!("snapshot: {e}")))
                    }
                },
                refs: s.refs,
                name: s.name.clone(),
            })).collect(),
            procs: self.procs.clone(),
            last_ret: self.last_ret,
            last_err: self.last_err,
            searches: self.searches.clone(),
            out_line: self.out_line.clone(),
        }
    }
}

impl Dos {
    /// Current position of every open file (duplicated handles share it, so a
    /// snapshot records positions and restores them).
    pub fn file_positions(&mut self) -> Vec<(usize, u64)> {
        let mut out = Vec::new();
        for (i, s) in self.sft.iter_mut().enumerate() {
            if let Some(Sft { kind: Kind::File(f), .. }) = s {
                out.push((i, f.stream_position().unwrap_or(0)));
            }
        }
        out
    }

    /// Open a file of an open handle again (read-write if allowed) at `pos`.
    fn reopen(&self, name: &str, pos: u64) -> Result<File, String> {
        let path = self.resolve(name.as_bytes());
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&path)
            .or_else(|_| File::open(&path)).map_err(|e| format!("reopen {}: {e}", path.display()))?;
        f.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
        Ok(f)
    }

    pub fn seek_files(&mut self, pos: &[(usize, u64)]) {
        for &(i, p) in pos {
            if let Some(Some(Sft { kind: Kind::File(f), .. })) = self.sft.get_mut(i) {
                f.seek(SeekFrom::Start(p)).ok();
            }
        }
    }

    pub fn new(root: PathBuf) -> Dos {
        let mut sft = Vec::new();
        for n in ["CON", "CON", "CON", "AUX", "PRN"] {
            sft.push(Some(Sft { kind: Kind::Con, refs: 1, name: n.into() }));
        }
        Dos {
            root,
            cwd: String::new(),
            psp: 0,
            dta: 0,
            sft,
            procs: Vec::new(),
            last_ret: 0,
            last_err: 0,
            searches: Default::default(),
            out_line: Vec::new(),
        }
    }

    pub fn init(&mut self, m: &mut Mem) {
        // List of lists: first MCB segment sits just before it.
        m.wr16(lin(DOS_SEG, LOL_OFF - 2), FIRST_MCB);
        let a = lin(FIRST_MCB, 0);
        m.ram[a as usize] = b'Z';
        m.wr16(a + 1, 0);
        m.wr16(a + 3, MEM_TOP - FIRST_MCB - 1);
        // "Device driver" placeholder so the DOS area is owned (owner 8 = system).
    }

    pub fn current_name(&self) -> &str {
        self.procs.last().map_or("-", |p| p.name.as_str())
    }

    // ---------- paths ----------
    /// Normalised DOS path (upper case, no drive, components) for a program-supplied name.
    fn dos_path(&self, raw: &[u8]) -> Vec<String> {
        let s: String = raw.iter().map(|&b| (b as char).to_ascii_uppercase()).collect();
        let s = s.trim().replace('/', "\\");
        let s = if s.len() >= 2 && s.as_bytes()[1] == b':' { s[2..].to_string() } else { s };
        let mut parts: Vec<String> = if s.starts_with('\\') {
            Vec::new()
        } else {
            self.cwd.split('\\').filter(|p| !p.is_empty()).map(String::from).collect()
        };
        for p in s.split('\\') {
            match p {
                "" | "." => {}
                ".." => { parts.pop(); }
                p => parts.push(p.to_string()),
            }
        }
        parts
    }

    /// Host path, matching each existing component case-insensitively.
    fn host_path(&self, parts: &[String]) -> PathBuf {
        let mut p = self.root.clone();
        for c in parts {
            let found = std::fs::read_dir(&p).ok().and_then(|rd| {
                rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).find(|n| n.eq_ignore_ascii_case(c))
            });
            p.push(found.unwrap_or_else(|| c.clone()));
        }
        p
    }

    pub fn resolve(&self, raw: &[u8]) -> PathBuf {
        self.host_path(&self.dos_path(raw))
    }

    // ---------- handles ----------
    fn jft(&self, h: u16) -> u32 {
        lin(self.psp, 0x18) + h as u32
    }

    fn sft_of(&self, m: &Mem, h: u16) -> Option<usize> {
        if h >= 20 {
            return None;
        }
        let i = m.ram[self.jft(h) as usize];
        (i != 0xff && self.sft.get(i as usize).map_or(false, |s| s.is_some())).then_some(i as usize)
    }

    fn new_handle(&mut self, m: &mut Mem, s: Sft) -> Option<u16> {
        let h = (0..20u16).find(|&h| m.ram[self.jft(h) as usize] == 0xff)?;
        let i = match self.sft.iter().position(|e| e.is_none()) {
            Some(i) => { self.sft[i] = Some(s); i }
            None => { self.sft.push(Some(s)); self.sft.len() - 1 }
        };
        m.ram[self.jft(h) as usize] = i as u8;
        Some(h)
    }

    fn close(&mut self, m: &mut Mem, h: u16) -> bool {
        let Some(i) = self.sft_of(m, h) else { return false };
        m.ram[self.jft(h) as usize] = 0xff;
        let s = self.sft[i].as_mut().unwrap();
        s.refs -= 1;
        if s.refs == 0 {
            self.sft[i] = None;
        }
        true
    }

    fn con_out(&mut self, b: &[u8]) {
        for &c in b {
            if c == b'\n' || self.out_line.len() > 200 {
                let line = std::mem::take(&mut self.out_line);
                eprintln!("[con] {}", String::from_utf8_lossy(&line).trim_end());
            } else if c != b'\r' {
                self.out_line.push(c);
            }
        }
    }

    // ---------- memory control blocks ----------
    fn mcb_kind(m: &Mem, s: u16) -> u8 { m.ram[lin(s, 0) as usize] }
    fn mcb_owner(m: &Mem, s: u16) -> u16 { rd16(m, lin(s, 1)) }
    fn mcb_size(m: &Mem, s: u16) -> u16 { rd16(m, lin(s, 3)) }

    /// Merge free blocks following `s` into it.
    fn coalesce(m: &mut Mem, s: u16) {
        while Self::mcb_kind(m, s) == b'M' {
            let n = s + 1 + Self::mcb_size(m, s);
            if Self::mcb_owner(m, n) != 0 {
                break;
            }
            let kind = Self::mcb_kind(m, n);
            let size = Self::mcb_size(m, s) + 1 + Self::mcb_size(m, n);
            m.ram[lin(s, 0) as usize] = kind;
            m.wr16(lin(s, 3), size);
        }
    }

    fn split(m: &mut Mem, s: u16, paras: u16) {
        let size = Self::mcb_size(m, s);
        if size > paras {
            let n = s + 1 + paras;
            let kind = Self::mcb_kind(m, s);
            m.ram[lin(n, 0) as usize] = kind;
            m.wr16(lin(n, 1), 0);
            m.wr16(lin(n, 3), size - paras - 1);
            m.ram[lin(s, 0) as usize] = b'M';
            m.wr16(lin(s, 3), paras);
        }
    }

    /// Allocate: Ok(data segment) or Err(largest free block).
    pub fn alloc(&mut self, m: &mut Mem, paras: u16, owner: u16) -> Result<u16, u16> {
        let mut s = FIRST_MCB;
        let mut best = 0;
        loop {
            if Self::mcb_owner(m, s) == 0 {
                Self::coalesce(m, s);
                let size = Self::mcb_size(m, s);
                if size >= paras {
                    Self::split(m, s, paras);
                    m.wr16(lin(s, 1), owner);
                    return Ok(s + 1);
                }
                best = best.max(size);
            }
            if Self::mcb_kind(m, s) != b'M' {
                return Err(best);
            }
            s += 1 + Self::mcb_size(m, s);
        }
    }

    fn largest(&mut self, m: &mut Mem) -> u16 {
        self.alloc(m, 0xffff, 0).err().unwrap_or(0)
    }

    fn valid_block(m: &Mem, seg: u16) -> bool {
        seg > FIRST_MCB && matches!(Self::mcb_kind(m, seg - 1), b'M' | b'Z')
    }

    pub fn free(&mut self, m: &mut Mem, seg: u16) -> bool {
        if !Self::valid_block(m, seg) {
            return false;
        }
        m.wr16(lin(seg - 1, 1), 0);
        true
    }

    pub fn resize(&mut self, m: &mut Mem, seg: u16, paras: u16) -> Result<(), u16> {
        if !Self::valid_block(m, seg) {
            return Err(0);
        }
        let s = seg - 1;
        let owner = Self::mcb_owner(m, s);
        // Temporarily mark free so coalescing absorbs following free blocks.
        m.wr16(lin(s, 1), 0);
        Self::coalesce(m, s);
        m.wr16(lin(s, 1), owner);
        let size = Self::mcb_size(m, s);
        if size < paras {
            return Err(size);
        }
        Self::split(m, s, paras);
        Ok(())
    }

    fn set_owner(m: &mut Mem, seg: u16, owner: u16, name: &str) {
        m.wr16(lin(seg - 1, 1), owner);
        let mut n = [0u8; 8];
        for (i, c) in name.bytes().take(8).enumerate() {
            n[i] = c;
        }
        m.write_bytes(lin(seg - 1, 8), &n);
    }

    fn free_owned(&mut self, m: &mut Mem, psp: u16) {
        let mut s = FIRST_MCB;
        loop {
            if Self::mcb_owner(m, s) == psp {
                m.wr16(lin(s, 1), 0);
            }
            if Self::mcb_kind(m, s) != b'M' {
                break;
            }
            s += 1 + Self::mcb_size(m, s);
        }
    }
}

// ---------- program loading ----------

struct Loaded {
    psp: u16,
    cs: u16,
    ip: u16,
    ss: u16,
    sp: u16,
}

fn base_name(path: &[u8]) -> String {
    let s = String::from_utf8_lossy(path).to_ascii_uppercase();
    let f = s.rsplit(['\\', '/', ':']).next().unwrap_or("").to_string();
    f.split('.').next().unwrap_or("").to_string()
}

/// Load a .COM/.EXE into a fresh PSP. `env` = environment segment to copy (0 = none).
fn load(m: &mut Machine, path: &[u8], env_src: u16, tail: &[u8], parent_psp: u16) -> Result<Loaded, u16> {
    let host = m.dos.resolve(path);
    let img = std::fs::read(&host).map_err(|_| 2u16)?;
    let name = base_name(path);
    let parts = m.dos.dos_path(path);
    let full = format!("C:\\{}", parts.join("\\"));
    // Environment block: parent's strings + count word + full program path.
    let mut env: Vec<u8> = Vec::new();
    if env_src != 0 {
        let a = lin(env_src, 0) as usize;
        let mut i = 0;
        while i < 0x8000 && !(m.mem.ram[a + i] == 0 && m.mem.ram[a + i + 1] == 0) {
            i += 1;
        }
        env.extend_from_slice(&m.mem.ram[a..a + i + 1]);
        env.push(0);
    } else {
        env.extend_from_slice(b"COMSPEC=C:\\COMMAND.COM\0PATH=C:\\\0\0");
    }
    env.extend_from_slice(&[1, 0]);
    env.extend_from_slice(full.as_bytes());
    env.push(0);
    let env_paras = ((env.len() + 15) / 16) as u16;
    let env_seg = m.dos.alloc(&mut m.mem, env_paras, 8).map_err(|_| 8u16)?;
    m.mem.write_bytes(lin(env_seg, 0), &env);

    let is_exe = img.len() >= 0x1c && (&img[..2] == b"MZ" || &img[..2] == b"ZM");
    let largest = m.dos.largest(&mut m.mem);
    let (psp, l) = if is_exe {
        let w = |o: usize| u16::from_le_bytes([img[o], img[o + 1]]);
        let (cblp, cp, crlc, cparhdr, minalloc, maxalloc) = (w(2), w(4), w(6), w(8), w(0x0a), w(0x0c));
        let mut size = cp as usize * 512;
        if cblp != 0 {
            size -= 512 - cblp as usize;
        }
        let hdr = cparhdr as usize * 16;
        let body = &img[hdr.min(img.len())..size.min(img.len())];
        let load_paras = ((body.len() + 15) / 16) as u16;
        let need = load_paras + 0x10 + minalloc;
        if largest < need {
            m.dos.free(&mut m.mem, env_seg);
            return Err(8);
        }
        let want = (load_paras as u32 + 0x10 + maxalloc as u32).min(largest as u32) as u16;
        let psp = m.dos.alloc(&mut m.mem, want.max(need), 0).map_err(|_| 8u16)?;
        let load_seg = psp + 0x10;
        m.mem.write_bytes(lin(load_seg, 0), body);
        let rel = w(0x18) as usize;
        for i in 0..crlc as usize {
            let o = rel + i * 4;
            let (off, seg) = (w(o), w(o + 2));
            let a = lin(load_seg.wrapping_add(seg), off);
            let v = m.mem.rd16(a).wrapping_add(load_seg);
            m.mem.wr16(a, v);
        }
        crate::saveui::patch(m, body, load_seg);
        (psp, Loaded { psp, cs: load_seg.wrapping_add(w(0x16)), ip: w(0x14), ss: load_seg.wrapping_add(w(0x0e)), sp: w(0x10) })
    } else {
        if largest < 0x1000.min(((img.len() + 0x100 + 15) / 16) as u16 + 0x10) {
            m.dos.free(&mut m.mem, env_seg);
            return Err(8);
        }
        let psp = m.dos.alloc(&mut m.mem, largest, 0).map_err(|_| 8u16)?;
        m.mem.write_bytes(lin(psp, 0x100), &img);
        m.mem.wr16(lin(psp, 0xfffe), 0);
        (psp, Loaded { psp, cs: psp, ip: 0x100, ss: psp, sp: 0xfffe })
    };
    Dos::set_owner(&mut m.mem, psp, psp, &name);
    Dos::set_owner(&mut m.mem, env_seg, psp, "");
    // PSP.
    let p = lin(psp, 0);
    m.mem.write_bytes(p, &[0u8; 0x100]);
    m.mem.write_bytes(p, &[0xcd, 0x20]);
    let top = psp + Dos::mcb_size(&m.mem, psp - 1);
    m.mem.wr16(p + 2, top);
    m.mem.write_bytes(p + 5, &[0x9a, 0xf0, 0xfe, 0x1d, 0xf0]);
    for (i, v) in [0x22u8, 0x23, 0x24].iter().enumerate() {
        let (seg, off) = m.vector(*v);
        m.mem.wr16(p + 0x0a + i as u32 * 4, off);
        m.mem.wr16(p + 0x0c + i as u32 * 4, seg);
    }
    m.mem.wr16(p + 0x16, if parent_psp == 0 { psp } else { parent_psp });
    // JFT: inherit the parent's handles (or the standard five).
    for h in 0..20u16 {
        let i = if parent_psp != 0 {
            m.mem.ram[(lin(parent_psp, 0x18) + h as u32) as usize]
        } else if h < 5 {
            h as u8
        } else {
            0xff
        };
        m.mem.ram[(p + 0x18 + h as u32) as usize] = i;
        if i != 0xff && parent_psp != 0 {
            if let Some(Some(s)) = m.dos.sft.get_mut(i as usize) {
                s.refs += 1;
            }
        }
    }
    m.mem.wr16(p + 0x2c, env_seg);
    m.mem.wr16(p + 0x32, 20);
    m.mem.wr16(p + 0x34, 0x18);
    m.mem.wr16(p + 0x36, psp);
    m.mem.write_bytes(p + 0x50, &[0xcd, 0x21, 0xcb]);
    let n = tail.len().min(126);
    m.mem.ram[(p + 0x80) as usize] = n as u8;
    m.mem.write_bytes(p + 0x81, &tail[..n]);
    m.mem.ram[(p + 0x81 + n as u32) as usize] = 0x0d;
    m.dos.procs.push(Proc { psp, name, parent: None });
    Ok(l)
}

fn enter(m: &mut Machine, l: &Loaded) {
    m.dos.psp = l.psp;
    m.dos.dta = lin(l.psp, 0x80);
    m.set_seg(DS, l.psp);
    m.set_seg(ES, l.psp);
    m.set_seg(SS, l.ss);
    m.set_seg(CS, l.cs);
    m.cpu.ip = l.ip as u32;
    m.cpu.set16(crate::cpu::ESP, l.sp);
    m.set_ax(0);
    m.set_bx(0);
    m.set_cx(0x00ff);
    m.set_dx(l.psp);
    m.set_si(l.ip);
    m.set_di(l.sp);
    m.cpu.set16(crate::cpu::EBP, 0x091c);
    m.cpu.fl = 0x0202;
}

pub fn start_program(m: &mut Machine, path: &str) -> R<()> {
    let l = load(m, path.as_bytes(), 0, b"", 0).map_err(|e| format!("cannot load {path} (DOS error {e})"))?;
    enter(m, &l);
    Ok(())
}

/// End the current program (INT 20h / 21h AH=00/4Ch/31h, INT 27h). `keep` = paragraphs
/// to stay resident.
pub fn terminate(m: &mut Machine, code: u8, keep: Option<u16>) -> R<()> {
    let Some(p) = m.dos.procs.pop() else { return Err("terminate with no program".into()) };
    let psp = p.psp;
    let a = lin(psp, 0);
    // Restore the vectors saved in the PSP.
    for (i, v) in [0x22u8, 0x23, 0x24].iter().enumerate() {
        let off = m.mem.rd16(a + 0x0a + i as u32 * 4);
        let seg = m.mem.rd16(a + 0x0c + i as u32 * 4);
        if *v != 0x22 {
            m.set_vector(*v, seg, off);
        }
    }
    match keep {
        Some(paras) => {
            m.dos.resize(&mut m.mem, psp, paras.max(6)).ok();
        }
        None => {
            for h in 0..20 {
                m.dos.close(&mut m.mem, h);
            }
            m.dos.free_owned(&mut m.mem, psp);
        }
    }
    m.dos.last_ret = code as u16 | if keep.is_some() { 0x0300 } else { 0 };
    if m.cfg.trace {
        eprintln!("[dos] {} exits with {code}{}", p.name, if keep.is_some() { " (resident)" } else { "" });
    }
    match p.parent {
        None => {
            m.stop = Some(Stop::Exit(code as i32));
        }
        Some(parent) => {
            let steps = m.cpu.steps;
            m.cpu = parent;
            m.cpu.steps = steps;
            m.cpu.fl &= !CF;
            m.dos.psp = m.mem.rd16(a + 0x16);
            m.dos.dta = lin(m.dos.psp, 0x80);
        }
    }
    Ok(())
}

fn err(m: &mut Machine, code: u16) {
    m.dos.last_err = code;
    m.set_ax(code);
    m.cpu.fl |= CF;
}

fn ok(m: &mut Machine) {
    m.cpu.fl &= !CF;
}

fn name_at(m: &Machine, seg: usize, off: u16) -> Vec<u8> {
    m.mem.cstr(m.at(seg, off))
}

/// Next key from the BIOS buffer, or None (caller re-runs the call after idling).
fn take_key(m: &mut Machine) -> Option<u16> {
    let (head, tail) = (m.mem.rd16(0x41a), m.mem.rd16(0x41c));
    if head == tail {
        return None;
    }
    let k = m.mem.rd16(0x400 + head as u32);
    m.mem.wr16(0x41a, if head + 2 >= 0x3e { 0x1e } else { head + 2 });
    Some(k)
}

fn wait_retry(m: &mut Machine) {
    m.cpu.ip = m.cpu.ip.wrapping_sub(3) & 0xffff;
    m.cpu.fl |= IF;
    if m.mem.kbd_queue.is_empty() {
        let next = m.mem.pit.next_irq0;
        if next > m.cpu.steps {
            m.cpu.steps = next;
        }
    }
}

/// DS:DX and the top of the caller's stack, for finding which code made a DOS call.
fn call_site(m: &Machine) -> String {
    let (ss, sp) = (m.cpu.s[crate::cpu::SS], m.cpu.r[crate::cpu::ESP] as u16);
    let words: Vec<String> = (0..16u16).map(|i| format!("{:04x}", m.mem.rd16(((ss as u32) << 4) + sp.wrapping_add(i * 2) as u32))).collect();
    format!("ds:dx={:04x}:{:04x} ss:sp={ss:04x}:{sp:04x} stack {}", m.cpu.s[crate::cpu::DS], m.cpu.r[crate::cpu::EDX] as u16, words.join(" "))
}

fn open_file(m: &mut Machine, raw: &[u8], mode: u8, create: bool, excl: bool) {
    let parts = m.dos.dos_path(raw);
    let name = parts.last().cloned().unwrap_or_default();
    if matches!(name.split('.').next(), Some("CON" | "NUL" | "PRN" | "AUX")) {
        let h = m.dos.new_handle(&mut m.mem, Sft { kind: Kind::Con, refs: 1, name });
        match h {
            Some(h) => { m.set_ax(h); ok(m) }
            None => err(m, 4),
        }
        return;
    }
    let path = m.dos.host_path(&parts);
    if excl && path.exists() {
        return err(m, 0x50);
    }
    let f = if create {
        crate::note_fs_write();
        File::create(&path)
    } else {
        std::fs::OpenOptions::new().read(true).write(mode & 3 != 0).open(&path)
    };
    match f {
        Ok(f) => {
            if m.cfg.trace {
                eprintln!("[dos] open {} -> {} {}", String::from_utf8_lossy(raw), path.display(), call_site(m));
            }
            match m.dos.new_handle(&mut m.mem, Sft { kind: Kind::File(f), refs: 1, name }) {
                Some(h) => { m.set_ax(h); ok(m) }
                None => err(m, 4),
            }
        }
        Err(e) => {
            let code = match e.kind() {
                std::io::ErrorKind::NotFound => if path.parent().map_or(false, |p| p.is_dir()) { 2 } else { 3 },
                _ => 5,
            };
            if m.cfg.trace {
                eprintln!("[dos] open {} failed ({code}) {}", String::from_utf8_lossy(raw), call_site(m));
            }
            err(m, code)
        }
    }
}

fn find_fill(m: &mut Machine) -> bool {
    let dta = m.dos.dta;
    let Some(s) = m.dos.searches.get_mut(&dta) else { return false };
    let Some((name, attr, size)) = s.entries.get(s.next).cloned() else {
        m.dos.searches.remove(&dta);
        return false;
    };
    s.next += 1;
    m.mem.ram[(dta + 0x15) as usize] = attr;
    m.mem.wr16(dta + 0x16, 0x4800);
    m.mem.wr16(dta + 0x18, 0x1b01);
    m.mem.wr16(dta + 0x1a, size as u16);
    m.mem.wr16(dta + 0x1c, (size >> 16) as u16);
    let mut n = [0u8; 13];
    n[..name.len()].copy_from_slice(name.as_bytes());
    m.mem.write_bytes(dta + 0x1e, &n);
    true
}

fn exec(m: &mut Machine) -> R<bool> {
    let al = m.al();
    let path = name_at(m, DS, m.dx());
    let pb = m.at(ES, m.bx());
    if m.cfg.trace {
        eprintln!("[dos] exec AL={al} {}", String::from_utf8_lossy(&path));
    }
    if al == 3 {
        // Overlay: load the image at the given segment with relocation.
        let host = m.dos.resolve(&path);
        let Ok(img) = std::fs::read(&host) else { err(m, 2); return Ok(false) };
        let (seg, reloc) = (m.mem.rd16(pb), m.mem.rd16(pb + 2));
        if &img[..2] == b"MZ" {
            let w = |o: usize| u16::from_le_bytes([img[o], img[o + 1]]);
            let hdr = w(8) as usize * 16;
            let mut size = w(4) as usize * 512;
            if w(2) != 0 {
                size -= 512 - w(2) as usize;
            }
            m.mem.write_bytes(lin(seg, 0), &img[hdr..size.min(img.len())]);
            for i in 0..w(6) as usize {
                let o = w(0x18) as usize + i * 4;
                let a = lin(seg.wrapping_add(w(o + 2)), w(o));
                let v = m.mem.rd16(a).wrapping_add(reloc);
                m.mem.wr16(a, v);
            }
        } else {
            m.mem.write_bytes(lin(seg, 0), &img);
        }
        ok(m);
        return Ok(false);
    }
    let env = m.mem.rd16(pb);
    let tail_ptr = lin(m.mem.rd16(pb + 4), m.mem.rd16(pb + 2));
    let n = m.mem.ram[tail_ptr as usize] as usize;
    let tail = m.mem.bytes(tail_ptr + 1, n.min(126)).to_vec();
    let parent_psp = m.dos.psp;
    let env_src = if env == 0 { m.mem.rd16(lin(parent_psp, 0x2c)) } else { env };
    // The parent's PSP records its stack at EXEC time.
    m.mem.wr16(lin(parent_psp, 0x2e), m.cpu.r[crate::cpu::ESP] as u16);
    m.mem.wr16(lin(parent_psp, 0x30), m.seg(SS));
    let saved = m.cpu.clone();
    match load(m, &path, env_src, &tail, parent_psp) {
        Err(e) => {
            err(m, e);
            Ok(false)
        }
        Ok(l) => {
            m.dos.procs.last_mut().unwrap().parent = Some(saved);
            if al == 1 {
                m.mem.wr16(pb + 0x0e, l.sp.wrapping_sub(2));
                m.mem.wr16(pb + 0x10, l.ss);
                m.mem.wr16(pb + 0x12, l.ip);
                m.mem.wr16(pb + 0x14, l.cs);
                m.mem.wr16(lin(l.ss, l.sp.wrapping_sub(2)), 0);
                m.dos.psp = l.psp;
                ok(m);
                return Ok(false);
            }
            enter(m, &l);
            Ok(true)
        }
    }
}

/// INT 21h. Returns true when control was transferred elsewhere (EXEC, terminate).
pub fn int21(m: &mut Machine) -> R<bool> {
    let ah = m.ah();
    match ah {
        0x00 => { terminate(m, 0, None)?; return Ok(true) }
        0x4c => { let c = m.al(); terminate(m, c, None)?; return Ok(true) }
        0x31 => { let (c, k) = (m.al(), m.dx()); terminate(m, c, Some(k))?; return Ok(true) }
        0x4b => return exec(m),
        0x01 | 0x07 | 0x08 => match take_key(m) {
            Some(k) => {
                m.set_al(k as u8);
                if ah == 0x01 { m.dos.con_out(&[k as u8]) }
            }
            None => wait_retry(m),
        },
        0x02 => { let c = m.dx() as u8; m.dos.con_out(&[c]) }
        0x06 => {
            if m.dx() as u8 == 0xff {
                match take_key(m) {
                    Some(k) => { m.set_al(k as u8); m.set_zf(false) }
                    None => { m.set_al(0); m.set_zf(true) }
                }
            } else {
                let c = m.dx() as u8;
                m.dos.con_out(&[c]);
            }
        }
        0x09 => {
            let a = m.at(DS, m.dx()) as usize;
            let s = &m.mem.ram[a..];
            let n = s.iter().position(|&b| b == b'$').unwrap_or(0).min(4096);
            let t = s[..n].to_vec();
            m.dos.con_out(&t);
        }
        0x0b => {
            let any = m.mem.rd16(0x41a) != m.mem.rd16(0x41c);
            m.set_al(if any { 0xff } else { 0 });
        }
        0x0c => {
            m.mem.wr16(0x41a, m.mem.rd16(0x41c));
            let f = m.al();
            if matches!(f, 0x01 | 0x06 | 0x07 | 0x08) {
                m.set_ah(f);
                return int21(m);
            }
        }
        0x0d => {}
        0x0e => m.set_al(26),
        0x19 => m.set_al(2),
        0x1a => m.dos.dta = m.at(DS, m.dx()),
        0x2f => {
            let d = m.dos.dta;
            m.set_seg(ES, (d >> 4) as u16);
            m.set_bx((d & 15) as u16);
        }
        0x25 => {
            let (n, seg, off) = (m.al(), m.seg(DS), m.dx());
            m.set_vector(n, seg, off);
        }
        0x35 => {
            let (seg, off) = m.vector(m.al());
            m.set_seg(ES, seg);
            m.set_bx(off);
        }
        0x2a => {
            m.set_cx(1993);
            m.set_dx(0x0801);
            m.set_al(0);
        }
        0x2c => {
            let ms = 9 * 3_600_000 + m.now_ms();
            m.set_cx(((ms / 3_600_000 % 24) << 8 | (ms / 60_000 % 60)) as u16);
            m.set_dx(((ms / 1000 % 60) << 8 | (ms / 10 % 100)) as u16);
        }
        0x2b | 0x2d => m.set_al(0),
        0x2e => {}
        0x30 => {
            m.set_ax(0x0005);
            m.set_bx(0xff00);
            m.set_cx(0);
        }
        0x33 => match m.al() {
            0x00 => m.set_dx(m.dx() & 0xff00),
            0x05 => m.set_dx((m.dx() & 0xff00) | 3),
            0x06 => { m.set_bx(0x0005); m.set_dx(0x1000) }
            _ => {}
        },
        0x34 => {
            m.set_seg(ES, DOS_SEG);
            m.set_bx(INDOS_OFF);
        }
        0x36 => {
            m.set_ax(8);
            m.set_bx(4096);
            m.set_cx(512);
            m.set_dx(8192);
        }
        0x37 => {
            if m.al() == 0 {
                m.set_dx((m.dx() & 0xff00) | b'/' as u16);
            }
            m.set_al(0);
        }
        0x38 => {
            let a = m.at(DS, m.dx());
            let mut info = [0u8; 34];
            info[2] = b'$';
            info[4] = b',';
            info[6] = b'.';
            info[8] = b'-';
            info[10] = b':';
            info[15] = 2;
            info[16] = 0;
            info[22] = b',';
            m.mem.write_bytes(a, &info);
            m.set_bx(1);
            ok(m);
        }
        0x39 | 0x3a => {
            let p = m.dos.resolve(&name_at(m, DS, m.dx()));
            let r = if ah == 0x39 { std::fs::create_dir(&p) } else { std::fs::remove_dir(&p) };
            if r.is_ok() { ok(m) } else { err(m, 5) }
        }
        0x3b => {
            let raw = name_at(m, DS, m.dx());
            let parts = m.dos.dos_path(&raw);
            if m.dos.host_path(&parts).is_dir() {
                m.dos.cwd = parts.join("\\");
                ok(m);
            } else {
                err(m, 3);
            }
        }
        0x3c => { let n = name_at(m, DS, m.dx()); open_file(m, &n, 2, true, false) }
        0x5b => { let n = name_at(m, DS, m.dx()); open_file(m, &n, 2, true, true) }
        0x3d => { let n = name_at(m, DS, m.dx()); let al = m.al(); open_file(m, &n, al, false, false) }
        0x6c => {
            let n = name_at(m, DS, m.si());
            let (mode, action) = (m.bx() as u8, m.dx());
            let exists = m.dos.resolve(&n).exists();
            if exists && action & 0x0f == 1 {
                open_file(m, &n, mode, false, false);
                m.set_cx(1);
            } else if exists && action & 0x0f == 2 {
                open_file(m, &n, mode, true, false);
                m.set_cx(3);
            } else if !exists && action & 0xf0 == 0x10 {
                open_file(m, &n, mode, true, false);
                m.set_cx(2);
            } else {
                err(m, if exists { 0x50 } else { 2 });
            }
        }
        0x3e => {
            let h = m.bx();
            if m.dos.close(&mut m.mem, h) { ok(m) } else { err(m, 6) }
        }
        0x3f => {
            let (h, n) = (m.bx(), m.cx() as usize);
            let dst = m.at(DS, m.dx());
            let Some(i) = m.dos.sft_of(&m.mem, h) else { err(m, 6); return Ok(false) };
            let mut buf = vec![0u8; n];
            let got = match &mut m.dos.sft[i].as_mut().unwrap().kind {
                Kind::Con => 0,
                Kind::File(f) => {
                    let mut t = 0;
                    while t < n {
                        match f.read(&mut buf[t..]) {
                            Ok(0) => break,
                            Ok(k) => t += k,
                            Err(_) => break,
                        }
                    }
                    t
                }
            };
            for (k, &b) in buf[..got].iter().enumerate() {
                m.mem.w8(dst + k as u32, b);
            }
            m.set_ax(got as u16);
            ok(m);
        }
        0x40 => {
            let (h, n) = (m.bx(), m.cx() as usize);
            let src = m.at(DS, m.dx());
            let Some(i) = m.dos.sft_of(&m.mem, h) else { err(m, 6); return Ok(false) };
            let buf: Vec<u8> = (0..n).map(|k| m.mem.ram[(src + k as u32) as usize]).collect();
            let r = match &mut m.dos.sft[i].as_mut().unwrap().kind {
                Kind::Con => { m.dos.con_out(&buf); Ok(n) }
                Kind::File(f) => {
                    if n == 0 {
                        let pos = f.stream_position().unwrap_or(0);
                        f.set_len(pos).map(|_| 0)
                    } else {
                        crate::note_fs_write();
                        f.write_all(&buf).map(|_| n)
                    }
                }
            };
            match r {
                Ok(k) => { m.set_ax(k as u16); ok(m) }
                Err(_) => err(m, 5),
            }
        }
        0x41 => {
            let p = m.dos.resolve(&name_at(m, DS, m.dx()));
            crate::note_fs_write();
            if std::fs::remove_file(&p).is_ok() { ok(m) } else { err(m, 2) }
        }
        0x42 => {
            let h = m.bx();
            let off = ((m.cx() as u32) << 16 | m.dx() as u32) as i32 as i64;
            let Some(i) = m.dos.sft_of(&m.mem, h) else { err(m, 6); return Ok(false) };
            let pos = match &mut m.dos.sft[i].as_mut().unwrap().kind {
                Kind::Con => Ok(0),
                Kind::File(f) => match m.cpu.r[0] as u8 {
                    0 => f.seek(SeekFrom::Start(off.max(0) as u64)),
                    1 => f.seek(SeekFrom::Current(off)),
                    _ => f.seek(SeekFrom::End(off)),
                },
            };
            match pos {
                Ok(p) => { m.set_ax(p as u16); m.set_dx((p >> 16) as u16); ok(m) }
                Err(_) => err(m, 0x19),
            }
        }
        0x43 => {
            let p = m.dos.resolve(&name_at(m, DS, m.dx()));
            if m.al() == 0 {
                match std::fs::metadata(&p) {
                    Ok(md) => { m.set_cx(if md.is_dir() { 0x10 } else { 0x20 }); ok(m) }
                    Err(_) => err(m, 2),
                }
            } else if p.exists() { ok(m) } else { err(m, 2) }
        }
        0x44 => {
            let h = m.bx();
            match m.al() {
                0x00 => {
                    let Some(i) = m.dos.sft_of(&m.mem, h) else { err(m, 6); return Ok(false) };
                    let d = match m.dos.sft[i].as_ref().unwrap().kind { Kind::Con => 0x80d3, Kind::File(_) => 0x0002 };
                    m.set_dx(d);
                    m.set_ax(d);
                    ok(m);
                }
                0x01 => ok(m),
                0x06 | 0x07 => { m.set_al(0xff); ok(m) }
                0x08 => { m.set_ax(1); ok(m) }
                0x09 => { m.set_dx(0); ok(m) }
                0x0e => { m.set_al(0); ok(m) }
                _ => err(m, 1),
            }
        }
        0x45 | 0x46 => {
            let h = m.bx();
            let Some(i) = m.dos.sft_of(&m.mem, h) else { err(m, 6); return Ok(false) };
            let nh = if ah == 0x45 {
                match (0..20u16).find(|&k| m.mem.ram[m.dos.jft(k) as usize] == 0xff) {
                    Some(k) => k,
                    None => { err(m, 4); return Ok(false) }
                }
            } else {
                let k = m.cx();
                m.dos.close(&mut m.mem, k);
                k
            };
            let a = m.dos.jft(nh) as usize;
            m.mem.ram[a] = i as u8;
            m.dos.sft[i].as_mut().unwrap().refs += 1;
            m.set_ax(nh);
            ok(m);
        }
        0x47 => {
            let a = m.at(DS, m.si());
            let mut s = m.dos.cwd.clone().into_bytes();
            s.push(0);
            m.mem.write_bytes(a, &s);
            m.set_ax(0x0100);
            ok(m);
        }
        0x48 => {
            let (paras, owner) = (m.bx(), m.dos.psp);
            let r = m.dos.alloc(&mut m.mem, paras, owner);
            if m.cfg.trace {
                eprintln!("[dos] alloc {paras:04x} paragraphs -> {r:x?}");
            }
            match r {
                Ok(s) => { m.set_ax(s); ok(m) }
                Err(big) => { m.set_bx(big); err(m, 8) }
            }
        }
        0x49 => {
            let s = m.seg(ES);
            if m.dos.free(&mut m.mem, s) { ok(m) } else { err(m, 9) }
        }
        0x4a => {
            let (s, paras) = (m.seg(ES), m.bx());
            match m.dos.resize(&mut m.mem, s, paras) {
                Ok(()) => {
                    if s == m.dos.psp {
                        m.mem.wr16(lin(s, 2), s + paras);
                    }
                    ok(m)
                }
                Err(big) => { m.set_bx(big); err(m, 8) }
            }
        }
        0x4d => {
            let r = std::mem::take(&mut m.dos.last_ret);
            m.set_ax(r);
            ok(m);
        }
        0x4e => {
            let raw = name_at(m, DS, m.dx());
            let attr = m.cx() as u8;
            let parts = m.dos.dos_path(&raw);
            let (dir, pat) = parts.split_at(parts.len().saturating_sub(1));
            let pat = pat.first().cloned().unwrap_or_else(|| "*.*".into());
            let hdir = m.dos.host_path(dir);
            let mut entries = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&hdir) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().to_ascii_uppercase();
                    let Ok(md) = e.metadata() else { continue };
                    if !is_83(&n) || !wild_match(&pat, &n) || (md.is_dir() && attr & 0x10 == 0) {
                        continue;
                    }
                    entries.push((n, if md.is_dir() { 0x10 } else { 0x20 }, md.len() as u32));
                }
            }
            entries.sort();
            let dta = m.dos.dta;
            m.dos.searches.insert(dta, Search { entries, next: 0 });
            if find_fill(m) { ok(m) } else { err(m, 0x12) }
        }
        0x4f => {
            if find_fill(m) { ok(m) } else { err(m, 0x12) }
        }
        0x50 => m.dos.psp = m.bx(),
        0x51 | 0x62 => { let p = m.dos.psp; m.set_bx(p) }
        0x52 => {
            m.set_seg(ES, DOS_SEG);
            m.set_bx(LOL_OFF);
        }
        0x54 => m.set_al(0),
        0x56 => {
            let a = m.dos.resolve(&name_at(m, DS, m.dx()));
            let b = m.dos.resolve(&name_at(m, ES, m.di()));
            crate::note_fs_write();
            if std::fs::rename(&a, &b).is_ok() { ok(m) } else { err(m, 5) }
        }
        0x57 => {
            if m.al() == 0 {
                m.set_cx(0x4800);
                m.set_dx(0x1b01);
            }
            ok(m);
        }
        0x58 => {
            if m.al() == 0 { m.set_ax(0) }
            ok(m);
        }
        0x59 => {
            let e = m.dos.last_err;
            m.set_ax(e);
            m.set_bx(0x0d01);
            m.set_cx(m.cx() & 0x00ff | 0x0200);
        }
        0x63 => m.set_al(0xff),
        0x66 => { m.set_bx(437); m.set_dx(437); ok(m) }
        0x67 | 0x68 => ok(m),
        _ => {
            m.warn_once(format!("INT 21h AH={ah:02x} not implemented (from {})", m.dos.current_name()));
            err(m, 1);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wild_match("*.R3S", "MSAVE0.R3S"));
        assert!(!wild_match("*.R3S", "MSAVE.R3"));
        assert!(wild_match("MSAVE?.R3S", "MSAVE0.R3S"));
        assert!(wild_match("*.*", "REKO3.COM"));
        assert!(wild_match("*", "CONFIG"));
    }

    #[test]
    fn mcb_alloc_free_resize() {
        let mut m = Mem::new();
        let mut d = Dos::new(PathBuf::from("."));
        d.init(&mut m);
        let total = d.largest(&mut m);
        let a = d.alloc(&mut m, 0x100, 0x50).unwrap();
        let b = d.alloc(&mut m, 0x200, 0x50).unwrap();
        assert_eq!(b, a + 0x101);
        assert_eq!(d.largest(&mut m), total - 0x302);
        assert!(d.resize(&mut m, a, 0x200).is_err(), "b is in the way");
        assert!(d.free(&mut m, b));
        assert!(d.resize(&mut m, a, 0x200).is_ok());
        assert_eq!(d.resize(&mut m, a, 0xffff), Err(total));
        assert!(d.free(&mut m, a));
        assert_eq!(d.largest(&mut m), total);
    }
}

impl Dos {
    /// Open files are saved by DOS name and position and reopened on load.
    pub fn save(&mut self, w: &mut crate::ser::W) {
        w.str(&self.cwd);
        w.u16(self.psp);
        w.u32(self.dta);
        w.u32(self.sft.len() as u32);
        for s in &mut self.sft {
            match s {
                None => w.u8(0),
                Some(Sft { kind: Kind::Con, refs, name }) => { w.u8(1); w.u16(*refs); w.str(name) }
                Some(Sft { kind: Kind::File(f), refs, name }) => {
                    w.u8(2); w.u16(*refs); w.str(name); w.u64(f.stream_position().unwrap_or(0));
                }
            }
        }
        w.u32(self.procs.len() as u32);
        for p in &self.procs {
            w.u16(p.psp); w.str(&p.name);
            match &p.parent {
                None => w.u8(0),
                Some(c) => { w.u8(1); c.save(w) }
            }
        }
        w.u16(self.last_ret);
        w.u16(self.last_err);
    }
    pub fn load(&mut self, r: &mut crate::ser::R) -> Result<(), String> {
        self.cwd = r.str()?;
        self.psp = r.u16()?;
        self.dta = r.u32()?;
        let n = r.u32()? as usize;
        let mut sft = Vec::with_capacity(n);
        for _ in 0..n {
            sft.push(match r.u8()? {
                0 => None,
                1 => { let refs = r.u16()?; let name = r.str()?; Some(Sft { kind: Kind::Con, refs, name }) }
                _ => {
                    let refs = r.u16()?; let name = r.str()?; let pos = r.u64()?;
                    let f = self.reopen(&name, pos)?;
                    Some(Sft { kind: Kind::File(f), refs, name })
                }
            });
        }
        self.sft = sft;
        let n = r.u32()? as usize;
        let mut procs = Vec::with_capacity(n);
        for _ in 0..n {
            let psp = r.u16()?; let name = r.str()?;
            let parent = if r.u8()? != 0 { Some(Cpu::load(r)?) } else { None };
            procs.push(Proc { psp, name, parent });
        }
        self.procs = procs;
        self.last_ret = r.u16()?;
        self.last_err = r.u16()?;
        self.searches.clear();
        Ok(())
    }
}
