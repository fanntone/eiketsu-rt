//! Battle state read straight from MAIN.EXE's memory (for the `state` control
//! command), so a player program gets units as text instead of reading pixels.
//!
//! Layout found by diffing memory snapshots (see README "戰場狀態"):
//! - General table: 0x2C-byte records, record g = general g. +1 Big5 name (NUL-
//!   terminated), +0x13/+0x14/+0x15 統率/武力/智力, +0x19 troops (u16), +0x1B class,
//!   +0x1C level, +0x1D EXP, +0x1E..+0x25 items (0x1E = 豆, 0xFF = empty).
//! - Unit table: 14-byte records at general table + 0x685D. -2 general index (u16;
//!   0x100+ are nameless troops such as 步兵隊, stored in the same table), +0 slot,
//!   +1 x, +2 y (0xFF = not on the field), +7 action state, +8 flags (0x40/0x80 = acted this turn), +9 side (bit 2 = ours), +0xA morale.
//! - View scroll: two u16 at general table + 0x677D / +0x677F, in half tiles.
//!   Tile (x,y) is drawn centered at screen (32 + 16·(2x − sx), 88 + 16·(2y − sy)).
//! The general table is located by 劉備's and 關羽's records, not a fixed address.

use crate::mem::Mem;

pub const GEN_SIZE: u32 = 0x2c;
const UNIT_OFF: u32 = 0x685d;
const VIEW_OFF: u32 = 0x677d;
/// Map view on screen: x 16–432, y 72–424.
pub const VIEW_PX: (i32, i32, i32, i32) = (16, 72, 432, 424);
const UNIT_SIZE: u32 = 14;
const MAX_UNITS: u32 = 64;
/// 劉備's record start: general 0, Big5 「劉備」.
const SIGNATURE: [u8; 5] = [0x00, 0xbc, 0x42, 0xb3, 0xc6];
/// The next record: general 1, 「關羽」.
const NEXT: [u8; 5] = [0x01, 0xc3, 0xf6, 0xa6, 0xd0];

pub struct Unit {
    pub general: u16,
    pub side: &'static str,
    pub acted: bool,
    /// Raw unit record bytes +3..+0xC (for diagnostics).
    pub raw: [u8; 9],
    pub name: String,
    pub x: u8,
    pub y: u8,
    pub troops: u16,
    pub level: u8,
    pub exp: u8,
    pub morale: u8,
    pub class: u8,
    pub state: u8,
    pub stats: [u8; 3],
    pub items: Vec<u8>,
}

/// Linear address of the general table, or None outside the game proper.
pub fn general_table(m: &Mem) -> Option<u32> {
    // 「劉備」 also appears in dialogue text: require 關羽's record right after it.
    let ram = &m.ram[0x10000..0xa0000];
    let gen = GEN_SIZE as usize;
    (0..ram.len() - gen - SIGNATURE.len())
        .find(|&p| ram[p..p + SIGNATURE.len()] == SIGNATURE && ram[p + gen..p + gen + SIGNATURE.len()] == NEXT)
        .map(|p| p as u32 + 0x10000)
}

/// The 鹿砦 in the middle of 汜水關 (marked on the grid overlay).
pub const LUJAI: (u8, u8) = (11, 8);

/// Lay the tile grid over a rendered frame: every visible tile outlined and
/// labelled with its map coordinates (from the view scroll in memory, the same
/// numbers clicks are computed from); our units green, enemies red, the 鹿砦
/// cyan, the selected unit magenta. Does nothing outside a battle.
pub fn draw_grid(m: &Mem, w: usize, h: usize, px: &mut [u32]) {
    let (Some(gt), Some((sx, sy))) = (general_table(m), view(m)) else { return };
    let (l, t, r, b) = VIEW_PX;
    let mut put = |x: i32, y: i32, c: u32| {
        if x > l && x < r && y > t && y < b && (x as usize) < w && (y as usize) < h {
            px[y as usize * w + x as usize] = c;
        }
    };
    let font = include_bytes!("../assets/font8x8.bin");
    let center = |tx: i32, ty: i32| (32 + 16 * (2 * tx - sx), 88 + 16 * (2 * ty - sy));
    let mut rect = |put: &mut dyn FnMut(i32, i32, u32), cx: i32, cy: i32, c: u32, inset: i32, thick: i32| {
        for d in 0..thick {
            let i = inset + d;
            for k in -16 + i..16 - i {
                put(cx + k, cy - 16 + i, c);
                put(cx + k, cy + 15 - i, c);
                put(cx - 16 + i, cy + k, c);
                put(cx + 15 - i, cy + k, c);
            }
        }
    };
    let (bx, by) = ((sx + 1) / 2, (sy + 1) / 2);
    for ty in by - 1..by + 13 {
        for tx in bx - 1..bx + 15 {
            let (cx, cy) = center(tx, ty);
            rect(&mut put, cx, cy, 0xffff00, 0, 1);
            for (i, ch) in format!("{tx},{ty}").bytes().enumerate() {
                for row in 0..8 {
                    let bits = font[ch as usize * 8 + row];
                    for col in 0..8 {
                        if bits & (0x80 >> col) != 0 {
                            let (x, y) = (cx - 14 + i as i32 * 6 + col, cy - 13 + row as i32);
                            put(x + 1, y + 1, 0x000000);
                            put(x, y, 0xffffff);
                        }
                    }
                }
            }
        }
    }
    let (cx, cy) = center(LUJAI.0 as i32, LUJAI.1 as i32);
    rect(&mut put, cx, cy, 0x00ffff, 2, 2);
    for u in units(m).unwrap_or_default().iter().filter(|u| u.troops > 0 && u.x != 0xff) {
        let (cx, cy) = center(u.x as i32, u.y as i32);
        rect(&mut put, cx, cy, if u.side == "我軍" { 0x00ff00 } else { 0xff0000 }, 4, 2);
    }
    let overlay = &m.ram[(gt + 0x31d21) as usize..(gt + 0x31d21 + 0x23c) as usize];
    if overlay.iter().any(|&v| v != 0) {
        let (x, y) = (m.ram[(gt + 0x83d1) as usize], m.ram[(gt + 0x83d2) as usize]);
        let (cx, cy) = center(x as i32, y as i32);
        rect(&mut put, cx, cy, 0xff00ff, 7, 2);
    }
}

pub fn units(m: &Mem) -> Option<Vec<Unit>> {
    let gt = general_table(m)?;
    let ut = gt + UNIT_OFF;
    let mut out = Vec::new();
    for i in 0..MAX_UNITS {
        let u = m.bytes(ut + i * UNIT_SIZE, UNIT_SIZE as usize);
        if u[1] == 0xff || u[0] as u32 != i {
            continue;
        }
        let gen = m.rd16(ut + i * UNIT_SIZE - 2) as u32;
        let g = m.bytes(gt + gen * GEN_SIZE, GEN_SIZE as usize);
        let raw = &g[1..12];
        let raw = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
        // +8 changes as a unit acts (0x30 fresh; rest in place → 0xB0, move then act
        // → 0x60), so the side comes from +9 (ours 0x07, enemies 0x02/0x03).
        let side = if u[9] & 0x04 != 0 { "我軍" } else { "敵軍" };
        out.push(Unit {
            general: gen as u16,
            side,
            acted: u[8] & 0xc0 != 0,
            raw: u[3..12].try_into().unwrap(),
            name: encoding_rs::BIG5.decode(raw).0.into_owned(),
            x: u[1],
            y: u[2],
            troops: u16::from_le_bytes([g[0x19], g[0x1a]]),
            level: g[0x1c],
            exp: g[0x1d],
            morale: u[0xa],
            class: g[0x1b],
            state: u[7],
            stats: [g[0x13], g[0x14], g[0x15]],
            items: g[0x1e..0x26].iter().copied().filter(|&b| b != 0xff).collect(),
        });
    }
    Some(out)
}

/// View scroll in half tiles.
pub fn view(m: &Mem) -> Option<(i32, i32)> {
    let gt = general_table(m)?;
    Some((m.rd16(gt + VIEW_OFF) as i32, m.rd16(gt + VIEW_OFF + 2) as i32))
}

/// Screen pixel of a map tile's center, if that tile is inside the map view.
pub fn tile_screen(m: &Mem, x: i32, y: i32) -> Option<(i32, i32)> {
    let (sx, sy) = view(m)?;
    let (px, py) = (32 + 16 * (2 * x - sx), 88 + 16 * (2 * y - sy));
    let (l, t, r, b) = VIEW_PX;
    (px > l && px < r && py > t && py < b).then_some((px, py))
}

/// One line per unit for the control port.
pub fn describe(m: &Mem) -> String {
    match units(m) {
        None => "ok no battle data (not in MAIN's game yet)".into(),
        Some(us) => {
            let (sx, sy) = view(m).unwrap_or((0, 0));
            let mut s = format!("ok units={} 視野左上=({},{}) 可見範圍 x {}..{} y {}..{}", us.len(), sx as f32 / 2.0, sy as f32 / 2.0,
                (sx + 1) / 2, (sx + 1) / 2 + 12, (sy + 1) / 2, (sy + 1) / 2 + 10);
            for u in us {
                let scr = match tile_screen(m, u.x as i32, u.y as i32) {
                    Some((px, py)) => format!(" 畫面=({px},{py})"),
                    None => " 畫面=不在視野".into(),
                };
                s.push_str(&format!(
                    "\n{}{} {} x={} y={} 兵力={} 等級={} 經驗={} 士氣={} 統率/武力/智力={}/{}/{} 兵種={} 狀態={:02x} 道具={}",
                    u.side, if u.acted { "(已行動)" } else { "" }, u.name, u.x, u.y, u.troops, u.level, u.exp, u.morale,
                    u.stats[0], u.stats[1], u.stats[2], u.class, u.state,
                    u.items.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(",")
                ));
                s.push_str(&scr);
                s.push_str(&format!(" raw={}", u.raw.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join("")));
            }
            s
        }
    }
}
