//! YM3812 (OPL2 / AdLib, ports 388h/389h).
//!
//! `Opl` is the chip as the bus sees it: the address latch, the timers the drivers
//! poll during detection, and a queue of register writes. `Synth` turns the register
//! file into samples on the audio thread; it is a floating-point model of the chip
//! (phase generator, envelope generator, the four waveforms, FM/additive connection,
//! feedback, vibrato/tremolo, key scaling, rhythm mode) rather than a bit-exact one.

use crate::IPS_PER_MS;
use std::f32::consts::TAU;
use std::sync::{Arc, Mutex};

pub type Sink = Arc<Mutex<Vec<(u8, u8)>>>;

#[derive(Clone)]
pub struct Opl {
    index: u8,
    t1: u8,
    t2: u8,
    /// Start time (instruction count) of each running timer.
    t1_start: Option<u64>,
    t2_start: Option<u64>,
    flags: u8,
    mask: u8,
    /// Register writes for the synthesizer, when audio is on.
    pub sink: Option<Sink>,
    /// Number of data writes (for diagnostics and tests).
    pub writes: u64,
    /// Last value written to every register, so a loaded save state can bring the
    /// synthesizer (which lives outside the machine) back in step.
    pub regs: [u8; 256],
}

impl Opl {
    pub fn new() -> Opl {
        Opl { index: 0, t1: 0, t2: 0, t1_start: None, t2_start: None, flags: 0, mask: 0, sink: None, writes: 0, regs: [0; 256] }
    }

    fn update_timers(&mut self, now: u64) {
        let us = |n: u64| n * IPS_PER_MS / 1000;
        if let Some(s) = self.t1_start {
            if now - s >= us((256 - self.t1 as u64) * 80) && self.mask & 0x40 == 0 {
                self.flags |= 0x40;
            }
        }
        if let Some(s) = self.t2_start {
            if now - s >= us((256 - self.t2 as u64) * 320) && self.mask & 0x20 == 0 {
                self.flags |= 0x20;
            }
        }
    }

    pub fn status(&mut self, now: u64) -> u8 {
        self.update_timers(now);
        let f = self.flags & 0x60;
        // Bits 1-2 read as 1 on a real OPL2 (0x06 idle), which is how drivers tell it from OPL3.
        (if f != 0 { 0x80 | f } else { 0 }) | 0x06
    }

    pub fn write(&mut self, data: bool, v: u8, now: u64) {
        if !data {
            self.index = v;
            return;
        }
        self.writes += 1;
        self.regs[self.index as usize] = v;
        match self.index {
            0x02 => self.t1 = v,
            0x03 => self.t2 = v,
            0x04 => {
                self.update_timers(now);
                if v & 0x80 != 0 {
                    self.flags = 0;
                    return;
                }
                self.mask = v & 0x60;
                self.flags &= !self.mask;
                self.t1_start = (v & 1 != 0).then(|| self.t1_start.unwrap_or(now));
                self.t2_start = (v & 2 != 0).then(|| self.t2_start.unwrap_or(now));
                return;
            }
            _ => {}
        }
        if let Some(s) = &self.sink {
            s.lock().unwrap().push((self.index, v));
        }
    }
}

const MULT: [f32; 16] = [0.5, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 10.0, 12.0, 12.0, 15.0, 15.0];
/// Key-scale level for block 7, in dB, indexed by the top 4 bits of F-number.
const KSL_DB: [f32; 16] = [0.0, 9.0, 12.0, 13.875, 15.0, 16.125, 16.875, 17.625, 18.0, 18.75, 19.125, 19.5, 19.875, 20.25, 20.625, 21.0];
/// Attack time (ms) and 0→96 dB decay time (ms) at rate 1 (each rate step halves it).
const ATTACK_MS_R1: f32 = 2826.24;
const DECAY_MS_R1: f32 = 39280.64;
const CHIP_RATE: f32 = 49716.0;
/// Register offset of each channel's two operators.
const OP_OFF: [[usize; 2]; 9] = [[0, 3], [1, 4], [2, 5], [8, 11], [9, 12], [10, 13], [16, 19], [17, 20], [18, 21]];

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Off,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy)]
struct Op {
    phase: f32,
    /// Attenuation in dB (0 = full volume, 96 = silent).
    env: f32,
    stage: Stage,
    out: [f32; 2],
}

impl Op {
    fn new() -> Op {
        Op { phase: 0.0, env: 96.0, stage: Stage::Off, out: [0.0; 2] }
    }
}

pub struct Synth {
    regs: [u8; 256],
    ops: [Op; 18],
    rate: f32,
    lfo_t: f32,
    noise: u32,
}

fn wave(w: u8, ph: f32) -> f32 {
    let ph = ph - ph.floor();
    let s = (ph * TAU).sin();
    match w & 3 {
        0 => s,
        1 => s.max(0.0),
        2 => s.abs(),
        _ => if (ph * 4.0) as u32 & 1 == 0 { s.abs() } else { 0.0 },
    }
}

impl Synth {
    pub fn new(rate: u32) -> Synth {
        Synth { regs: [0; 256], ops: [Op::new(); 18], rate: rate as f32, lfo_t: 0.0, noise: 1 }
    }

    pub fn write(&mut self, r: u8, v: u8) {
        let old = self.regs[r as usize];
        self.regs[r as usize] = v;
        match r {
            0xb0..=0xb8 => {
                let ch = (r - 0xb0) as usize;
                if (v ^ old) & 0x20 != 0 {
                    self.key(ch, 0, v & 0x20 != 0);
                    self.key(ch, 1, v & 0x20 != 0);
                }
            }
            0xbd => {
                // Rhythm key-ons: BD (ch6 both ops), SD (ch7 op1), TOM (ch8 op0),
                // CYM (ch8 op1), HH (ch7 op0).
                if v & 0x20 != 0 {
                    let on = |b: u8| v & (1 << b) != 0;
                    let was = |b: u8| old & 0x20 != 0 && old & (1 << b) != 0;
                    for (b, ch, o) in [(4u8, 6, 0), (4, 6, 1), (3, 7, 1), (2, 8, 0), (1, 8, 1), (0, 7, 0)] {
                        if on(b) != was(b) {
                            self.key(ch, o, on(b));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn slot(ch: usize, o: usize) -> usize {
        ch * 2 + o
    }

    fn key(&mut self, ch: usize, o: usize, on: bool) {
        let op = &mut self.ops[Self::slot(ch, o)];
        if on {
            op.phase = 0.0;
            op.stage = Stage::Attack;
        } else if op.stage != Stage::Off {
            op.stage = Stage::Release;
        }
    }

    fn fnum_block(&self, ch: usize) -> (u32, u32) {
        let lo = self.regs[0xa0 + ch] as u32;
        let hi = self.regs[0xb0 + ch] as u32;
        (lo | (hi & 3) << 8, (hi >> 2) & 7)
    }

    /// Effective envelope rate (0-63) for a 4-bit rate register value.
    fn eff_rate(&self, ch: usize, reg20: u8, r: u8) -> u32 {
        if r == 0 {
            return 0;
        }
        let (f, b) = self.fnum_block(ch);
        let nts = self.regs[8] & 0x40 != 0;
        let kc = b * 2 + if nts { (f >> 8) & 1 } else { (f >> 9) & 1 };
        let rof = if reg20 & 0x10 != 0 { kc } else { kc >> 2 };
        (r as u32 * 4 + rof).min(63)
    }

    /// Duration in ms of a full sweep at this effective rate.
    fn rate_ms(base: f32, eff: u32) -> f32 {
        let steps = eff >> 2;
        let frac = 1.0 + (eff & 3) as f32 / 4.0;
        base / ((1u32 << (steps.max(1) - 1)) as f32 * frac)
    }

    fn step_env(&mut self, ch: usize, o: usize, dt_ms: f32) {
        let off = OP_OFF[ch][o];
        let r20 = self.regs[0x20 + off];
        let ad = self.regs[0x60 + off];
        let sr = self.regs[0x80 + off];
        let sl = match sr >> 4 { 15 => 93.0, s => s as f32 * 3.0 };
        let (ar, dr, rr) = (ad >> 4, ad & 15, sr & 15);
        let i = Self::slot(ch, o);
        let stage = self.ops[i].stage;
        let mut env = self.ops[i].env;
        let decay = |s: &Synth, r: u8, env: &mut f32| {
            let eff = s.eff_rate(ch, r20, r);
            if eff >= 4 {
                *env += 96.0 * dt_ms / Self::rate_ms(DECAY_MS_R1, eff);
            }
        };
        let mut next = stage;
        match stage {
            Stage::Off => env = 96.0,
            Stage::Attack => {
                let eff = self.eff_rate(ch, r20, ar);
                if eff >= 60 {
                    env = 0.0;
                } else if eff >= 4 {
                    env -= 96.0 * dt_ms / Self::rate_ms(ATTACK_MS_R1, eff);
                }
                if env <= 0.0 {
                    env = 0.0;
                    next = Stage::Decay;
                }
            }
            Stage::Decay => {
                decay(self, dr, &mut env);
                if env >= sl {
                    env = sl;
                    next = Stage::Sustain;
                }
            }
            Stage::Sustain => {
                // EG-type 0: no sustain hold, keep falling at the release rate.
                if r20 & 0x20 == 0 {
                    decay(self, rr, &mut env);
                }
            }
            Stage::Release => decay(self, rr, &mut env),
        }
        if env >= 96.0 && matches!(next, Stage::Release | Stage::Sustain) {
            env = 96.0;
            next = Stage::Off;
        }
        self.ops[i].env = env;
        self.ops[i].stage = next;
    }

    /// One operator sample given phase modulation `pm` (in cycles).
    fn op_out(&mut self, ch: usize, o: usize, pm: f32, trem: f32, vib: f32, dt: f32) -> f32 {
        let off = OP_OFF[ch][o];
        let r20 = self.regs[0x20 + off];
        let r40 = self.regs[0x40 + off];
        let (f, b) = self.fnum_block(ch);
        let mut freq = f as f32 * CHIP_RATE / (1u32 << (20 - b)) as f32 * MULT[(r20 & 15) as usize];
        if r20 & 0x40 != 0 {
            freq *= vib;
        }
        let i = Self::slot(ch, o);
        let ksl = (KSL_DB[(f >> 6) as usize] - 3.0 * (7 - b) as f32).max(0.0) * [0.0, 0.5, 0.25, 1.0][(r40 >> 6) as usize];
        let mut att = self.ops[i].env + (r40 & 63) as f32 * 0.75 + ksl;
        if r20 & 0x80 != 0 {
            att += trem;
        }
        let ws = if self.regs[1] & 0x20 != 0 { self.regs[0xe0 + off] } else { 0 };
        let op = &mut self.ops[i];
        let out = if att >= 96.0 || op.stage == Stage::Off { 0.0 } else { wave(ws, op.phase + pm) * 10f32.powf(-att / 20.0) };
        op.phase = (op.phase + freq * dt).fract();
        out
    }

    /// Mix `out.len() / 2` stereo frames (the chip is mono; both sides get the same).
    pub fn generate(&mut self, out: &mut [f32]) {
        let dt = 1.0 / self.rate;
        let dt_ms = 1000.0 * dt;
        for frame in out.chunks_mut(2) {
            self.lfo_t += dt;
            let bd = self.regs[0xbd];
            let trem_depth = if bd & 0x80 != 0 { 4.8 } else { 1.0 };
            let trem = trem_depth * 0.5 * (1.0 - (self.lfo_t * 3.7 * TAU).cos());
            let vib_cents = if bd & 0x40 != 0 { 14.0 } else { 7.0 };
            let vib = 2f32.powf(vib_cents * (self.lfo_t * 6.1 * TAU).sin() / 1200.0);
            self.noise ^= self.noise << 13;
            self.noise ^= self.noise >> 17;
            self.noise ^= self.noise << 5;
            let rhythm = bd & 0x20 != 0;
            let mut mix = 0.0;
            for ch in 0..9 {
                for o in 0..2 {
                    self.step_env(ch, o, dt_ms);
                }
                if rhythm && ch >= 6 {
                    continue;
                }
                mix += self.channel(ch, trem, vib, dt);
            }
            if rhythm {
                // Bass drum is a normal 2-op voice at double level; the other four
                // are single operators, the cymbal-like ones driven by noise.
                mix += 2.0 * self.channel(6, trem, vib, dt);
                let nz = if self.noise & 1 != 0 { 1.0 } else { -1.0 };
                let hh = self.op_out(7, 0, 0.0, trem, vib, dt);
                let sd = self.op_out(7, 1, 0.0, trem, vib, dt);
                let tom = self.op_out(8, 0, 0.0, trem, vib, dt);
                let cym = self.op_out(8, 1, 0.0, trem, vib, dt);
                mix += hh.abs() * nz + sd.abs() * nz * 0.8 + tom + cym.abs() * nz;
            }
            let s = (mix * 0.18).clamp(-1.0, 1.0);
            frame[0] = s;
            if frame.len() > 1 {
                frame[1] = s;
            }
        }
    }

    fn channel(&mut self, ch: usize, trem: f32, vib: f32, dt: f32) -> f32 {
        let c0 = self.regs[0xc0 + ch];
        let fb = (c0 >> 1) & 7;
        let m = Self::slot(ch, 0);
        let pm_fb = if fb == 0 { 0.0 } else { (self.ops[m].out[0] + self.ops[m].out[1]) * 2f32.powi(fb as i32 - 7) };
        let a = self.op_out(ch, 0, pm_fb, trem, vib, dt);
        self.ops[m].out = [a, self.ops[m].out[0]];
        if c0 & 1 == 0 {
            // FM: modulator output of ±1 shifts the carrier by ±4 cycles (as on the chip).
            self.op_out(ch, 1, a * 4.0, trem, vib, dt)
        } else {
            a + self.op_out(ch, 1, 0.0, trem, vib, dt)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_sequence_sees_timer1_expire() {
        // The classic AdLib probe: reset, read 0, start timer 1 at FF, wait >80 us, read C0.
        let mut o = Opl::new();
        let w = |o: &mut Opl, r: u8, v: u8, t: u64| {
            o.write(false, r, t);
            o.write(true, v, t);
        };
        w(&mut o, 4, 0x60, 0);
        w(&mut o, 4, 0x80, 0);
        assert_eq!(o.status(0) & 0xe0, 0);
        w(&mut o, 2, 0xff, 0);
        w(&mut o, 4, 0x21, 0);
        assert_eq!(o.status(10) & 0xe0, 0, "too early");
        let later = 100 * IPS_PER_MS / 1000; // 100 us
        assert_eq!(o.status(later) & 0xe0, 0xc0);
        w(&mut o, 4, 0x60, later);
        w(&mut o, 4, 0x80, later);
        assert_eq!(o.status(later) & 0xe0, 0);
    }

    /// A 440 Hz additive sine (F-num for block 4) must come out at 440 Hz.
    #[test]
    fn synth_plays_the_programmed_pitch() {
        let rate = 44100;
        let mut s = Synth::new(rate);
        // Channel 0, both operators: mult 1, max volume, instant attack, sustain.
        for (r, v) in [(0x20u8, 0x21u8), (0x23, 0x21), (0x40, 0x3f), (0x43, 0x00), (0x60, 0xf0), (0x63, 0xf0), (0x80, 0x00), (0x83, 0x00), (0xc0, 0x01)] {
            s.write(r, v);
        }
        // f = fnum * 49716 / 2^(20-block); block 4 → fnum = 440 * 65536 / 49716 ≈ 580.
        s.write(0xa0, (580 & 0xff) as u8);
        s.write(0xb0, 0x20 | (4 << 2) | (580 >> 8) as u8);
        let mut buf = vec![0f32; rate as usize * 2];
        s.generate(&mut buf);
        let mono: Vec<f32> = buf.iter().step_by(2).copied().collect();
        let ups = mono.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        // 580 * 49716 / 65536 = 440.0 Hz.
        assert!((438..=442).contains(&ups), "{ups} rising zero crossings in 1 s");
        assert!(mono.iter().fold(0f32, |a, &b| a.max(b)) > 0.1);
        // Key off: release at rate 0 holds, so set a fast release first.
        s.write(0x80, 0x0f);
        s.write(0x83, 0x0f);
        s.write(0xb0, (4 << 2) | (580 >> 8) as u8);
        s.generate(&mut buf);
        let tail = &buf[buf.len() - 2000..];
        assert!(tail.iter().all(|v| v.abs() < 1e-3), "note did not release");
    }

    #[test]
    fn resync_replays_registers_with_notes_last() {
        let mut o = Opl::new();
        for (r, v) in [(0x20u8, 0x21u8), (0x40, 0x10), (0xa0, 0x44), (0xb0, 0x32), (0xbd, 0x20)] {
            o.write(false, r, 0);
            o.write(true, v, 0);
        }
        let w = o.resync_writes();
        // Voice 0 keyed off first (0x32 without the key-on bit), keyed on again last.
        assert_eq!(w[0], (0xb0, 0x12));
        assert_eq!(*w.last().unwrap(), (0xb8, 0));
        assert_eq!(w[w.len() - 9], (0xb0, 0x32));
        for want in [(0x20, 0x21), (0x40, 0x10), (0xa0, 0x44), (0xbd, 0x20)] {
            assert!(w.contains(&want), "missing {want:x?}");
        }
        // Timer registers are chip state, not sound: never resent.
        assert!(!w.iter().any(|&(r, _)| (2..=4).contains(&r)));
    }
}

impl Opl {
    /// Send the whole register file to the synthesizer: every voice keyed off first,
    /// then the instrument/rhythm registers, then the notes as they were.
    pub fn resync(&self) {
        let Some(s) = &self.sink else { return };
        let mut q = s.lock().unwrap();
        q.extend(self.resync_writes());
    }

    pub fn resync_writes(&self) -> Vec<(u8, u8)> {
        let note = 0xb0..=0xb8u8;
        let mut out: Vec<(u8, u8)> = note.clone().map(|r| (r, self.regs[r as usize] & !0x20)).collect();
        out.extend((0x01..=0xf5u8).filter(|r| !note.contains(r) && !(0x02..=0x04).contains(r)).map(|r| (r, self.regs[r as usize])));
        out.extend(note.map(|r| (r, self.regs[r as usize])));
        out
    }

    pub fn save(&self, w: &mut crate::ser::W) {
        w.u8(self.index); w.u8(self.t1); w.u8(self.t2);
        w.opt_u64(self.t1_start); w.opt_u64(self.t2_start);
        w.u8(self.flags); w.u8(self.mask); w.u64(self.writes);
    }
    pub fn load(&mut self, r: &mut crate::ser::R) -> Result<(), String> {
        self.index = r.u8()?; self.t1 = r.u8()?; self.t2 = r.u8()?;
        self.t1_start = r.opt_u64()?; self.t2_start = r.opt_u64()?;
        self.flags = r.u8()?; self.mask = r.u8()?; self.writes = r.u64()?;
        Ok(())
    }
}
