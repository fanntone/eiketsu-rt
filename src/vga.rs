//! VGA: four 64 KiB planes behind the A0000 window, the sequencer / graphics
//! controller write and read modes with latches, attribute controller, DAC and the
//! CRTC registers that decide what part of the planes is on screen.
//!
//! The game draws in the 16-colour planar modes through the graphics controller
//! (ports 3C4/3CE), so those paths follow the hardware exactly. Chain-4 (mode 13h)
//! is stored as plane = addr & 3, offset = addr >> 2.

#[derive(Clone)]
pub struct Vga {
    pub planes: [Vec<u8>; 4],
    latch: [u8; 4],
    pub seq: [u8; 8],
    seq_i: u8,
    pub gc: [u8; 16],
    gc_i: u8,
    pub crtc: [u8; 32],
    crtc_i: u8,
    pub attr: [u8; 32],
    attr_i: u8,
    attr_flip: bool,
    pub dac: [[u8; 3]; 256],
    dac_w: u8,
    dac_r: u8,
    dac_comp: u8,
    pub dac_mask: u8,
    pub misc: u8,
    /// Text-mode shadow: true while a text mode is set (B8000 is plain RAM then).
    pub text: bool,
    /// Bumped on every change that can alter the picture.
    pub dirty: u64,
}

// Register tables for the BIOS modes the programs set (from the IBM VGA BIOS).
struct ModeRegs {
    misc: u8,
    seq: [u8; 4],
    crtc: [u8; 25],
    attr: [u8; 20],
    gc: [u8; 9],
}

const PAL16: [u8; 16] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x14, 0x07, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f];

fn attr16(mode_ctl: u8) -> [u8; 20] {
    let mut a = [0u8; 20];
    a[..16].copy_from_slice(&PAL16);
    a[16] = mode_ctl;
    a[17] = 0;
    a[18] = 0x0f;
    a[19] = 0;
    a
}

fn mode_regs(mode: u8) -> Option<ModeRegs> {
    Some(match mode {
        0x03 => ModeRegs {
            misc: 0x67,
            seq: [0x00, 0x03, 0x00, 0x02],
            crtc: [0x5f, 0x4f, 0x50, 0x82, 0x55, 0x81, 0xbf, 0x1f, 0x00, 0x4f, 0x0d, 0x0e, 0, 0, 0, 0, 0x9c, 0x8e, 0x8f, 0x28, 0x1f, 0x96, 0xb9, 0xa3, 0xff],
            attr: { let mut a = attr16(0x0c); a[..8].copy_from_slice(&[0, 1, 2, 3, 4, 5, 0x14, 7]); a[18] = 0x0f; a[19] = 8; a },
            gc: [0, 0, 0, 0, 0, 0x10, 0x0e, 0x00, 0xff],
        },
        0x0d => ModeRegs {
            misc: 0x63,
            seq: [0x09, 0x0f, 0x00, 0x06],
            crtc: [0x2d, 0x27, 0x28, 0x90, 0x2b, 0x80, 0xbf, 0x1f, 0x00, 0xc0, 0, 0, 0, 0, 0, 0, 0x9c, 0x8e, 0x8f, 0x14, 0x00, 0x96, 0xb9, 0xe3, 0xff],
            attr: attr16(0x01),
            gc: [0, 0, 0, 0, 0, 0x00, 0x05, 0x0f, 0xff],
        },
        0x0e => ModeRegs {
            misc: 0x63,
            seq: [0x01, 0x0f, 0x00, 0x06],
            crtc: [0x5f, 0x4f, 0x50, 0x82, 0x54, 0x80, 0xbf, 0x1f, 0x00, 0xc1, 0, 0, 0, 0, 0, 0, 0x9c, 0x8e, 0x8f, 0x28, 0x00, 0x96, 0xb9, 0xe3, 0xff],
            attr: attr16(0x01),
            gc: [0, 0, 0, 0, 0, 0x00, 0x05, 0x0f, 0xff],
        },
        0x10 => ModeRegs {
            misc: 0xa3,
            seq: [0x01, 0x0f, 0x00, 0x06],
            crtc: [0x5f, 0x4f, 0x50, 0x82, 0x54, 0x80, 0xbf, 0x1f, 0x00, 0x40, 0, 0, 0, 0, 0, 0, 0x83, 0x85, 0x5d, 0x28, 0x0f, 0x63, 0xba, 0xe3, 0xff],
            attr: attr16(0x01),
            gc: [0, 0, 0, 0, 0, 0x00, 0x05, 0x0f, 0xff],
        },
        0x12 => ModeRegs {
            misc: 0xe3,
            seq: [0x01, 0x0f, 0x00, 0x06],
            crtc: [0x5f, 0x4f, 0x50, 0x82, 0x54, 0x80, 0x0b, 0x3e, 0x00, 0x40, 0, 0, 0, 0, 0, 0, 0xea, 0x8c, 0xdf, 0x28, 0x00, 0xe7, 0x04, 0xe3, 0xff],
            attr: attr16(0x01),
            gc: [0, 0, 0, 0, 0, 0x00, 0x05, 0x0f, 0xff],
        },
        0x13 => ModeRegs {
            misc: 0x63,
            seq: [0x01, 0x0f, 0x00, 0x0e],
            crtc: [0x5f, 0x4f, 0x50, 0x82, 0x54, 0x80, 0xbf, 0x1f, 0x00, 0x41, 0, 0, 0, 0, 0, 0, 0x9c, 0x8e, 0x8f, 0x28, 0x40, 0x96, 0xb9, 0xa3, 0xff],
            attr: { let mut a = [0u8; 20]; for i in 0..16 { a[i] = i as u8 } a[16] = 0x41; a[18] = 0x0f; a },
            gc: [0, 0, 0, 0, 0, 0x40, 0x05, 0x0f, 0xff],
        },
        _ => return None,
    })
}

/// Default DAC: the 16 EGA colours followed by the standard 256-colour ramp is
/// overkill for this game; the EGA 64-colour table in the first 64 entries is what
/// the 16-colour modes use until a program loads its own palette.
fn default_dac() -> [[u8; 3]; 256] {
    let mut d = [[0u8; 3]; 256];
    for (i, e) in d.iter_mut().enumerate().take(64) {
        let c = |hi: usize, lo: usize| ((((i >> hi) & 1) * 2 + ((i >> lo) & 1)) * 0x15) as u8;
        *e = [c(2, 5), c(1, 4), c(0, 3)];
    }
    d
}

impl Vga {
    pub fn new() -> Vga {
        let mut v = Vga {
            planes: [vec![0; 65536], vec![0; 65536], vec![0; 65536], vec![0; 65536]],
            latch: [0; 4],
            seq: [0; 8],
            seq_i: 0,
            gc: [0; 16],
            gc_i: 0,
            crtc: [0; 32],
            crtc_i: 0,
            attr: [0; 32],
            attr_i: 0,
            attr_flip: false,
            dac: default_dac(),
            dac_w: 0,
            dac_r: 0,
            dac_comp: 0,
            dac_mask: 0xff,
            misc: 0,
            text: true,
            dirty: 0,
        };
        v.set_mode(0x03, true);
        v
    }

    /// Program the registers the way INT 10h AH=00 does. Returns false for modes we
    /// do not know.
    pub fn set_mode(&mut self, mode: u8, clear: bool) -> bool {
        let Some(r) = mode_regs(mode & 0x7f) else { return false };
        self.misc = r.misc;
        self.seq[0] = 3;
        self.seq[1..5].copy_from_slice(&r.seq);
        self.crtc[..25].copy_from_slice(&r.crtc);
        self.attr[..20].copy_from_slice(&r.attr);
        self.gc[..9].copy_from_slice(&r.gc);
        self.text = mode & 0x7f == 0x03;
        if !self.text {
            self.dac = default_dac();
            if mode & 0x7f == 0x13 {
                // VGA BIOS loads the 16 EGA colours into DAC 0-15 for mode 13h too.
                for i in 0..16 {
                    self.dac[i] = default_dac()[PAL16[i] as usize];
                }
            }
        }
        if clear && mode & 0x80 == 0 {
            for p in &mut self.planes {
                p.fill(0);
            }
        }
        self.dirty += 1;
        true
    }

    fn chain4(&self) -> bool {
        self.seq[4] & 0x08 != 0
    }

    /// Offset inside the A0000 window, or None when the address is outside the
    /// window the graphics controller currently maps.
    fn window(&self, a: u32) -> Option<u32> {
        let (base, size) = match (self.gc[6] >> 2) & 3 {
            0 => (0xa0000, 0x20000),
            1 => (0xa0000, 0x10000),
            2 => (0xb0000, 0x8000),
            _ => (0xb8000, 0x8000),
        };
        (a >= base && a < base + size).then(|| (a - base) & 0xffff)
    }

    /// True if this physical address belongs to the VGA (A0000-AFFFF in graphics modes).
    #[inline]
    pub fn claims(&self, a: u32) -> bool {
        !self.text && (0xa0000..0xb0000).contains(&a)
    }

    pub fn read(&mut self, a: u32) -> u8 {
        let Some(off) = self.window(a) else { return 0xff };
        if self.chain4() {
            return self.planes[(off & 3) as usize][(off >> 2) as usize];
        }
        let off = off as usize;
        for i in 0..4 {
            self.latch[i] = self.planes[i][off];
        }
        if self.gc[5] & 0x08 == 0 {
            self.latch[(self.gc[4] & 3) as usize]
        } else {
            // Read mode 1: colour compare.
            let mut r = 0xff;
            for i in 0..4 {
                if self.gc[7] & (1 << i) != 0 {
                    let want = if self.gc[2] & (1 << i) != 0 { 0xff } else { 0 };
                    r &= !(self.latch[i] ^ want);
                }
            }
            r
        }
    }

    pub fn write(&mut self, a: u32, v: u8) {
        let Some(off) = self.window(a) else { return };
        self.dirty += 1;
        if self.chain4() {
            let p = (off & 3) as usize;
            if self.seq[2] & (1 << p) != 0 {
                self.planes[p][(off >> 2) as usize] = v;
            }
            return;
        }
        let off = off as usize;
        let rot = self.gc[3] & 7;
        let func = (self.gc[3] >> 3) & 3;
        let mut mask = self.gc[8];
        let rv = v.rotate_right(rot as u32);
        let mode = self.gc[5] & 3;
        if mode == 3 {
            mask &= rv;
        }
        for i in 0..4 {
            if self.seq[2] & (1 << i) == 0 {
                continue;
            }
            let sr = if self.gc[0] & (1 << i) != 0 { 0xff } else { 0 };
            let val = match mode {
                0 => if self.gc[1] & (1 << i) != 0 { sr } else { rv },
                1 => {
                    self.planes[i][off] = self.latch[i];
                    continue;
                }
                2 => if v & (1 << i) != 0 { 0xff } else { 0 },
                _ => sr,
            };
            let l = self.latch[i];
            let val = match func {
                0 => val,
                1 => val & l,
                2 => val | l,
                _ => val ^ l,
            };
            self.planes[i][off] = (val & mask) | (l & !mask);
        }
    }

    pub fn inp(&mut self, port: u16, vretrace: bool, hblank: bool) -> u8 {
        match port {
            0x3c0 => self.attr_i,
            0x3c1 => self.attr[(self.attr_i & 0x1f) as usize],
            0x3c2 => 0x10,
            0x3c4 => self.seq_i,
            0x3c5 => self.seq[(self.seq_i & 7) as usize],
            0x3c6 => self.dac_mask,
            0x3c7 => 0,
            0x3c8 => self.dac_w,
            0x3c9 => {
                let v = self.dac[self.dac_r as usize][self.dac_comp as usize];
                self.dac_comp += 1;
                if self.dac_comp == 3 {
                    self.dac_comp = 0;
                    self.dac_r = self.dac_r.wrapping_add(1);
                }
                v
            }
            0x3cc => self.misc,
            0x3ce => self.gc_i,
            0x3cf => self.gc[(self.gc_i & 15) as usize],
            0x3d4 | 0x3b4 => self.crtc_i,
            0x3d5 | 0x3b5 => self.crtc[(self.crtc_i & 31) as usize],
            0x3da | 0x3ba => {
                self.attr_flip = false;
                (vretrace as u8) << 3 | (vretrace || hblank) as u8
            }
            _ => 0xff,
        }
    }

    pub fn outp(&mut self, port: u16, v: u8) {
        match port {
            0x3c0 => {
                if !self.attr_flip {
                    self.attr_i = v;
                } else {
                    let i = (self.attr_i & 0x1f) as usize;
                    if i < 0x15 {
                        self.attr[i] = v;
                        self.dirty += 1;
                    }
                }
                self.attr_flip = !self.attr_flip;
            }
            0x3c2 => self.misc = v,
            0x3c4 => self.seq_i = v,
            0x3c5 => self.seq[(self.seq_i & 7) as usize] = v,
            0x3c6 => { self.dac_mask = v; self.dirty += 1 }
            0x3c7 => { self.dac_r = v; self.dac_comp = 0 }
            0x3c8 => { self.dac_w = v; self.dac_comp = 0 }
            0x3c9 => {
                self.dac[self.dac_w as usize][self.dac_comp as usize] = v & 0x3f;
                self.dac_comp += 1;
                if self.dac_comp == 3 {
                    self.dac_comp = 0;
                    self.dac_w = self.dac_w.wrapping_add(1);
                }
                self.dirty += 1;
            }
            0x3ce => self.gc_i = v,
            0x3cf => self.gc[(self.gc_i & 15) as usize] = v,
            0x3d4 | 0x3b4 => self.crtc_i = v,
            0x3d5 | 0x3b5 => {
                self.crtc[(self.crtc_i & 31) as usize] = v;
                self.dirty += 1;
            }
            _ => {}
        }
    }

    /// Visible size in pixels from the CRTC (display-enable end, scan doubling).
    pub fn size(&self) -> (usize, usize) {
        let c = &self.crtc;
        let mut w = (c[1] as usize + 1) * 8;
        let lines = (c[0x12] as usize | ((c[7] as usize & 2) << 7) | ((c[7] as usize & 0x40) << 3)) + 1;
        let mut h = lines / ((c[9] as usize & 0x1f) + 1);
        if c[9] & 0x80 != 0 {
            h /= 2;
        }
        if self.attr[0x10] & 0x40 != 0 {
            w /= 2;
        }
        (w.clamp(8, 1024), h.clamp(8, 1024))
    }

    fn rgb(&self, i: u8) -> u32 {
        let [r, g, b] = self.dac[(i & self.dac_mask) as usize];
        let e = |c: u8| ((c as u32) << 2) | ((c as u32) >> 4);
        e(r) << 16 | e(g) << 8 | e(b)
    }

    /// DAC index for a 4-bit planar pixel, through the attribute controller.
    fn attr_index(&self, c: u8) -> u8 {
        let c = c & self.attr[0x12] & 0x0f;
        let p = self.attr[c as usize];
        let hi = if self.attr[0x10] & 0x80 != 0 {
            (p & 0x0f) | ((self.attr[0x14] & 3) << 4)
        } else {
            p & 0x3f
        };
        hi | ((self.attr[0x14] & 0x0c) << 4)
    }

    /// Render the visible screen as 0x00RRGGBB. Text mode is shown blank (only the
    /// launcher's error messages use it; those also go to stderr).
    pub fn render(&self) -> (usize, usize, Vec<u32>) {
        if self.text {
            return (640, 400, vec![0; 640 * 400]);
        }
        let (w, h) = self.size();
        let mut out = vec![0u32; w * h];
        let start = ((self.crtc[0x0c] as usize) << 8) | self.crtc[0x0d] as usize;
        let pitch = self.crtc[0x13] as usize * 2;
        if self.chain4() {
            for y in 0..h {
                for x in 0..w {
                    let off = (start + y * pitch * 4 + x) & 0x3ffff;
                    let v = self.planes[off & 3][off >> 2];
                    out[y * w + x] = self.rgb(v);
                }
            }
            return (w, h, out);
        }
        let mut lut = [0u32; 16];
        for (c, e) in lut.iter_mut().enumerate() {
            *e = self.rgb(self.attr_index(c as u8));
        }
        let pan = (self.attr[0x13] & 7) as usize;
        for y in 0..h {
            let row = start + y * pitch;
            for x in 0..w {
                let px = x + pan;
                let off = (row + px / 8) & 0xffff;
                let bit = 7 - (px & 7);
                let c = ((self.planes[0][off] >> bit) & 1)
                    | ((self.planes[1][off] >> bit) & 1) << 1
                    | ((self.planes[2][off] >> bit) & 1) << 2
                    | ((self.planes[3][off] >> bit) & 1) << 3;
                out[y * w + x] = lut[c as usize];
            }
        }
        (w, h, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gc(v: &mut Vga, i: u8, x: u8) {
        v.outp(0x3ce, i);
        v.outp(0x3cf, x);
    }

    #[test]
    fn mode12_geometry() {
        let mut v = Vga::new();
        v.set_mode(0x12, true);
        assert_eq!(v.size(), (640, 480));
        v.set_mode(0x13, true);
        assert_eq!(v.size(), (320, 200));
        v.set_mode(0x0d, true);
        assert_eq!(v.size(), (320, 200));
    }

    #[test]
    fn write_mode0_set_reset_and_bitmask() {
        let mut v = Vga::new();
        v.set_mode(0x12, true);
        // Set/reset colour 0101b on all planes, only the left nibble.
        gc(&mut v, 0, 0x05);
        gc(&mut v, 1, 0x0f);
        gc(&mut v, 8, 0xf0);
        v.read(0xa0000);
        v.write(0xa0000, 0);
        assert_eq!([v.planes[0][0], v.planes[1][0], v.planes[2][0], v.planes[3][0]], [0xf0, 0, 0xf0, 0]);
        // Read mode 1 colour compare finds the 0101b pixels.
        gc(&mut v, 2, 0x05);
        gc(&mut v, 5, 0x08);
        gc(&mut v, 7, 0x0f);
        assert_eq!(v.read(0xa0000), 0xf0);
    }

    #[test]
    fn write_mode1_copies_latches() {
        let mut v = Vga::new();
        v.set_mode(0x12, true);
        for i in 0..4 {
            v.planes[i][10] = 0x11 * (i as u8 + 1);
        }
        gc(&mut v, 5, 0x01);
        v.read(0xa000a);
        v.write(0xa0020, 0);
        for i in 0..4 {
            assert_eq!(v.planes[i][0x20], 0x11 * (i as u8 + 1));
        }
    }

    #[test]
    fn write_mode2_and_xor() {
        let mut v = Vga::new();
        v.set_mode(0x12, true);
        v.planes[0][0] = 0xff;
        gc(&mut v, 5, 0x02);
        gc(&mut v, 3, 0x18); // XOR
        gc(&mut v, 8, 0x0f);
        v.read(0xa0000);
        v.write(0xa0000, 0x03);
        assert_eq!(v.planes[0][0], 0xf0);
        assert_eq!(v.planes[1][0], 0x0f);
        assert_eq!(v.planes[2][0], 0x00);
    }

    #[test]
    fn planar_render_uses_attribute_and_dac() {
        let mut v = Vga::new();
        v.set_mode(0x12, true);
        v.planes[0][0] = 0x80; // pixel (0,0) colour 1
        v.outp(0x3c8, 1);
        for c in [63, 0, 0] {
            v.outp(0x3c9, c);
        }
        let (w, h, px) = v.render();
        assert_eq!((w, h), (640, 480));
        assert_eq!(px[0], 0xff0000);
        assert_eq!(px[1], 0);
    }
}

impl Vga {
    pub fn save(&self, w: &mut crate::ser::W) {
        for p in &self.planes { w.bytes(p) }
        w.bytes(&self.latch);
        w.bytes(&self.seq); w.u8(self.seq_i);
        w.bytes(&self.gc); w.u8(self.gc_i);
        w.bytes(&self.crtc); w.u8(self.crtc_i);
        w.bytes(&self.attr); w.u8(self.attr_i); w.bool(self.attr_flip);
        for e in &self.dac { w.bytes(e) }
        w.u8(self.dac_w); w.u8(self.dac_r); w.u8(self.dac_comp); w.u8(self.dac_mask); w.u8(self.misc);
        w.bool(self.text); w.u64(self.dirty);
    }
    pub fn load(&mut self, r: &mut crate::ser::R) -> Result<(), String> {
        for i in 0..4 { self.planes[i] = r.bytes()?.to_vec() }
        self.latch.copy_from_slice(r.bytes()?);
        self.seq.copy_from_slice(r.bytes()?); self.seq_i = r.u8()?;
        self.gc.copy_from_slice(r.bytes()?); self.gc_i = r.u8()?;
        self.crtc.copy_from_slice(r.bytes()?); self.crtc_i = r.u8()?;
        self.attr.copy_from_slice(r.bytes()?); self.attr_i = r.u8()?; self.attr_flip = r.bool()?;
        for i in 0..256 { self.dac[i].copy_from_slice(r.bytes()?) }
        self.dac_w = r.u8()?; self.dac_r = r.u8()?; self.dac_comp = r.u8()?; self.dac_mask = r.u8()?; self.misc = r.u8()?;
        self.text = r.bool()?; self.dirty = r.u64()?;
        Ok(())
    }
}
